//! The MMIO bus and the PL011 UART. The PL011 is now a full-enough PrimeCell for
//! Linux's `amba-pl011` driver to bind and bring up `ttyAMA0`: it answers the
//! PrimeCell/peripheral ID reads the AMBA bus matches on, models the control and
//! interrupt registers, and has a receive FIFO fed by the host. That is what
//! turns earlycon-only output into a real interactive console.
//!
//! `MmioBus`/`MmioDevice` stay for future non-interrupting devices; the PL011 is
//! owned directly by the VM because the run loop introspects its interrupt line.

use std::collections::VecDeque;

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

/// A PL011 PrimeCell UART, enough for an interactive console: TX to a host sink,
/// an RX FIFO fed by `push_rx`, the control/mask/status registers the driver
/// drives, and the AMBA ID registers it probes. The single interrupt line is
/// level-triggered; the run loop reads `irq_level` and drives the GIC.
pub struct Pl011 {
    out: Box<dyn std::io::Write + Send>,
    rx: VecDeque<u8>,
    cr: u32,   // control
    imsc: u32, // interrupt mask set/clear (1 = enabled)
    ris: u32,  // raw interrupt status
    ibrd: u32,
    fbrd: u32,
    lcr_h: u32,
    ifls: u32,
    // Console-output trigger: when `marker` is non-empty, `marked` latches the
    // first time the guest's TX stream ends with it. The run loop uses this to
    // snapshot exactly when the guest announces readiness (deterministic, unlike
    // a wall-clock delay). `tx_tail` is the last `marker.len()` bytes written.
    marker: Vec<u8>,
    tx_tail: VecDeque<u8>,
    marked: bool,
}

impl Pl011 {
    // Register offsets.
    const DR: u64 = 0x000;
    const FR: u64 = 0x018;
    const IBRD: u64 = 0x024;
    const FBRD: u64 = 0x028;
    const LCR_H: u64 = 0x02c;
    const CR: u64 = 0x030;
    const IFLS: u64 = 0x034;
    const IMSC: u64 = 0x038;
    const RIS: u64 = 0x03c;
    const MIS: u64 = 0x040;
    const ICR: u64 = 0x044;

    // FR bits.
    const FR_RXFE: u64 = 1 << 4; // receive FIFO empty
    const FR_TXFE: u64 = 1 << 7; // transmit FIFO empty

    // Interrupt bits (RIS/MIS/IMSC/ICR): RX (4) and receive-timeout (6).
    const INT_RX: u32 = 1 << 4;
    const INT_RT: u32 = 1 << 6;

    // PrimeCell PL011 identification, low byte of each word. The AMBA bus reads
    // these to bind the driver; without them ttyAMA0 never registers.
    const ID: [(u64, u8); 8] = [
        (0xfe0, 0x11), (0xfe4, 0x10), (0xfe8, 0x14), (0xfec, 0x00), // PeriphID0..3
        (0xff0, 0x0d), (0xff4, 0xf0), (0xff8, 0x05), (0xffc, 0xb1), // PCellID0..3
    ];

    pub fn new(out: Box<dyn std::io::Write + Send>) -> Self {
        Self {
            out,
            rx: VecDeque::new(),
            cr: 0,
            imsc: 0,
            ris: 0,
            ibrd: 0,
            fbrd: 0,
            lcr_h: 0,
            ifls: 0,
            marker: Vec::new(),
            tx_tail: VecDeque::new(),
            marked: false,
        }
    }

    /// Arm a console-output trigger: `marked()` latches once the guest's TX
    /// stream ends with `marker`. Empty disables it.
    pub fn arm_marker(&mut self, marker: Vec<u8>) {
        self.marker = marker;
        self.tx_tail.clear();
        self.marked = false;
    }

    /// Whether the armed marker has appeared in the guest's console output.
    pub fn marked(&self) -> bool {
        self.marked
    }

    /// Push a received byte from the host into the RX FIFO and raise the raw RX
    /// interrupt. The run loop calls `irq_level` afterwards to update the GIC.
    pub fn push_rx(&mut self, byte: u8) {
        self.rx.push_back(byte);
        self.ris |= Self::INT_RX | Self::INT_RT;
    }

    /// The current interrupt line level: any unmasked status bit set.
    pub fn irq_level(&self) -> bool {
        self.ris & self.imsc != 0
    }

    /// The configuration registers, for a snapshot. The RX FIFO and raw status are
    /// transient (refilled by new input), but the control/mask/baud/line registers
    /// the guest's driver programmed must survive a restore — without `imsc` the RX
    /// interrupt stays masked and console input never wakes the guest.
    pub fn regs(&self) -> [u32; 6] {
        [self.cr, self.imsc, self.ibrd, self.fbrd, self.lcr_h, self.ifls]
    }

    /// Restore the configuration registers captured by [`regs`].
    pub fn set_regs(&mut self, r: [u32; 6]) {
        self.cr = r[0];
        self.imsc = r[1];
        self.ibrd = r[2];
        self.fbrd = r[3];
        self.lcr_h = r[4];
        self.ifls = r[5];
    }

    pub fn read(&mut self, offset: u64, _size: u8) -> u64 {
        if let Some(&(_, v)) = Self::ID.iter().find(|&&(off, _)| off == offset) {
            return v as u64;
        }
        match offset {
            Self::DR => {
                let byte = self.rx.pop_front().unwrap_or(0);
                if self.rx.is_empty() {
                    self.ris &= !(Self::INT_RX | Self::INT_RT);
                }
                byte as u64
            }
            // TX always ready/empty; RXFE set when the receive FIFO is empty.
            Self::FR => {
                let mut fr = Self::FR_TXFE;
                if self.rx.is_empty() {
                    fr |= Self::FR_RXFE;
                }
                fr
            }
            Self::RIS => self.ris as u64,
            Self::MIS => (self.ris & self.imsc) as u64,
            Self::IMSC => self.imsc as u64,
            Self::CR => self.cr as u64,
            Self::IBRD => self.ibrd as u64,
            Self::FBRD => self.fbrd as u64,
            Self::LCR_H => self.lcr_h as u64,
            Self::IFLS => self.ifls as u64,
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, _size: u8, value: u64) {
        let v32 = value as u32;
        match offset {
            Self::DR => {
                let byte = value as u8;
                let _ = self.out.write_all(&[byte]);
                let _ = self.out.flush();
                if !self.marker.is_empty() && !self.marked {
                    self.tx_tail.push_back(byte);
                    while self.tx_tail.len() > self.marker.len() {
                        self.tx_tail.pop_front();
                    }
                    if self.tx_tail.len() == self.marker.len()
                        && self.tx_tail.iter().eq(self.marker.iter())
                    {
                        self.marked = true;
                    }
                }
            }
            Self::IMSC => self.imsc = v32,
            Self::CR => self.cr = v32,
            Self::IBRD => self.ibrd = v32,
            Self::FBRD => self.fbrd = v32,
            Self::LCR_H => self.lcr_h = v32,
            Self::IFLS => self.ifls = v32,
            // Write-1-to-clear the raw interrupt status.
            Self::ICR => self.ris &= !v32,
            _ => {}
        }
    }
}
