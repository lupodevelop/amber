//! virtio-mmio (version 2, modern) transport plus the devices amber emulates.
//!
//! The transport is the register state machine and the split-virtqueue mechanics
//! — magic/version/feature negotiation, queue setup, walking the available ring
//! into descriptor buffers, and writing the used ring. A [`VirtioDevice`] plugs
//! in the device-specific parts: its device id, feature bits, config space, and
//! how to service one descriptor chain. Today: a read-only block device and an
//! entropy source. All single-threaded (the vcpu thread); the device reads and
//! writes guest memory through a [`GuestRam`] view.

use crate::memory::GuestRam;
use crate::{Error, Result};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

// virtio-mmio register offsets (modern).
const MAGIC: u64 = 0x000;
const VERSION: u64 = 0x004;
const DEVICE_ID: u64 = 0x008;
const VENDOR_ID: u64 = 0x00c;
const DEVICE_FEATURES: u64 = 0x010;
const DEVICE_FEATURES_SEL: u64 = 0x014;
const DRIVER_FEATURES: u64 = 0x020;
const DRIVER_FEATURES_SEL: u64 = 0x024;
const QUEUE_SEL: u64 = 0x030;
const QUEUE_NUM_MAX: u64 = 0x034;
const QUEUE_NUM: u64 = 0x038;
const QUEUE_READY: u64 = 0x044;
const QUEUE_NOTIFY: u64 = 0x050;
const INTERRUPT_STATUS: u64 = 0x060;
const INTERRUPT_ACK: u64 = 0x064;
const STATUS: u64 = 0x070;
const QUEUE_DESC_LOW: u64 = 0x080;
const QUEUE_DESC_HIGH: u64 = 0x084;
const QUEUE_DRIVER_LOW: u64 = 0x090;
const QUEUE_DRIVER_HIGH: u64 = 0x094;
const QUEUE_DEVICE_LOW: u64 = 0x0a0;
const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
const CONFIG_GENERATION: u64 = 0x0fc;
const CONFIG: u64 = 0x100;

const VIRTIO_MAGIC: u32 = 0x7472_6976; // "virt"
const VENDOR: u32 = 0x616d_6265; // "ambe"
const QUEUE_MAX: u32 = 256;
const SECTOR: u64 = 512;

// VIRTIO_F_VERSION_1 is feature bit 32 (bit 0 of the high word): required for a
// modern device. The transport advertises it for every device.
const FEATURE_VERSION_1_WORD: u32 = 1;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

/// Per-buffer length ceiling — a guest can't make a device allocate more. No real
/// blk/net/rng/balloon buffer comes near it.
const MAX_DESC_LEN: u32 = 16 << 20; // 16 MiB

/// One descriptor's buffer, gathered from a chain.
///
/// [`collect_chain`] guarantees `[addr, addr+len)` is within guest RAM and
/// `len <= MAX_DESC_LEN`, so a device may size an allocation from `len` safely.
pub struct Buf {
    pub addr: u64,
    pub len: u32,
    /// True if the device may write it (VIRTQ_DESC_F_WRITE).
    pub writable: bool,
}

/// The device-specific half of a virtio-mmio device. `Send` because the device
/// set lives behind a Mutex shared with the secondary-vcpu threads.
pub trait VirtioDevice: Send {
    fn device_id(&self) -> u32;
    /// Device-type feature bits for 32-bit selector word `sel` (0 = low).
    fn device_features(&self, _sel: u32) -> u32 {
        0
    }
    /// Config-space dword at byte offset `off` from the config region.
    fn config(&self, _off: u64) -> u32 {
        0
    }
    /// How many virtqueues this device has.
    fn num_queues(&self) -> usize {
        1
    }
    /// Whether the device's config space changed and a config-change interrupt is
    /// pending (e.g. the balloon target was moved from another thread).
    fn config_changed(&self) -> bool {
        false
    }
    /// Acknowledge the pending config-change interrupt.
    fn ack_config(&self) {}
    /// Service one descriptor chain on `queue`; return the number of bytes the
    /// device wrote into guest memory (the used-ring length).
    fn handle(&mut self, queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32;

    /// The queue this device fills asynchronously (host -> guest), if any. Notifies
    /// on it are not consumed in `handle` (they just post buffers); the run loop
    /// pumps it via `poll_rx` when data arrives. virtio-net's receive queue uses it.
    fn rx_queue(&self) -> Option<usize> {
        None
    }
    /// A device-driven queue whose buffers the guest posts for the device to fill
    /// only when an event occurs (virtio-vsock's event queue). Like `rx_queue`, a
    /// notify on it must NOT consume the posted buffers — doing so signals a bogus
    /// event, the guest reposts, and the two livelock. Returns the queue index, if any.
    fn event_queue(&self) -> Option<usize> {
        None
    }
    /// Next payload to deliver on `rx_queue` (already including any device header),
    /// or None. Polled by the run loop; must be non-blocking. `max_frame` is the
    /// writable capacity of the buffer the guest has posted: a stream device (vsock)
    /// must not return a frame larger than this, or it would be truncated and dropped.
    fn poll_rx(&mut self, _max_frame: usize) -> Option<Vec<u8>> {
        None
    }
    /// True while the device has asynchronous work pending and wants the run loop
    /// to poll it eagerly rather than wait out the idle park (e.g. the network
    /// backend awaiting host-side replies).
    fn wants_poll(&self) -> bool {
        false
    }
}

#[derive(Default, Clone, Copy)]
struct Queue {
    num: u32,
    ready: u32,
    desc: u64,
    avail: u64,
    used: u64,
    last_avail: u16,
}

/// The virtio-mmio transport wrapping one device and its virtqueues.
pub struct VirtioMmio {
    dev: Box<dyn VirtioDevice>,
    ram: Option<GuestRam>,
    device_features_sel: u32,
    status: u32,
    queue_sel: usize,
    queues: Vec<Queue>,
    interrupt_status: u32,
    /// An rx payload polled from the device but not yet injected because the guest
    /// had no rx buffer posted. Held (not dropped) so a reliable stream like vsock
    /// doesn't lose data when the guest's rx queue momentarily drains.
    pending_rx: Option<Vec<u8>>,
}

impl VirtioMmio {
    /// Capture host-side device state (status + per-queue ring addresses, ready
    /// flag, and consumed index) for a snapshot. The guest-side driver state lives
    /// in guest RAM; this is the part that lives here and would otherwise reset.
    pub fn capture(&self) -> crate::snapshot::VirtioDevState {
        crate::snapshot::VirtioDevState {
            status: self.status,
            interrupt_status: self.interrupt_status,
            queues: self
                .queues
                .iter()
                .map(|q| [q.num as u64, q.ready as u64, q.desc, q.avail, q.used, q.last_avail as u64])
                .collect(),
        }
    }

    /// Restore state captured by [`capture`], so a post-restore queue kick resumes
    /// from the right ring addresses and consumed index.
    pub fn restore(&mut self, s: &crate::snapshot::VirtioDevState) {
        self.status = s.status;
        self.interrupt_status = s.interrupt_status;
        for (q, v) in self.queues.iter_mut().zip(&s.queues) {
            q.num = (v[0] as u32).min(QUEUE_MAX);
            q.ready = v[1] as u32;
            q.desc = v[2];
            q.avail = v[3];
            q.used = v[4];
            q.last_avail = v[5] as u16;
        }
    }

    pub fn new(dev: Box<dyn VirtioDevice>) -> Self {
        let n = dev.num_queues().max(1);
        Self {
            dev,
            ram: None,
            device_features_sel: 0,
            status: 0,
            queue_sel: 0,
            queues: vec![Queue::default(); n],
            interrupt_status: 0,
            pending_rx: None,
        }
    }

    pub fn attach(&mut self, ram: GuestRam) {
        self.ram = Some(ram);
    }

    pub fn irq_level(&self) -> bool {
        self.interrupt_status != 0 || self.dev.config_changed()
    }

    pub fn read(&mut self, offset: u64, _size: u8) -> u64 {
        let q = self.queues.get(self.queue_sel).copied().unwrap_or_default();
        let v = match offset {
            MAGIC => VIRTIO_MAGIC,
            VERSION => 2,
            DEVICE_ID => self.dev.device_id(),
            VENDOR_ID => VENDOR,
            DEVICE_FEATURES => {
                let mut f = self.dev.device_features(self.device_features_sel);
                if self.device_features_sel == 1 {
                    f |= FEATURE_VERSION_1_WORD;
                }
                f
            }
            // A selected queue that exists has the max size; past the end, 0.
            QUEUE_NUM_MAX => {
                if self.queue_sel < self.queues.len() {
                    QUEUE_MAX
                } else {
                    0
                }
            }
            QUEUE_READY => q.ready,
            // bit 0 = used-buffer notification, bit 1 = config change.
            INTERRUPT_STATUS => {
                self.interrupt_status | if self.dev.config_changed() { 2 } else { 0 }
            }
            STATUS => self.status,
            CONFIG_GENERATION => 0,
            o if o >= CONFIG => self.dev.config(o - CONFIG),
            _ => 0,
        };
        v as u64
    }

    pub fn write(&mut self, offset: u64, _size: u8, value: u64) {
        let v = value as u32;
        let sel = self.queue_sel;
        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = v,
            DRIVER_FEATURES | DRIVER_FEATURES_SEL => {}
            QUEUE_SEL => self.queue_sel = v as usize,
            // The driver must not pick a ring larger than we advertise; clamp so the
            // ring-index math (`% num`) and the per-notify work stay bounded.
            QUEUE_NUM => self.with_queue(sel, |q| q.num = v.min(QUEUE_MAX)),
            QUEUE_READY => self.with_queue(sel, |q| q.ready = v),
            QUEUE_NOTIFY => self.process(v as usize),
            INTERRUPT_ACK => {
                self.interrupt_status &= !v;
                if v & 2 != 0 {
                    self.dev.ack_config();
                }
            }
            STATUS => {
                self.status = v;
                if v == 0 {
                    self.reset();
                }
            }
            QUEUE_DESC_LOW => self.with_queue(sel, |q| q.desc = (q.desc & !0xffff_ffff) | v as u64),
            QUEUE_DESC_HIGH => self.with_queue(sel, |q| q.desc = (q.desc & 0xffff_ffff) | ((v as u64) << 32)),
            QUEUE_DRIVER_LOW => self.with_queue(sel, |q| q.avail = (q.avail & !0xffff_ffff) | v as u64),
            QUEUE_DRIVER_HIGH => self.with_queue(sel, |q| q.avail = (q.avail & 0xffff_ffff) | ((v as u64) << 32)),
            QUEUE_DEVICE_LOW => self.with_queue(sel, |q| q.used = (q.used & !0xffff_ffff) | v as u64),
            QUEUE_DEVICE_HIGH => self.with_queue(sel, |q| q.used = (q.used & 0xffff_ffff) | ((v as u64) << 32)),
            _ => {}
        }
    }

    fn with_queue(&mut self, sel: usize, f: impl FnOnce(&mut Queue)) {
        if let Some(q) = self.queues.get_mut(sel) {
            f(q);
        }
    }

    fn reset(&mut self) {
        self.interrupt_status = 0;
        for q in &mut self.queues {
            *q = Queue::default();
        }
    }

    /// Whether the wrapped device has async work pending (see
    /// [`VirtioDevice::wants_poll`]).
    pub fn wants_poll(&self) -> bool {
        self.dev.wants_poll()
    }

    /// The virtio device-type id of the wrapped device (e.g. 1 = net).
    pub fn device_id(&self) -> u32 {
        self.dev.device_id()
    }

    /// Deliver host-originated frames into the device's receive queue, consuming
    /// one posted guest buffer per frame. Returns true if anything was delivered
    /// (the caller raises the interrupt). Frames with no buffer waiting are dropped.
    pub fn pump_rx(&mut self) -> bool {
        let Some(ram) = self.ram else { return false };
        let Some(rxq) = self.dev.rx_queue() else { return false };
        let mut any = false;
        // First re-try a payload held back last time (the guest had no buffer then).
        // Holding rather than dropping keeps a reliable stream (vsock) intact when
        // the guest's rx queue momentarily has no posted buffers.
        if let Some(data) = self.pending_rx.take() {
            let Some(q) = self.queues.get_mut(rxq) else { return false };
            if !inject_one(&ram, q, &data) {
                self.pending_rx = Some(data);
                return false;
            }
            any = true;
        }
        // Size each frame to the buffer the guest has actually posted: a frame
        // larger than the next rx buffer would be truncated by `inject_one` and
        // dropped by the guest as malformed. virtio-vsock guests post buffers
        // smaller than a page (skb overhead), so this is well under 4 KiB.
        while let Some(cap) = self.queues.get(rxq).and_then(|q| next_rx_capacity(&ram, q)) {
            let Some(data) = self.dev.poll_rx(cap) else { break };
            let Some(q) = self.queues.get_mut(rxq) else { break };
            if !inject_one(&ram, q, &data) {
                self.pending_rx = Some(data); // hold it; retry on the next pump
                break;
            }
            any = true;
        }
        if any {
            self.interrupt_status |= 1;
        }
        any
    }

    fn process(&mut self, qidx: usize) {
        let Some(ram) = self.ram else { return };
        // The receive and event queues are device-driven: a notify there just posts
        // buffers for the device to fill later (rx) or on an event (event queue).
        // Consuming them here would deliver empty buffers — for the event queue that
        // signals a bogus event and livelocks with the guest reposting. Skip both.
        if self.dev.rx_queue() == Some(qidx) || self.dev.event_queue() == Some(qidx) {
            return;
        }
        let Some(mut q) = self.queues.get(qidx).copied() else { return };
        if q.ready == 0 || q.num == 0 {
            return;
        }
        let qsz = q.num as u16;
        let avail_idx = ram.read_u16(q.avail.wrapping_add(2));

        let mut progressed = false;
        // A ready queue can have at most `qsz` buffers outstanding; bound the work
        // per notify by that, so a hostile available index can't spin the vcpu.
        let mut budget = qsz;
        while q.last_avail != avail_idx && budget > 0 {
            let slot = q.last_avail % qsz;
            let head = ram.read_u16(q.avail.wrapping_add(4 + 2 * slot as u64));
            let bufs = collect_chain(&ram, q.desc, head, qsz);
            let written = self.dev.handle(qidx, &ram, &bufs);
            push_used(&ram, q.used, head as u32, written, qsz);
            q.last_avail = q.last_avail.wrapping_add(1);
            budget -= 1;
            progressed = true;
        }
        self.queues[qidx].last_avail = q.last_avail;
        if progressed {
            self.interrupt_status |= 1; // used buffer notification
        }
    }
}

/// Walk a guest-controlled descriptor chain into buffers, bounding every step:
/// index `< qsz`, at most `qsz` descriptors (a `next` cycle can't loop), each
/// buffer clamped to `MAX_DESC_LEN`, out-of-RAM buffers downgraded to zero length,
/// and wrapping arithmetic so a crafted base can't panic.
fn collect_chain(ram: &GuestRam, desc: u64, head: u16, qsz: u16) -> Vec<Buf> {
    let mut bufs = Vec::new();
    let mut i = head;
    for _ in 0..qsz {
        if i >= qsz {
            break; // index outside the descriptor table: malformed chain.
        }
        let d = desc.wrapping_add(16 * i as u64);
        let addr = ram.read_u64(d);
        let len = ram.read_u32(d.wrapping_add(8)).min(MAX_DESC_LEN);
        let flags = ram.read_u16(d.wrapping_add(12));
        let next = ram.read_u16(d.wrapping_add(14));
        // Trust the length only for a buffer fully inside guest RAM.
        let len = if ram.in_range(addr, len as usize) { len } else { 0 };
        bufs.push(Buf {
            addr,
            len,
            writable: flags & VIRTQ_DESC_F_WRITE != 0,
        });
        if flags & VIRTQ_DESC_F_NEXT == 0 {
            break;
        }
        i = next;
    }
    bufs
}

fn push_used(ram: &GuestRam, used: u64, id: u32, len: u32, qsz: u16) {
    if qsz == 0 {
        return;
    }
    let idx = ram.read_u16(used.wrapping_add(2));
    let elem = used.wrapping_add(4 + 8 * (idx % qsz) as u64);
    ram.write_u32(elem, id);
    ram.write_u32(elem.wrapping_add(4), len);
    ram.write_u16(used.wrapping_add(2), idx.wrapping_add(1));
}

/// Total writable bytes in the next posted buffer of a receive queue, or None if
/// the guest hasn't posted one. Used to size a frame so it fits — an oversized
/// frame would be silently truncated by `inject_one` and dropped by the guest.
fn next_rx_capacity(ram: &GuestRam, q: &Queue) -> Option<usize> {
    if q.ready == 0 || q.num == 0 {
        return None;
    }
    let qsz = q.num as u16;
    let avail_idx = ram.read_u16(q.avail.wrapping_add(2));
    if q.last_avail == avail_idx {
        return None;
    }
    let slot = q.last_avail % qsz;
    let head = ram.read_u16(q.avail.wrapping_add(4 + 2 * slot as u64));
    let cap: usize = collect_chain(ram, q.desc, head, qsz)
        .iter()
        .filter(|b| b.writable)
        .map(|b| b.len as usize)
        .sum();
    Some(cap)
}

/// Write one host->guest payload into the next posted buffer of a receive queue
/// (spilling across a descriptor chain if needed) and publish it on the used ring.
/// Returns false if no buffer is available, so the caller can stop and drop.
fn inject_one(ram: &GuestRam, q: &mut Queue, data: &[u8]) -> bool {
    if q.ready == 0 || q.num == 0 {
        return false;
    }
    let qsz = q.num as u16;
    let avail_idx = ram.read_u16(q.avail.wrapping_add(2));
    if q.last_avail == avail_idx {
        return false; // no buffer posted by the guest
    }
    let slot = q.last_avail % qsz;
    let head = ram.read_u16(q.avail.wrapping_add(4 + 2 * slot as u64));
    let bufs = collect_chain(ram, q.desc, head, qsz);
    let mut off = 0usize;
    for b in bufs.iter().filter(|b| b.writable) {
        if off >= data.len() {
            break;
        }
        let n = (b.len as usize).min(data.len() - off);
        ram.write(b.addr, &data[off..off + n]);
        off += n;
    }
    push_used(ram, q.used, head as u32, off as u32, qsz);
    q.last_avail = q.last_avail.wrapping_add(1);
    true
}

/// virtio-blk. The squashfs root opens read-only (the guest layers a tmpfs
/// overlay for its writes); a data disk opens read-write, so IN, OUT and FLUSH
/// are all implemented and writes persist to the backing host file.
pub struct BlkDevice {
    disk: File,
    capacity_sectors: u64,
    writable: bool,
    /// I/O-rate cap (`AMBER_DISK_BPS`); None = unlimited.
    limit: Option<crate::limiter::TokenBucket>,
}

impl BlkDevice {
    const DEVICE_ID: u32 = 2;
    const T_IN: u32 = 0;
    const T_OUT: u32 = 1;
    const T_FLUSH: u32 = 4;
    const S_OK: u8 = 0;
    const S_IOERR: u8 = 1;
    const S_UNSUPP: u8 = 2;
    /// VIRTIO_BLK_F_RO (bit 5): tells the guest the device is read-only.
    const F_RO: u32 = 1 << 5;

    /// Open a read-only backing image (the squashfs root).
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, false)
    }

    /// Open a read-write data disk; OUT/FLUSH persist to `path`.
    pub fn open_writable(path: &Path) -> Result<Self> {
        Self::open_with(path, true)
    }

    fn open_with(path: &Path, writable: bool) -> Result<Self> {
        let disk = std::fs::OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)
            .map_err(|e| Error::Device(format!("open {}: {e}", path.display())))?;
        let bytes = disk
            .metadata()
            .map_err(|e| Error::Device(format!("stat disk: {e}")))?
            .len();
        Ok(Self {
            disk,
            capacity_sectors: bytes / SECTOR,
            writable,
            limit: crate::limiter::TokenBucket::from_env("AMBER_DISK_BPS"),
        })
    }

    fn read_disk(&self, offset: u64, buf: &mut [u8]) {
        match self.disk.read_at(buf, offset) {
            Ok(n) if n < buf.len() => buf[n..].fill(0),
            Ok(_) => {}
            Err(_) => buf.fill(0),
        }
    }
}

impl VirtioDevice for BlkDevice {
    fn device_id(&self) -> u32 {
        Self::DEVICE_ID
    }

    fn device_features(&self, sel: u32) -> u32 {
        if sel == 0 && !self.writable {
            Self::F_RO
        } else {
            0
        }
    }

    fn config(&self, off: u64) -> u32 {
        match off {
            0 => self.capacity_sectors as u32,
            4 => (self.capacity_sectors >> 32) as u32,
            _ => 0,
        }
    }

    /// Chain is [header(RO), data..., status(WO,1)]. IN fills the device-writable
    /// data buffers from disk; OUT drains the device-readable ones to disk; FLUSH
    /// fsyncs. The used-ring length is the bytes written back into guest RAM (the
    /// IN payload) plus the one status byte.
    fn handle(&mut self, _queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32 {
        if bufs.len() < 2 {
            return 0;
        }
        let req_type = ram.read_u32(bufs[0].addr);
        let sector = ram.read_u64(bufs[0].addr + 8);
        let status_idx = bufs.len() - 1;
        let data = &bufs[1..status_idx];
        let mut written = 0u32;

        // Rate cap: pay for the whole transfer up front. Blocking here is the
        // backpressure — the guest is inside the notify and sees a slow disk.
        let rate = |limit: &mut Option<crate::limiter::TokenBucket>, want_writable: bool| {
            if let Some(l) = limit {
                let total: u64 =
                    data.iter().filter(|b| b.writable == want_writable).map(|b| b.len as u64).sum();
                l.throttle(total);
            }
        };

        let status = match req_type {
            Self::T_IN => {
                rate(&mut self.limit, true);
                let mut offset = sector * SECTOR;
                for b in data.iter().filter(|b| b.writable) {
                    let mut d = vec![0u8; b.len as usize];
                    self.read_disk(offset, &mut d);
                    ram.write(b.addr, &d);
                    offset += b.len as u64;
                    written += b.len;
                }
                Self::S_OK
            }
            Self::T_OUT if self.writable => {
                rate(&mut self.limit, false);
                let mut offset = sector * SECTOR;
                let mut ok = true;
                for b in data.iter().filter(|b| !b.writable) {
                    let mut d = vec![0u8; b.len as usize];
                    ram.read(b.addr, &mut d);
                    if self.disk.write_all_at(&d, offset).is_err() {
                        ok = false;
                        break;
                    }
                    offset += b.len as u64;
                }
                if ok { Self::S_OK } else { Self::S_IOERR }
            }
            // Write to a read-only disk: refuse rather than silently drop.
            Self::T_OUT => Self::S_IOERR,
            Self::T_FLUSH => {
                if !self.writable || self.disk.sync_all().is_ok() {
                    Self::S_OK
                } else {
                    Self::S_IOERR
                }
            }
            _ => Self::S_UNSUPP,
        };
        ram.write(bufs[status_idx].addr, &[status]);
        written + 1
    }
}

/// virtio-entropy: fills the guest's request buffers with host randomness, so
/// the guest's crng has a continuous source. (Boot-time seeding is handled
/// separately by `/chosen/rng-seed`.)
pub struct RngDevice {
    src: File,
}

impl RngDevice {
    const DEVICE_ID: u32 = 4;

    pub fn open() -> Result<Self> {
        let src = File::open("/dev/urandom")
            .map_err(|e| Error::Device(format!("open /dev/urandom: {e}")))?;
        Ok(Self { src })
    }
}

impl VirtioDevice for RngDevice {
    fn device_id(&self) -> u32 {
        Self::DEVICE_ID
    }

    fn handle(&mut self, _queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32 {
        let mut written = 0;
        for b in bufs.iter().filter(|b| b.writable) {
            let mut data = vec![0u8; b.len as usize];
            if self.src.read_exact(&mut data).is_err() {
                continue;
            }
            ram.write(b.addr, &data);
            written += b.len;
        }
        written
    }
}

/// A handle to a balloon's target and config-change flag, shared with whatever
/// drives reclaim (the control thread). Setting the target and flag, then raising
/// the device's interrupt, makes the guest inflate toward the new target.
#[derive(Clone)]
pub struct BalloonHandle {
    pub target_pages: Arc<AtomicU64>,
    pub config_dirty: Arc<AtomicBool>,
}

/// virtio-balloon with both free-page reporting (passive reclaim) and inflation
/// (active reclaim under pressure). On the reporting queue the guest hands over
/// ranges of its free RAM; on the inflate queue, after the host raises the target
/// via [`BalloonHandle`], the guest hands over page-frame numbers it gave up.
/// Both paths `madvise` the backing host pages reusable, shrinking the VM's real
/// footprint — the RAM-coexistence levers from MEMORY.md.
pub struct BalloonDevice {
    target_pages: Arc<AtomicU64>,
    config_dirty: Arc<AtomicBool>,
}

impl BalloonDevice {
    const DEVICE_ID: u32 = 5;
    // Features (low word): free-page reporting (bit 5). Inflate/deflate are core.
    const F_REPORTING: u32 = 1 << 5;
    // With only F_REPORTING: inflate(0), deflate(1), reporting(2).
    const INFLATE_VQ: usize = 0;
    const REPORTING_VQ: usize = 2;
    // Balloon page-frame numbers are in 4 KiB units, regardless of guest page size.
    const BALLOON_PAGE: u64 = 4096;
    // macOS: mark pages reusable and immediately drop them from the resident set.
    const MADV_FREE_REUSABLE: i32 = 7;

    pub fn new(target_pages: Arc<AtomicU64>, config_dirty: Arc<AtomicBool>) -> Self {
        Self { target_pages, config_dirty }
    }

    pub fn handle(&self) -> BalloonHandle {
        BalloonHandle {
            target_pages: self.target_pages.clone(),
            config_dirty: self.config_dirty.clone(),
        }
    }

    fn madvise(ram: &GuestRam, gpa: u64, len: usize) {
        if let Some(p) = ram.host_ptr_at(gpa, len) {
            unsafe { libc::madvise(p as *mut libc::c_void, len, Self::MADV_FREE_REUSABLE) };
        }
    }
}

impl VirtioDevice for BalloonDevice {
    fn device_id(&self) -> u32 {
        Self::DEVICE_ID
    }

    fn device_features(&self, sel: u32) -> u32 {
        if sel == 0 {
            Self::F_REPORTING
        } else {
            0
        }
    }

    fn num_queues(&self) -> usize {
        3
    }

    /// Config space: `num_pages` (offset 0) is the inflate target the guest reads.
    fn config(&self, off: u64) -> u32 {
        match off {
            0 => self.target_pages.load(Ordering::Relaxed) as u32,
            _ => 0,
        }
    }

    fn config_changed(&self) -> bool {
        self.config_dirty.load(Ordering::Relaxed)
    }

    fn ack_config(&self) {
        self.config_dirty.store(false, Ordering::Relaxed);
    }

    fn handle(&mut self, queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32 {
        match queue {
            // Inflate: each buffer is an array of little-endian 4 KiB PFNs the
            // guest is handing back; drop the backing host pages.
            Self::INFLATE_VQ => {
                for b in bufs {
                    let n = b.len / 4;
                    for i in 0..n as u64 {
                        let pfn = ram.read_u32(b.addr.wrapping_add(4 * i)) as u64;
                        Self::madvise(ram, pfn.wrapping_mul(Self::BALLOON_PAGE), Self::BALLOON_PAGE as usize);
                    }
                }
            }
            // Reporting: each descriptor is a range of free guest RAM.
            Self::REPORTING_VQ => {
                for b in bufs {
                    Self::madvise(ram, b.addr, b.len as usize);
                }
            }
            // Deflate (and anything else): the guest is taking pages back; they
            // fault in on next write, nothing to do here.
            _ => {}
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::GuestMemory;


    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Arc, Mutex};

    const BASE: u64 = 0x4000_0000;
    const RAMSZ: usize = 0x4_0000; // 256 KiB
    const DESC: u64 = BASE + 0x1000;
    const AVAIL: u64 = BASE + 0x2000;
    const USED: u64 = BASE + 0x3000;
    const DATA: u64 = BASE + 0x4000;

    fn ram() -> GuestMemory {
        GuestMemory::new(BASE, RAMSZ).unwrap()
    }
    fn put_desc(r: &GuestRam, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let d = DESC + 16 * i as u64;
        r.write(d, &addr.to_le_bytes());
        r.write_u32(d + 8, len);
        r.write_u16(d + 12, flags);
        r.write_u16(d + 14, next);
    }
    fn avail_set(r: &GuestRam, slot: u16, head: u16, idx: u16) {
        r.write_u16(AVAIL + 4 + 2 * slot as u64, head);
        r.write_u16(AVAIL + 2, idx);
    }
    fn used_idx(r: &GuestRam) -> u16 {
        r.read_u16(USED + 2)
    }
    fn used_elem(r: &GuestRam, slot: u16) -> (u32, u32) {
        let e = USED + 4 + 8 * slot as u64;
        (r.read_u32(e), r.read_u32(e + 4))
    }
    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("amber-test-{}-{}", std::process::id(), name))
    }

    /// (addr, len, writable) of each buffer in one serviced chain.
    type Chain = Vec<(u64, u32, bool)>;

    /// A device that records every chain it is handed and reports a fixed length.
    /// (Arc/Mutex, not Rc/RefCell: VirtioDevice is Send.)
    #[derive(Default)]
    struct RecState {
        calls: Vec<(usize, Chain)>,
    }
    struct Rec {
        id: u32,
        nq: usize,
        st: Arc<Mutex<RecState>>,
        written: u32,
    }
    fn rec(id: u32, nq: usize, written: u32) -> (Box<Rec>, Arc<Mutex<RecState>>) {
        let st = Arc::new(Mutex::new(RecState::default()));
        (Box::new(Rec { id, nq, st: st.clone(), written }), st)
    }
    impl VirtioDevice for Rec {
        fn device_id(&self) -> u32 {
            self.id
        }
        fn num_queues(&self) -> usize {
            self.nq
        }
        fn handle(&mut self, queue: usize, _ram: &GuestRam, bufs: &[Buf]) -> u32 {
            let row = bufs.iter().map(|b| (b.addr, b.len, b.writable)).collect();
            self.st.lock().unwrap().calls.push((queue, row));
            self.written
        }
    }

    fn setup_queue(m: &mut VirtioMmio, sel: u32, num: u32) {
        m.write(QUEUE_SEL, 4, sel as u64);
        m.write(QUEUE_NUM, 4, num as u64);
        m.write(QUEUE_DESC_LOW, 4, DESC & 0xffff_ffff);
        m.write(QUEUE_DESC_HIGH, 4, DESC >> 32);
        m.write(QUEUE_DRIVER_LOW, 4, AVAIL & 0xffff_ffff);
        m.write(QUEUE_DRIVER_HIGH, 4, AVAIL >> 32);
        m.write(QUEUE_DEVICE_LOW, 4, USED & 0xffff_ffff);
        m.write(QUEUE_DEVICE_HIGH, 4, USED >> 32);
        m.write(QUEUE_READY, 4, 1);
    }

    // ---- collect_chain: every input is guest-controlled ----

    #[test]
    fn chain_single_descriptor() {
        let m = ram();
        let r = m.ram();
        put_desc(&r, 0, DATA, 100, 0, 0);
        let bufs = collect_chain(&r, DESC, 0, 8);
        assert_eq!(bufs.len(), 1);
        assert_eq!((bufs[0].addr, bufs[0].len, bufs[0].writable), (DATA, 100, false));
    }

    #[test]
    fn chain_follows_next_and_write_flag() {
        let m = ram();
        let r = m.ram();
        put_desc(&r, 0, DATA, 10, VIRTQ_DESC_F_NEXT, 3);
        put_desc(&r, 3, DATA + 10, 20, VIRTQ_DESC_F_WRITE, 0);
        let bufs = collect_chain(&r, DESC, 0, 8);
        assert_eq!(bufs.len(), 2);
        assert_eq!((bufs[1].addr, bufs[1].len, bufs[1].writable), (DATA + 10, 20, true));
    }

    #[test]
    fn chain_head_outside_table_is_empty() {
        let m = ram();
        let r = m.ram();
        assert!(collect_chain(&r, DESC, 8, 8).is_empty()); // head == qsz
        assert!(collect_chain(&r, DESC, 9, 8).is_empty()); // head > qsz
    }

    #[test]
    fn chain_next_outside_table_stops() {
        let m = ram();
        let r = m.ram();
        put_desc(&r, 0, DATA, 10, VIRTQ_DESC_F_NEXT, 50);
        assert_eq!(collect_chain(&r, DESC, 0, 8).len(), 1);
    }

    #[test]
    fn chain_cycle_is_bounded_by_queue_size() {
        let m = ram();
        let r = m.ram();
        put_desc(&r, 0, DATA, 1, VIRTQ_DESC_F_NEXT, 1);
        put_desc(&r, 1, DATA, 1, VIRTQ_DESC_F_NEXT, 0);
        // A 0<->1 cycle never terminates on its own; the qsz cap stops it.
        assert_eq!(collect_chain(&r, DESC, 0, 8).len(), 8);
    }

    #[test]
    fn chain_clamps_oversized_length() {
        let big = GuestMemory::new(BASE, 32 << 20).unwrap();
        let r = big.ram();
        let d = BASE; // descriptor table at base
        r.write(d, &(BASE + 0x1_0000).to_le_bytes());
        r.write_u32(d + 8, 30 << 20); // 30 MiB > MAX_DESC_LEN
        r.write_u16(d + 12, 0);
        r.write_u16(d + 14, 0);
        let bufs = collect_chain(&r, d, 0, 8);
        assert_eq!(bufs[0].len, MAX_DESC_LEN);
    }

    #[test]
    fn chain_out_of_range_buffer_is_zero_length() {
        let m = ram();
        let r = m.ram();
        put_desc(&r, 0, BASE + RAMSZ as u64, 16, VIRTQ_DESC_F_WRITE, 0);
        let bufs = collect_chain(&r, DESC, 0, 8);
        assert_eq!(bufs[0].len, 0); // address past RAM -> length downgraded
        assert!(bufs[0].writable); // flag still preserved
    }

    #[test]
    fn chain_wrapping_descriptor_base_does_not_panic() {
        let m = ram();
        let r = m.ram();
        let bufs = collect_chain(&r, u64::MAX - 3, 0, 4);
        assert_eq!(bufs.len(), 1);
        assert_eq!(bufs[0].len, 0);
    }

    // ---- process: notify-driven service ----

    #[test]
    fn process_delivers_chain_and_publishes_used() {
        let m = ram();
        let (dev, st) = rec(99, 1, 42);
        let mut mmio = VirtioMmio::new(dev);
        mmio.attach(m.ram());
        setup_queue(&mut mmio, 0, 8);
        let r = m.ram();
        put_desc(&r, 0, DATA, 64, VIRTQ_DESC_F_WRITE, 0);
        avail_set(&r, 0, 0, 1);
        mmio.write(QUEUE_NOTIFY, 4, 0);

        let calls = &st.lock().unwrap().calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, 0);
        assert_eq!(calls[0].1, vec![(DATA, 64, true)]);
        assert_eq!(used_idx(&r), 1);
        assert_eq!(used_elem(&r, 0), (0, 42)); // (head id, bytes written)
        assert!(mmio.irq_level());
    }

    #[test]
    fn process_work_is_bounded_by_queue_size() {
        let m = ram();
        let (dev, st) = rec(99, 1, 0);
        let mut mmio = VirtioMmio::new(dev);
        mmio.attach(m.ram());
        setup_queue(&mut mmio, 0, 4);
        let r = m.ram();
        for i in 0..4u16 {
            put_desc(&r, i, DATA, 8, 0, 0);
        }
        r.write_u16(AVAIL + 2, 60000); // hostile available index
        mmio.write(QUEUE_NOTIFY, 4, 0);
        assert_eq!(st.lock().unwrap().calls.len(), 4); // capped at qsz, not 60000
    }

    #[test]
    fn process_ignores_unready_or_empty_queue() {
        let m = ram();
        let (dev, st) = rec(9, 1, 0);
        let mut mmio = VirtioMmio::new(dev);
        mmio.attach(m.ram());
        setup_queue(&mut mmio, 0, 4);
        mmio.write(QUEUE_READY, 4, 0); // not ready
        let r = m.ram();
        put_desc(&r, 0, DATA, 8, 0, 0);
        avail_set(&r, 0, 0, 1);
        mmio.write(QUEUE_NOTIFY, 4, 0);
        assert!(st.lock().unwrap().calls.is_empty());
    }

    #[test]
    fn queue_num_is_clamped_to_max() {
        let (dev, _) = rec(9, 1, 0);
        let mut mmio = VirtioMmio::new(dev);
        mmio.write(QUEUE_SEL, 4, 0);
        mmio.write(QUEUE_NUM, 4, 100_000);
        assert_eq!(mmio.capture().queues[0][0], QUEUE_MAX as u64);
    }

    #[test]
    fn reset_clears_queue_state() {
        let (dev, _) = rec(9, 1, 0);
        let mut mmio = VirtioMmio::new(dev);
        setup_queue(&mut mmio, 0, 8);
        mmio.write(STATUS, 4, 0); // status 0 -> reset
        let q = mmio.capture().queues[0];
        assert_eq!(q, [0, 0, 0, 0, 0, 0]);
    }

    // ---- used/inject ring mechanics ----

    #[test]
    fn push_used_wraps_ring_index() {
        let m = ram();
        let r = m.ram();
        r.write_u16(USED + 2, 7); // idx 7, qsz 4 -> slot 3
        push_used(&r, USED, 5, 99, 4);
        assert_eq!(used_idx(&r), 8);
        assert_eq!(used_elem(&r, 3), (5, 99));
    }

    #[test]
    fn push_used_with_zero_queue_is_noop() {
        let m = ram();
        let r = m.ram();
        push_used(&r, USED, 1, 1, 0); // must not divide by zero
        assert_eq!(used_idx(&r), 0);
    }

    #[test]
    fn inject_one_fills_posted_buffer() {
        let m = ram();
        let r = m.ram();
        put_desc(&r, 0, DATA, 16, VIRTQ_DESC_F_WRITE, 0);
        avail_set(&r, 0, 0, 1);
        let mut q = Queue { num: 8, ready: 1, desc: DESC, avail: AVAIL, used: USED, last_avail: 0 };
        assert!(inject_one(&r, &mut q, &[0xaa, 0xbb, 0xcc]));
        let mut got = [0u8; 3];
        r.read(DATA, &mut got);
        assert_eq!(got, [0xaa, 0xbb, 0xcc]);
        assert_eq!(used_elem(&r, 0), (0, 3));
        assert_eq!(q.last_avail, 1);
    }

    #[test]
    fn inject_one_without_posted_buffer_drops() {
        let m = ram();
        let r = m.ram();
        let mut q = Queue { num: 8, ready: 1, desc: DESC, avail: AVAIL, used: USED, last_avail: 0 };
        assert!(!inject_one(&r, &mut q, &[1, 2, 3])); // avail idx == last_avail
    }

    // ---- virtio-blk (read-only) ----

    fn blk_chain(data_len: u32) -> Vec<Buf> {
        vec![
            Buf { addr: DATA, len: 16, writable: false },          // header
            Buf { addr: DATA + 16, len: data_len, writable: true }, // data
            Buf { addr: DATA + 16 + data_len as u64, len: 1, writable: true }, // status
        ]
    }

    #[test]
    fn blk_in_reads_from_disk_and_acks() {
        let path = tmp("blk.img");
        std::fs::write(&path, vec![0xABu8; 1024]).unwrap();
        let mut blk = BlkDevice::open(&path).unwrap();
        let m = ram();
        let r = m.ram();
        r.write_u32(DATA, BlkDevice::T_IN);
        r.write(DATA + 8, &0u64.to_le_bytes()); // sector 0
        let written = blk.handle(0, &r, &blk_chain(512));
        assert_eq!(written, 512 + 1);
        let mut data = [0u8; 512];
        r.read(DATA + 16, &mut data);
        assert!(data.iter().all(|&b| b == 0xAB));
        assert_eq!(r.read_u32(DATA + 16 + 512) & 0xff, 0); // VIRTIO_BLK_S_OK
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn blk_rate_cap_applies_backpressure() {
        let path = tmp("blk-rate.img");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let mut blk = BlkDevice::open(&path).unwrap();
        let mut bucket = crate::limiter::TokenBucket::new(20 * 1024); // 20 KiB/s
        assert!(bucket.try_take(20 * 1024)); // drain: 2 KiB ≈ 100 ms debt
        blk.limit = Some(bucket);
        let m = ram();
        let r = m.ram();
        r.write_u32(DATA, BlkDevice::T_IN);
        r.write(DATA + 8, &0u64.to_le_bytes());
        let t0 = std::time::Instant::now();
        let written = blk.handle(0, &r, &blk_chain(2048));
        assert!(t0.elapsed() >= std::time::Duration::from_millis(60));
        assert_eq!(written, 2048 + 1); // request completed in full
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn blk_reads_past_eof_as_zero() {
        let path = tmp("blk2.img");
        std::fs::write(&path, vec![0xABu8; 512]).unwrap();
        let mut blk = BlkDevice::open(&path).unwrap();
        let m = ram();
        let r = m.ram();
        r.write_u32(DATA, BlkDevice::T_IN);
        r.write(DATA + 8, &9999u64.to_le_bytes()); // sector well past EOF
        blk.handle(0, &r, &blk_chain(512));
        let mut data = [0xffu8; 512];
        r.read(DATA + 16, &mut data);
        assert!(data.iter().all(|&b| b == 0));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn blk_writable_persists_to_disk() {
        let path = tmp("blk-w.img");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let mut blk = BlkDevice::open_writable(&path).unwrap();
        let m = ram();
        let r = m.ram();
        r.write_u32(DATA, BlkDevice::T_OUT);
        r.write(DATA + 8, &1u64.to_le_bytes()); // sector 1
        let payload = [0xABu8; 512];
        r.write(DATA + 16, &payload);
        let bufs = vec![
            Buf { addr: DATA, len: 16, writable: false },        // header
            Buf { addr: DATA + 16, len: 512, writable: false },  // data: guest -> device
            Buf { addr: DATA + 16 + 512, len: 1, writable: true }, // status
        ];
        assert_eq!(blk.handle(0, &r, &bufs), 1); // OUT writes nothing back, just status
        assert_eq!(r.read_u32(DATA + 16 + 512) & 0xff, BlkDevice::S_OK as u32);
        drop(blk);
        let disk = std::fs::read(&path).unwrap();
        assert_eq!(&disk[512..1024], &payload, "sector 1 persisted to the host file");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn blk_readonly_rejects_writes() {
        let path = tmp("blk-ro.img");
        std::fs::write(&path, vec![0u8; 1024]).unwrap();
        let mut blk = BlkDevice::open(&path).unwrap(); // read-only
        let m = ram();
        let r = m.ram();
        r.write_u32(DATA, BlkDevice::T_OUT);
        r.write(DATA + 8, &0u64.to_le_bytes());
        r.write(DATA + 16, &[0xABu8; 512]);
        let bufs = vec![
            Buf { addr: DATA, len: 16, writable: false },
            Buf { addr: DATA + 16, len: 512, writable: false },
            Buf { addr: DATA + 16 + 512, len: 1, writable: true },
        ];
        blk.handle(0, &r, &bufs);
        assert_eq!(r.read_u32(DATA + 16 + 512) & 0xff, BlkDevice::S_IOERR as u32);
        assert!(std::fs::read(&path).unwrap().iter().all(|&b| b == 0), "file untouched");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn blk_advertises_ro_feature_only_when_readonly() {
        let path = tmp("blk-feat.img");
        std::fs::write(&path, vec![0u8; 512]).unwrap();
        let ro = BlkDevice::open(&path).unwrap();
        let rw = BlkDevice::open_writable(&path).unwrap();
        assert_eq!(ro.device_features(0) & BlkDevice::F_RO, BlkDevice::F_RO);
        assert_eq!(rw.device_features(0) & BlkDevice::F_RO, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn blk_short_chain_returns_zero() {
        let path = tmp("blk3.img");
        std::fs::write(&path, vec![0u8; 512]).unwrap();
        let mut blk = BlkDevice::open(&path).unwrap();
        let m = ram();
        let r = m.ram();
        let bufs = vec![Buf { addr: DATA, len: 16, writable: false }];
        assert_eq!(blk.handle(0, &r, &bufs), 0);
        std::fs::remove_file(&path).ok();
    }

    // ---- virtio-rng ----

    #[test]
    fn rng_fills_only_writable_buffers() {
        let mut rng = RngDevice::open().unwrap();
        let m = ram();
        let r = m.ram();
        let bufs = vec![
            Buf { addr: DATA, len: 32, writable: true },
            Buf { addr: DATA + 64, len: 16, writable: false }, // read-only: skipped
        ];
        assert_eq!(rng.handle(0, &r, &bufs), 32);
    }

    // ---- virtio-balloon ----

    #[test]
    fn balloon_inflate_and_report_are_bounded_and_safe() {
        let bal_target = Arc::new(AtomicU64::new(0));
        let bal_dirty = Arc::new(AtomicBool::new(false));
        let mut bal = BalloonDevice::new(bal_target, bal_dirty);
        let m = ram();
        let r = m.ram();
        r.write_u32(DATA, 5); // pfn 5
        r.write_u32(DATA + 4, 6); // pfn 6
        // `handle` collides with the inherent BalloonHandle accessor; call the trait.
        let inflate = <BalloonDevice as VirtioDevice>::handle(&mut bal, BalloonDevice::INFLATE_VQ, &r, &[Buf { addr: DATA, len: 8, writable: false }]);
        assert_eq!(inflate, 0);
        let report = <BalloonDevice as VirtioDevice>::handle(&mut bal, BalloonDevice::REPORTING_VQ, &r, &[Buf { addr: DATA, len: 4096, writable: false }]);
        assert_eq!(report, 0);
    }

    // ---- snapshot capture / restore ----

    #[test]
    fn capture_restore_roundtrips() {
        let (dev, _) = rec(2, 2, 0);
        let mut a = VirtioMmio::new(dev);
        setup_queue(&mut a, 0, 8);
        a.write(STATUS, 4, 0xb);
        let snap = a.capture();

        let (dev2, _) = rec(2, 2, 0);
        let mut b = VirtioMmio::new(dev2);
        b.restore(&snap);
        let snap2 = b.capture();
        assert_eq!(snap.status, snap2.status);
        assert_eq!(snap.interrupt_status, snap2.interrupt_status);
        assert_eq!(snap.queues, snap2.queues);
    }

    #[test]
    fn restore_clamps_queue_num() {
        let s = crate::snapshot::VirtioDevState {
            status: 1,
            interrupt_status: 0,
            queues: vec![[100_000, 1, 0, 0, 0, 0]],
        };
        let (dev, _) = rec(2, 1, 0);
        let mut b = VirtioMmio::new(dev);
        b.restore(&s);
        assert_eq!(b.capture().queues[0][0], QUEUE_MAX as u64);
    }
}
