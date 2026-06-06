//! A minimal virtio-mmio (version 2, modern) block device, read-only.
//!
//! Enough to back a guest root filesystem from a host image file: the MMIO
//! transport registers, one split virtqueue, and the block IN (read) path. The
//! base is read-only (a squashfs blob); the guest layers a tmpfs overlay on top
//! for writes, so OUT/flush are not needed. The single interrupt line is
//! level-triggered off `InterruptStatus`, driven into the GIC by the run loop.

use crate::memory::GuestRam;
use crate::{Error, Result};
use std::fs::File;
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
const CONFIG: u64 = 0x100; // device-specific config; blk capacity (u64 sectors)

const VIRTIO_MAGIC: u32 = 0x7472_6976; // "virt"
const DEVICE_ID_BLOCK: u32 = 2;
const VENDOR: u32 = 0x616d_6265; // "ambe"
const QUEUE_MAX: u32 = 256;
const SECTOR: u64 = 512;

// VIRTIO_F_VERSION_1 is bit 32: required for a modern device. Advertised in the
// high feature word (DeviceFeaturesSel == 1).
const FEATURES_HI: u32 = 1; // bit 0 of word 1 == feature bit 32

// Split virtqueue descriptor flags.
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// virtio-blk request types.
const VIRTIO_BLK_T_IN: u32 = 0;

pub struct VirtioBlk {
    disk: File,
    capacity_sectors: u64,
    ram: Option<GuestRam>,

    device_features_sel: u32,
    status: u32,
    queue_num: u32,
    queue_ready: u32,
    desc: u64,
    avail: u64,
    used: u64,
    last_avail: u16,
    interrupt_status: u32,
}

impl VirtioBlk {
    pub fn open(path: &Path) -> Result<Self> {
        let disk = File::open(path).map_err(|e| Error::Device(format!("open {}: {e}", path.display())))?;
        let bytes = disk
            .metadata()
            .map_err(|e| Error::Device(format!("stat disk: {e}")))?
            .len();
        Ok(Self {
            disk,
            capacity_sectors: bytes / SECTOR,
            ram: None,
            device_features_sel: 0,
            status: 0,
            queue_num: 0,
            queue_ready: 0,
            desc: 0,
            avail: 0,
            used: 0,
            last_avail: 0,
            interrupt_status: 0,
        })
    }

    /// Attach the guest RAM view the device reads rings and buffers through.
    pub fn attach(&mut self, ram: GuestRam) {
        self.ram = Some(ram);
    }

    /// Level-triggered interrupt line: asserted while a used-buffer notification
    /// is outstanding, cleared when the driver acks it.
    pub fn irq_level(&self) -> bool {
        self.interrupt_status != 0
    }

    pub fn read(&mut self, offset: u64, _size: u8) -> u64 {
        let v = match offset {
            MAGIC => VIRTIO_MAGIC,
            VERSION => 2,
            DEVICE_ID => DEVICE_ID_BLOCK,
            VENDOR_ID => VENDOR,
            DEVICE_FEATURES => {
                if self.device_features_sel == 1 {
                    FEATURES_HI
                } else {
                    0
                }
            }
            QUEUE_NUM_MAX => QUEUE_MAX,
            QUEUE_READY => self.queue_ready,
            INTERRUPT_STATUS => self.interrupt_status,
            STATUS => self.status,
            CONFIG_GENERATION => 0,
            CONFIG => self.capacity_sectors as u32, // low 32 bits
            o if o == CONFIG + 4 => (self.capacity_sectors >> 32) as u32, // high
            _ => 0,
        };
        v as u64
    }

    pub fn write(&mut self, offset: u64, _size: u8, value: u64) {
        let v = value as u32;
        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = v,
            DRIVER_FEATURES | DRIVER_FEATURES_SEL => {} // accepted, not used
            QUEUE_SEL => {} // only queue 0 exists
            QUEUE_NUM => self.queue_num = v,
            QUEUE_READY => self.queue_ready = v,
            QUEUE_NOTIFY => self.process_queue(),
            INTERRUPT_ACK => self.interrupt_status &= !v,
            STATUS => {
                self.status = v;
                if v == 0 {
                    self.reset();
                }
            }
            QUEUE_DESC_LOW => self.desc = (self.desc & !0xffff_ffff) | v as u64,
            QUEUE_DESC_HIGH => self.desc = (self.desc & 0xffff_ffff) | ((v as u64) << 32),
            QUEUE_DRIVER_LOW => self.avail = (self.avail & !0xffff_ffff) | v as u64,
            QUEUE_DRIVER_HIGH => self.avail = (self.avail & 0xffff_ffff) | ((v as u64) << 32),
            QUEUE_DEVICE_LOW => self.used = (self.used & !0xffff_ffff) | v as u64,
            QUEUE_DEVICE_HIGH => self.used = (self.used & 0xffff_ffff) | ((v as u64) << 32),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.queue_ready = 0;
        self.last_avail = 0;
        self.interrupt_status = 0;
        self.desc = 0;
        self.avail = 0;
        self.used = 0;
    }

    /// Process every newly-available request, writing results into guest memory
    /// and the used ring, then raise the used-buffer interrupt.
    fn process_queue(&mut self) {
        let Some(ram) = self.ram else { return };
        if self.queue_ready == 0 || self.queue_num == 0 {
            return;
        }
        let qsz = self.queue_num as u16;
        // avail ring: flags(u16), idx(u16), ring[u16; qsz].
        let avail_idx = ram.read_u16(self.avail + 2);

        let mut progressed = false;
        while self.last_avail != avail_idx {
            let slot = self.last_avail % qsz;
            let head = ram.read_u16(self.avail + 4 + 2 * slot as u64);
            let written = self.handle_request(&ram, head, qsz);
            self.push_used(&ram, head as u32, written, qsz);
            self.last_avail = self.last_avail.wrapping_add(1);
            progressed = true;
        }

        if progressed {
            self.interrupt_status |= 1; // used buffer notification
        }
    }

    /// Walk a descriptor chain: [header(RO), data...(WO), status(WO,1)]. For an
    /// IN request, fill the data buffers from the disk and write status = OK.
    /// Returns the number of bytes the device wrote (data + status).
    fn handle_request(&self, ram: &GuestRam, head: u16, qsz: u16) -> u32 {
        // Collect the chain.
        let mut bufs: Vec<(u64, u32, bool)> = Vec::new(); // (addr, len, device_writable)
        let mut i = head;
        loop {
            let d = self.desc + 16 * i as u64;
            let addr = ram.read_u64(d);
            let len = ram.read_u32(d + 8);
            let flags = ram.read_u16(d + 12);
            let next = ram.read_u16(d + 14);
            bufs.push((addr, len, flags & VIRTQ_DESC_F_WRITE != 0));
            if flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            i = next;
            if bufs.len() > qsz as usize {
                break; // malformed chain guard
            }
        }
        if bufs.len() < 2 {
            return 0;
        }

        // Header is the first descriptor: type(u32), reserved(u32), sector(u64).
        let (haddr, _, _) = bufs[0];
        let req_type = ram.read_u32(haddr);
        let sector = ram.read_u64(haddr + 8);

        let status_idx = bufs.len() - 1;
        let mut written: u32 = 0;

        if req_type == VIRTIO_BLK_T_IN {
            let mut offset = sector * SECTOR;
            for &(addr, len, writable) in &bufs[1..status_idx] {
                if !writable {
                    continue;
                }
                let mut buf = vec![0u8; len as usize];
                self.read_disk(offset, &mut buf);
                ram.write(addr, &buf);
                offset += len as u64;
                written += len;
            }
        }
        // Status byte: 0 == VIRTIO_BLK_S_OK (also our answer for an ignored OUT).
        let (saddr, _, _) = bufs[status_idx];
        ram.write(saddr, &[0u8]);
        written + 1
    }

    fn push_used(&self, ram: &GuestRam, id: u32, len: u32, qsz: u16) {
        // used ring: flags(u16), idx(u16), ring[{id:u32, len:u32}; qsz].
        let idx = ram.read_u16(self.used + 2);
        let slot = (idx % qsz) as u64;
        let elem = self.used + 4 + 8 * slot;
        ram.write_u32(elem, id);
        ram.write_u32(elem + 4, len);
        ram.write_u16(self.used + 2, idx.wrapping_add(1));
    }

    /// Read `buf.len()` bytes from the disk at `offset`, zero-filling past EOF.
    fn read_disk(&self, offset: u64, buf: &mut [u8]) {
        match self.disk.read_at(buf, offset) {
            Ok(n) if n < buf.len() => buf[n..].fill(0),
            Ok(_) => {}
            Err(_) => buf.fill(0),
        }
    }
}
