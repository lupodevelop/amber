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

/// One descriptor's buffer, as gathered from a chain.
pub struct Buf {
    pub addr: u64,
    pub len: u32,
    /// True if the device may write it (VIRTQ_DESC_F_WRITE).
    pub writable: bool,
}

/// The device-specific half of a virtio-mmio device.
pub trait VirtioDevice {
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
    /// Next payload to deliver on `rx_queue` (already including any device header),
    /// or None. Polled by the run loop; must be non-blocking.
    fn poll_rx(&mut self) -> Option<Vec<u8>> {
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
            q.num = v[0] as u32;
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
            QUEUE_NUM => self.with_queue(sel, |q| q.num = v),
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
        while let Some(data) = self.dev.poll_rx() {
            let Some(q) = self.queues.get_mut(rxq) else { break };
            if !inject_one(&ram, q, &data) {
                break; // no buffer posted: drop and stop (the frame is lost)
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
        // The receive queue is filled asynchronously by `pump_rx`, not on notify;
        // a notify there just posts buffers, so do not consume them here.
        if self.dev.rx_queue() == Some(qidx) {
            return;
        }
        let Some(mut q) = self.queues.get(qidx).copied() else { return };
        if q.ready == 0 || q.num == 0 {
            return;
        }
        let qsz = q.num as u16;
        let avail_idx = ram.read_u16(q.avail + 2);

        let mut progressed = false;
        while q.last_avail != avail_idx {
            let slot = q.last_avail % qsz;
            let head = ram.read_u16(q.avail + 4 + 2 * slot as u64);
            let bufs = collect_chain(&ram, q.desc, head, qsz);
            let written = self.dev.handle(qidx, &ram, &bufs);
            push_used(&ram, q.used, head as u32, written, qsz);
            q.last_avail = q.last_avail.wrapping_add(1);
            progressed = true;
        }
        self.queues[qidx].last_avail = q.last_avail;
        if progressed {
            self.interrupt_status |= 1; // used buffer notification
        }
    }
}

fn collect_chain(ram: &GuestRam, desc: u64, head: u16, qsz: u16) -> Vec<Buf> {
    let mut bufs = Vec::new();
    let mut i = head;
    loop {
        let d = desc + 16 * i as u64;
        let addr = ram.read_u64(d);
        let len = ram.read_u32(d + 8);
        let flags = ram.read_u16(d + 12);
        let next = ram.read_u16(d + 14);
        bufs.push(Buf {
            addr,
            len,
            writable: flags & VIRTQ_DESC_F_WRITE != 0,
        });
        if flags & VIRTQ_DESC_F_NEXT == 0 || bufs.len() > qsz as usize {
            break;
        }
        i = next;
    }
    bufs
}

fn push_used(ram: &GuestRam, used: u64, id: u32, len: u32, qsz: u16) {
    let idx = ram.read_u16(used + 2);
    let elem = used + 4 + 8 * (idx % qsz) as u64;
    ram.write_u32(elem, id);
    ram.write_u32(elem + 4, len);
    ram.write_u16(used + 2, idx.wrapping_add(1));
}

/// Write one host->guest payload into the next posted buffer of a receive queue
/// (spilling across a descriptor chain if needed) and publish it on the used ring.
/// Returns false if no buffer is available, so the caller can stop and drop.
fn inject_one(ram: &GuestRam, q: &mut Queue, data: &[u8]) -> bool {
    if q.ready == 0 || q.num == 0 {
        return false;
    }
    let qsz = q.num as u16;
    let avail_idx = ram.read_u16(q.avail + 2);
    if q.last_avail == avail_idx {
        return false; // no buffer posted by the guest
    }
    let slot = q.last_avail % qsz;
    let head = ram.read_u16(q.avail + 4 + 2 * slot as u64);
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

/// virtio-blk, read-only. The base is a read-only image (squashfs); the guest
/// layers a tmpfs overlay for writes, so only IN (read) is implemented.
pub struct BlkDevice {
    disk: File,
    capacity_sectors: u64,
}

impl BlkDevice {
    const DEVICE_ID: u32 = 2;
    const T_IN: u32 = 0;

    pub fn open(path: &Path) -> Result<Self> {
        let disk =
            File::open(path).map_err(|e| Error::Device(format!("open {}: {e}", path.display())))?;
        let bytes = disk
            .metadata()
            .map_err(|e| Error::Device(format!("stat disk: {e}")))?
            .len();
        Ok(Self {
            disk,
            capacity_sectors: bytes / SECTOR,
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

    fn config(&self, off: u64) -> u32 {
        match off {
            0 => self.capacity_sectors as u32,
            4 => (self.capacity_sectors >> 32) as u32,
            _ => 0,
        }
    }

    /// Chain is [header(RO), data...(WO), status(WO,1)]. Fill data from disk for
    /// an IN request and write status = OK.
    fn handle(&mut self, _queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32 {
        if bufs.len() < 2 {
            return 0;
        }
        let req_type = ram.read_u32(bufs[0].addr);
        let sector = ram.read_u64(bufs[0].addr + 8);
        let status_idx = bufs.len() - 1;
        let mut written = 0;

        if req_type == Self::T_IN {
            let mut offset = sector * SECTOR;
            for b in &bufs[1..status_idx] {
                if !b.writable {
                    continue;
                }
                let mut data = vec![0u8; b.len as usize];
                self.read_disk(offset, &mut data);
                ram.write(b.addr, &data);
                offset += b.len as u64;
                written += b.len;
            }
        }
        ram.write(bufs[status_idx].addr, &[0u8]); // VIRTIO_BLK_S_OK
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
                        let pfn = ram.read_u32(b.addr + 4 * i) as u64;
                        Self::madvise(ram, pfn * Self::BALLOON_PAGE, Self::BALLOON_PAGE as usize);
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
