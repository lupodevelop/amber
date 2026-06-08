//! A software GICv2 — distributor + CPU interface, single CPU, MMIO only.
//!
//! Why GICv2 and not GICv3: GICv3's CPU interface is the `ICC_*` **system
//! registers**, and HVF does not trap guest sysreg access to the GIC interface
//! (that gap is exactly why Apple added the in-kernel vGIC in macOS 15). GICv2's
//! CPU interface is **memory-mapped** (GICC), so every access is a stage-2 data
//! abort we can emulate. With no in-kernel vGIC, HVF also stops handling WFI
//! internally and delivers it as an exception — which is what lets us own the
//! idle/timer path and make snapshot/restore's periodic timer work.
//!
//! Interrupts are injected into the vcpu through `hv_vcpu_set_pending_interrupt`
//! (the IRQ line); the guest then reads `GICC_IAR` to learn the INTID. This file
//! is pure state — no HVF calls — so it unit-tests without a hypervisor.

/// INTIDs modelled: SGIs 0–15, PPIs 16–31 (the virtual timer is PPI 27), and
/// SPIs 32+. 256 is well above the handful of SPIs amber wires.
const NUM_INTID: usize = 256;
const SPURIOUS: u32 = 1023;
/// GICv2 priorities are 8-bit, lower = higher priority. Reset/idle running
/// priority is 0xff (nothing active, everything can preempt).
const IDLE_PRIO: u8 = 0xff;

/// Distributor register offsets (within the GICD MMIO window).
mod gicd {
    pub const CTLR: u64 = 0x000;
    pub const TYPER: u64 = 0x004;
    pub const IIDR: u64 = 0x008;
    pub const IGROUPR: u64 = 0x080;
    pub const ISENABLER: u64 = 0x100;
    pub const ICENABLER: u64 = 0x180;
    pub const ISPENDR: u64 = 0x200;
    pub const ICPENDR: u64 = 0x280;
    pub const ISACTIVER: u64 = 0x300;
    pub const ICACTIVER: u64 = 0x380;
    pub const IPRIORITYR: u64 = 0x400;
    pub const ITARGETSR: u64 = 0x800;
    pub const ICFGR: u64 = 0xc00;
    pub const SGIR: u64 = 0xf00;
}

/// CPU-interface register offsets (within the GICC MMIO window).
mod gicc {
    pub const CTLR: u64 = 0x00;
    pub const PMR: u64 = 0x04;
    pub const BPR: u64 = 0x08;
    pub const IAR: u64 = 0x0c;
    pub const EOIR: u64 = 0x10;
    pub const RPR: u64 = 0x14;
    pub const HPPIR: u64 = 0x18;
    pub const IIDR: u64 = 0xfc;
}

pub struct GicV2 {
    /// GICD_CTLR enable (group forwarding from the distributor).
    dist_enabled: bool,
    /// GICC_CTLR enable (forwarding to this CPU).
    cpu_enabled: bool,
    /// GICC_PMR: only interrupts strictly higher priority (lower value) pass.
    pmr: u8,

    enabled: [bool; NUM_INTID],
    /// Latched pending for edge-triggered INTIDs (and SGIs).
    pending: [bool; NUM_INTID],
    active: [bool; NUM_INTID],
    priority: [u8; NUM_INTID],
    /// Raw input level for level-triggered INTIDs (devices, timer). For these the
    /// effective pending state follows the line, not the latched bit.
    level: [bool; NUM_INTID],
    /// ICFGR bit: true = edge-triggered, false = level-triggered. SGIs/PPIs have
    /// architecture-fixed config but tracking a flag uniformly is simplest.
    edge: [bool; NUM_INTID],

    /// Running-priority stack: priorities of interrupts acknowledged but not yet
    /// EOI'd. The top bounds what can preempt. Empty → IDLE_PRIO.
    running: Vec<u8>,
}

impl Default for GicV2 {
    fn default() -> Self {
        let mut edge = [false; NUM_INTID];
        // SGIs (0–15) are edge by nature.
        for e in edge.iter_mut().take(16) {
            *e = true;
        }
        GicV2 {
            dist_enabled: false,
            cpu_enabled: false,
            pmr: 0,
            enabled: [false; NUM_INTID],
            pending: [false; NUM_INTID],
            active: [false; NUM_INTID],
            priority: [0; NUM_INTID],
            level: [false; NUM_INTID],
            edge,
            running: Vec::new(),
        }
    }
}

impl GicV2 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Effective pending: latched bit for edge, live line for level.
    fn is_pending(&self, i: usize) -> bool {
        if self.edge[i] {
            self.pending[i]
        } else {
            self.level[i]
        }
    }

    fn running_prio(&self) -> u8 {
        self.running.last().copied().unwrap_or(IDLE_PRIO)
    }

    /// The highest-priority deliverable INTID (pending, enabled, unmasked by PMR
    /// and by any running interrupt, not already active), or None. Lower priority
    /// value wins; ties break by lower INTID.
    fn highest_pending(&self) -> Option<(u32, u8)> {
        if !self.dist_enabled {
            return None;
        }
        let cap = self.running_prio();
        let mut best: Option<(u32, u8)> = None;
        for i in 0..NUM_INTID {
            if !self.enabled[i] || self.active[i] || !self.is_pending(i) {
                continue;
            }
            let p = self.priority[i];
            // PMR and running-priority both gate strictly (lower value passes).
            if p >= self.pmr || p >= cap {
                continue;
            }
            if best.map_or(true, |(_, bp)| p < bp) {
                best = Some((i as u32, p));
            }
        }
        best
    }

    /// Is an interrupt deliverable to the CPU right now? Used to decide whether to
    /// raise the vcpu IRQ line before entering the guest.
    pub fn irq_pending(&self) -> bool {
        self.cpu_enabled && self.highest_pending().is_some()
    }

    /// Drive a level-triggered line (devices: PL011, virtio) by absolute INTID.
    pub fn set_level(&mut self, intid: u32, high: bool) {
        let i = intid as usize;
        if i < NUM_INTID {
            self.level[i] = high;
            self.edge[i] = false;
        }
    }

    // --- distributor MMIO ---------------------------------------------------

    pub fn dist_read(&self, off: u64, size: u8) -> u64 {
        let v = self.dist_read32(off & !0x3);
        extract(v as u64, off, size)
    }

    fn dist_read32(&self, off: u64) -> u32 {
        match off {
            gicd::CTLR => self.dist_enabled as u32,
            // ITLinesNumber: (NUM_INTID/32)-1 in bits[4:0]; CPUNumber 0 → 1 cpu.
            gicd::TYPER => ((NUM_INTID / 32 - 1) as u32) & 0x1f,
            gicd::IIDR => 0x0200_043b, // arm implementer, GICv2-ish
            o if range(o, gicd::IGROUPR, 32) => 0, // all group 0
            o if range(o, gicd::ISENABLER, 32) || range(o, gicd::ICENABLER, 32) => {
                let start = if o < gicd::ICENABLER { gicd::ISENABLER } else { gicd::ICENABLER };
                self.bitset_read(&self.enabled, base_index(o, start))
            }
            o if range(o, gicd::ISPENDR, 32) || range(o, gicd::ICPENDR, 32) => {
                let start = if o < gicd::ICPENDR { gicd::ISPENDR } else { gicd::ICPENDR };
                let base = base_index(o, start);
                let mut v = 0u32;
                for b in 0..32 {
                    if self.is_pending(base + b) {
                        v |= 1 << b;
                    }
                }
                v
            }
            o if range(o, gicd::ISACTIVER, 32) || range(o, gicd::ICACTIVER, 32) => {
                let start = if o < gicd::ICACTIVER { gicd::ISACTIVER } else { gicd::ICACTIVER };
                self.bitset_read(&self.active, base_index(o, start))
            }
            o if range(o, gicd::IPRIORITYR, 64) => {
                let base = ((o - gicd::IPRIORITYR) & !0x3) as usize;
                let mut v = 0u32;
                for b in 0..4 {
                    if base + b < NUM_INTID {
                        v |= (self.priority[base + b] as u32) << (b * 8);
                    }
                }
                v
            }
            // ITARGETSR: single CPU → always target cpu0 (0x01 per byte) for SPIs.
            o if range(o, gicd::ITARGETSR, 64) => 0x0101_0101,
            o if range(o, gicd::ICFGR, 16) => {
                let base = base_index_cfg(o);
                let mut v = 0u32;
                for b in 0..16 {
                    if base + b < NUM_INTID && self.edge[base + b] {
                        v |= 0b10 << (b * 2);
                    }
                }
                v
            }
            _ => 0,
        }
    }

    pub fn dist_write(&mut self, off: u64, size: u8, val: u64) {
        let off32 = off & !0x3;
        // Byte-granular writes matter for IPRIORITYR/ITARGETSR; handle those
        // directly, everything else is naturally 32-bit.
        if range(off, gicd::IPRIORITYR, 64) {
            let idx = (off - gicd::IPRIORITYR) as usize;
            for k in 0..size as usize {
                if idx + k < NUM_INTID {
                    self.priority[idx + k] = (val >> (k * 8)) as u8;
                }
            }
            return;
        }
        let v = val as u32;
        match off32 {
            gicd::CTLR => self.dist_enabled = v & 1 != 0,
            o if range(o, gicd::ISENABLER, 32) => self.bits_set(&mut |g, i| g.enabled[i] = true, o, gicd::ISENABLER, v),
            o if range(o, gicd::ICENABLER, 32) => self.bits_set(&mut |g, i| g.enabled[i] = false, o, gicd::ICENABLER, v),
            o if range(o, gicd::ISPENDR, 32) => self.bits_set(&mut |g, i| g.pending[i] = true, o, gicd::ISPENDR, v),
            o if range(o, gicd::ICPENDR, 32) => self.bits_set(&mut |g, i| g.pending[i] = false, o, gicd::ICPENDR, v),
            o if range(o, gicd::ISACTIVER, 32) => self.bits_set(&mut |g, i| g.active[i] = true, o, gicd::ISACTIVER, v),
            o if range(o, gicd::ICACTIVER, 32) => self.bits_set(&mut |g, i| g.active[i] = false, o, gicd::ICACTIVER, v),
            o if range(o, gicd::ICFGR, 16) => {
                let base = base_index_cfg(o);
                for b in 0..16 {
                    let i = base + b;
                    if i >= 16 && i < NUM_INTID {
                        // bit[1] of each 2-bit field selects edge; PPIs (16–31)
                        // are kept as the guest configures.
                        self.edge[i] = (v >> (b * 2 + 1)) & 1 != 0;
                    }
                }
            }
            gicd::SGIR => {
                // Software-generated interrupt to self: make the target SGI pending.
                let sgi = (v & 0xf) as usize;
                self.pending[sgi] = true;
            }
            _ => {}
        }
    }

    // --- CPU interface MMIO -------------------------------------------------

    /// IAR has side effects (acknowledge), so reads take `&mut self`.
    pub fn cpu_read(&mut self, off: u64, size: u8) -> u64 {
        let v = match off & !0x3 {
            gicc::CTLR => self.cpu_enabled as u32,
            gicc::PMR => self.pmr as u32,
            gicc::BPR => 0,
            gicc::IAR => self.acknowledge(),
            gicc::RPR => self.running_prio() as u32,
            gicc::HPPIR => self.highest_pending().map(|(i, _)| i).unwrap_or(SPURIOUS),
            gicc::IIDR => 0x0002_043b,
            _ => 0,
        };
        extract(v as u64, off, size)
    }

    pub fn cpu_write(&mut self, off: u64, _size: u8, val: u64) {
        let v = val as u32;
        match off & !0x3 {
            gicc::CTLR => self.cpu_enabled = v & 1 != 0,
            gicc::PMR => self.pmr = v as u8,
            gicc::EOIR => self.end_of_interrupt(v & 0x3ff),
            _ => {}
        }
    }

    /// GICC_IAR: return the highest-priority deliverable INTID, mark it active,
    /// consume its edge-pending latch, and push its priority on the running stack.
    fn acknowledge(&mut self) -> u32 {
        match self.highest_pending() {
            None => SPURIOUS,
            Some((intid, prio)) => {
                let i = intid as usize;
                self.active[i] = true;
                if self.edge[i] {
                    self.pending[i] = false;
                }
                self.running.push(prio);
                intid
            }
        }
    }

    /// GICC_EOIR: deactivate the INTID and pop one running-priority level.
    fn end_of_interrupt(&mut self, intid: u32) {
        let i = intid as usize;
        if i < NUM_INTID {
            self.active[i] = false;
        }
        self.running.pop();
    }

    // --- helpers ------------------------------------------------------------

    fn bitset_read(&self, set: &[bool; NUM_INTID], base: usize) -> u32 {
        let mut v = 0u32;
        for b in 0..32 {
            if base + b < NUM_INTID && set[base + b] {
                v |= 1 << b;
            }
        }
        v
    }

    fn bits_set(&mut self, f: &mut dyn FnMut(&mut Self, usize), off: u64, start: u64, val: u32) {
        let base = base_index(off, start);
        for b in 0..32 {
            if val & (1 << b) != 0 && base + b < NUM_INTID {
                // SGIs cannot be enable/disabled via these in our simple model but
                // setting their bits is harmless.
                f(self, base + b);
            }
        }
    }
}

/// Is `off` within the `count`-register block beginning at `start` (each register
/// 4 bytes)?
fn range(off: u64, start: u64, count: u64) -> bool {
    off >= start && off < start + count * 4
}

/// First INTID covered by the 32-bit register at `off` in a 1-bit-per-INTID block.
fn base_index(off: u64, start: u64) -> usize {
    (((off - start) & !0x3) * 8) as usize
}

/// First INTID covered by the 32-bit register at `off` in ICFGR (2 bits/INTID).
fn base_index_cfg(off: u64) -> usize {
    (((off - gicd::ICFGR) & !0x3) * 4) as usize
}

/// Extract the `size`-byte slice the access wants from a 32-bit register value.
fn extract(reg: u64, off: u64, size: u8) -> u64 {
    let shift = (off & 0x3) * 8;
    let mask = match size {
        1 => 0xff,
        2 => 0xffff,
        _ => 0xffff_ffff,
    };
    (reg >> shift) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPI: u32 = 32 + 1; // a device SPI (PL011-shaped)

    fn enabled_gic() -> GicV2 {
        let mut g = GicV2::new();
        // Enable distributor + CPU interface, open the priority mask.
        g.dist_write(gicd::CTLR, 4, 1);
        g.cpu_write(gicc::CTLR, 4, 1);
        g.cpu_write(gicc::PMR, 4, 0xff);
        g
    }

    #[test]
    fn enable_pend_ack_eoi_cycle() {
        let mut g = enabled_gic();
        // priority mid-range, enable the SPI.
        g.dist_write(gicd::IPRIORITYR + SPI as u64, 1, 0xa0);
        g.dist_write(gicd::ISENABLER + (SPI / 32) as u64 * 4, 4, 1 << (SPI % 32));

        assert!(!g.irq_pending(), "no line raised yet");
        g.set_level(SPI, true);
        assert!(g.irq_pending(), "deliverable once the line is high");

        // Acknowledge: IAR returns the INTID and marks it active.
        let iar = g.cpu_read(gicc::IAR, 4) as u32;
        assert_eq!(iar, SPI);
        assert!(!g.irq_pending(), "active interrupt masks itself until EOI");

        // Device drops its line, then guest EOIs.
        g.set_level(SPI, false);
        g.cpu_write(gicc::EOIR, 4, SPI as u64);
        assert!(!g.irq_pending());
        // A second IAR with nothing pending returns spurious.
        assert_eq!(g.cpu_read(gicc::IAR, 4) as u32, SPURIOUS);
    }

    #[test]
    fn pmr_masks_low_priority() {
        let mut g = enabled_gic();
        g.dist_write(gicd::IPRIORITYR + SPI as u64, 1, 0x80);
        g.dist_write(gicd::ISENABLER + (SPI / 32) as u64 * 4, 4, 1 << (SPI % 32));
        g.set_level(SPI, true);
        // PMR only lets priorities < PMR through.
        g.cpu_write(gicc::PMR, 4, 0x80);
        assert!(!g.irq_pending(), "priority == PMR is masked");
        g.cpu_write(gicc::PMR, 4, 0x81);
        assert!(g.irq_pending(), "priority < PMR passes");
    }

    #[test]
    fn edge_timer_ppi_clears_on_ack() {
        let mut g = enabled_gic();
        let ppi = 27u32; // virtual timer
        g.edge[ppi as usize] = true;
        g.dist_write(gicd::IPRIORITYR + ppi as u64, 1, 0x80);
        g.dist_write(gicd::ISENABLER, 4, 1 << ppi); // PPI 27 in the first ISENABLER
        g.dist_write(gicd::ISPENDR, 4, 1 << ppi); // make it pending via GICD_ISPENDR
        assert!(g.irq_pending());
        assert_eq!(g.cpu_read(gicc::IAR, 4) as u32, ppi);
        // Edge latch consumed by the acknowledge.
        g.cpu_write(gicc::EOIR, 4, ppi as u64);
        assert!(!g.irq_pending(), "edge pending does not re-fire without a new event");
    }

    #[test]
    fn priority_orders_two_pending() {
        let mut g = enabled_gic();
        let (a, b) = (32u32, 40u32);
        g.dist_write(gicd::IPRIORITYR + a as u64, 1, 0x90);
        g.dist_write(gicd::IPRIORITYR + b as u64, 1, 0x50); // higher priority
        g.dist_write(gicd::ISENABLER + 4, 4, (1 << (a % 32)) | (1 << (b % 32)));
        g.set_level(a, true);
        g.set_level(b, true);
        // The lower priority value (b) is delivered first.
        assert_eq!(g.cpu_read(gicc::IAR, 4) as u32, b);
    }
}
