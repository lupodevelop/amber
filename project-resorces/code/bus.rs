//! The MMIO bus and the one device M0 needs: a minimal PL011 UART so the kernel
//! can print over earlycon. A device handles reads and writes at an offset within
//! its window. New devices (virtio-mmio blk and vsock, the GIC) register here in
//! later milestones; the run loop never changes.

use crate::layout;

pub trait MmioDevice: Send {
    fn read(&mut self, offset: u64, size: u8) -> u64;
    fn write(&mut self, offset: u64, size: u8, value: u64);
}

struct Region {
    base: u64,
    size: u64,
    dev: Box<dyn MmioDevice>,
}

#[derive(Default)]
pub struct MmioBus {
    regions: Vec<Region>,
}

impl MmioBus {
    pub fn register(&mut self, base: u64, size: u64, dev: Box<dyn MmioDevice>) {
        self.regions.push(Region { base, size, dev });
    }

    pub fn read(&mut self, ipa: u64, size: u8) -> u64 {
        match self.find(ipa) {
            Some(r) => r.dev.read(ipa - r.base, size),
            None => {
                log::trace!("mmio read to unbacked {ipa:#x}");
                0
            }
        }
    }

    pub fn write(&mut self, ipa: u64, size: u8, value: u64) {
        match self.find(ipa) {
            Some(r) => r.dev.write(ipa - r.base, size, value),
            None => log::trace!("mmio write {value:#x} to unbacked {ipa:#x}"),
        }
    }

    fn find(&mut self, ipa: u64) -> Option<&mut Region> {
        self.regions
            .iter_mut()
            .find(|r| ipa >= r.base && ipa < r.base + r.size)
    }
}

/// Minimal PL011. Enough for earlycon TX: writes to DR go to stdout, FR always
/// reports "ready, not full". Everything else reads as zero. Real RX, interrupts,
/// and baud config come later; the kernel's early console does not need them.
pub struct Pl011 {
    out: Box<dyn std::io::Write + Send>,
}

impl Pl011 {
    const DR: u64 = 0x00; // data register
    const FR: u64 = 0x18; // flag register

    pub fn new(out: Box<dyn std::io::Write + Send>) -> Self {
        Self { out }
    }

    pub fn register_on(bus: &mut MmioBus, out: Box<dyn std::io::Write + Send>) {
        bus.register(layout::PL011_BASE, layout::PL011_SIZE, Box::new(Pl011::new(out)));
    }
}

impl MmioDevice for Pl011 {
    fn read(&mut self, offset: u64, _size: u8) -> u64 {
        match offset {
            // FR: clear TXFF (bit 5) and set TXFE (bit 7) -> always ready, empty.
            Self::FR => 1 << 7,
            _ => 0,
        }
    }

    fn write(&mut self, offset: u64, _size: u8, value: u64) {
        if offset == Self::DR {
            let byte = [value as u8];
            let _ = self.out.write_all(&byte);
            let _ = self.out.flush();
        }
    }
}
