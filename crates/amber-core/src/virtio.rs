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
    /// Service one descriptor chain on `queue`; return the number of bytes the
    /// device wrote into guest memory (the used-ring length).
    fn handle(&mut self, queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32;
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
        self.interrupt_status != 0
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
            INTERRUPT_STATUS => self.interrupt_status,
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
            INTERRUPT_ACK => self.interrupt_status &= !v,
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

    fn process(&mut self, qidx: usize) {
        let Some(ram) = self.ram else { return };
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

/// virtio-balloon, free-page-reporting only. The guest proactively hands the host
/// ranges of its free RAM on the reporting queue; we `madvise` the host pages
/// reusable so the VM's real footprint shrinks back toward what the guest
/// actually uses — the cheapest RAM-coexistence lever (MEMORY.md). We never
/// inflate (num_pages stays 0), so the guest keeps all its RAM; only genuinely
/// free pages are dropped from the host.
pub struct BalloonDevice;

impl BalloonDevice {
    const DEVICE_ID: u32 = 5;
    // Features (low word): only free-page reporting (bit 5).
    const F_REPORTING: u32 = 1 << 5;
    // With only F_REPORTING the queues are inflate(0), deflate(1), reporting(2).
    const REPORTING_VQ: usize = 2;
    // macOS: mark pages reusable and immediately drop them from the resident set.
    const MADV_FREE_REUSABLE: i32 = 7;
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

    /// On the reporting queue each descriptor is a range of free guest RAM; hand
    /// the backing host pages back to the OS. Inflate/deflate are unused (we set
    /// no target), so they just complete.
    fn handle(&mut self, queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32 {
        if queue == Self::REPORTING_VQ {
            for b in bufs {
                let len = b.len as usize;
                if let Some(p) = ram.host_ptr_at(b.addr, len) {
                    unsafe { libc::madvise(p as *mut libc::c_void, len, Self::MADV_FREE_REUSABLE) };
                }
            }
        }
        0
    }
}
