//! A software GICv2 — distributor + CPU interface, MMIO only, multi-CPU capable.
//!
//! Why GICv2 and not GICv3: GICv3's CPU interface is the `ICC_*` **system
//! registers**, and HVF does not trap guest sysreg access to the GIC interface
//! (that gap is exactly why Apple added the in-kernel vGIC in macOS 15). GICv2's
//! CPU interface is **memory-mapped** (GICC), so every access is a stage-2 data
//! abort we can emulate. With no in-kernel vGIC, HVF also stops handling WFI
//! internally and delivers it as an exception — which is what lets us own the
//! idle/timer path and make snapshot/restore's periodic timer work.
//!
//! Multi-CPU model (the multi-vcpu groundwork): INTIDs 0–31 (SGIs + PPIs) and the
//! whole CPU interface are banked per CPU, SPIs are shared and routed by
//! ITARGETSR, and GICD_SGIR posts SGIs (IPIs) to target CPUs. Single-CPU use is
//! the `cpus=1` special case and behaves exactly as before.
//!
//! Interrupts are injected into each vcpu through `hv_vcpu_set_pending_interrupt`
//! (the IRQ line); the guest then reads `GICC_IAR` to learn the INTID. This file
//! is pure state — no HVF calls — so it unit-tests without a hypervisor.

/// INTIDs modelled: SGIs 0–15, PPIs 16–31 (the virtual timer is PPI 27), and
/// SPIs 32+. 256 is well above the handful of SPIs amber wires.
const NUM_INTID: usize = 256;
/// SGIs + PPIs: banked per CPU. SPIs (32+) are shared.
const PRIVATE: usize = 32;
const SPURIOUS: u32 = 1023;
/// GICv2 priorities are 8-bit, lower = higher priority. Reset/idle running
/// priority is 0xff (nothing active, everything can preempt).
const IDLE_PRIO: u8 = 0xff;
/// GICv2 architectural ceiling (the SGIR target list is 8 bits wide).
pub const MAX_CPUS: usize = 8;

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

/// Per-CPU (banked) state: the CPU interface plus the private INTIDs 0–31.
#[derive(Clone)]
struct CpuBank {
    /// GICC_CTLR enable (forwarding to this CPU).
    cpu_enabled: bool,
    /// GICC_PMR: only interrupts strictly higher priority (lower value) pass.
    pmr: u8,
    /// Running-priority stack: priorities of interrupts acknowledged but not yet
    /// EOI'd. The top bounds what can preempt. Empty → IDLE_PRIO.
    running: Vec<u8>,

    enabled: [bool; PRIVATE],
    /// Latched pending for edge-triggered private INTIDs (and SGIs).
    pending: [bool; PRIVATE],
    active: [bool; PRIVATE],
    priority: [u8; PRIVATE],
    /// Raw input level for level-triggered private INTIDs (the per-CPU timer).
    level: [bool; PRIVATE],
    /// ICFGR bit: true = edge-triggered. SGIs are architecturally edge.
    edge: [bool; PRIVATE],
    /// Source CPU of the last poster of each SGI (reported in IAR bits 12:10).
    sgi_src: [u8; 16],
}

impl Default for CpuBank {
    fn default() -> Self {
        let mut edge = [false; PRIVATE];
        for e in edge.iter_mut().take(16) {
            *e = true; // SGIs are edge by nature
        }
        CpuBank {
            cpu_enabled: false,
            pmr: 0,
            running: Vec::new(),
            enabled: [false; PRIVATE],
            pending: [false; PRIVATE],
            active: [false; PRIVATE],
            priority: [0; PRIVATE],
            level: [false; PRIVATE],
            edge,
            sgi_src: [0; 16],
        }
    }
}

pub struct GicV2 {
    /// GICD_CTLR enable (group forwarding from the distributor).
    dist_enabled: bool,
    cpus: Vec<CpuBank>,

    // Shared (SPI) state; indexes < PRIVATE are unused (live in the banks).
    enabled: [bool; NUM_INTID],
    pending: [bool; NUM_INTID],
    active: [bool; NUM_INTID],
    priority: [u8; NUM_INTID],
    level: [bool; NUM_INTID],
    edge: [bool; NUM_INTID],
    /// ITARGETSR per SPI: a bitmask of target CPUs. Reset: CPU 0.
    targets: [u8; NUM_INTID],
}

impl Default for GicV2 {
    fn default() -> Self {
        Self::with_cpus(1)
    }
}

impl GicV2 {
    #[allow(dead_code)] // single-cpu convenience (tests; with_cpus is the real entry)
    pub fn new() -> Self {
        Self::default()
    }

    /// A GIC serving `n` CPUs (1..=MAX_CPUS). Today amber runs one vcpu; the
    /// multi-vcpu milestone passes its count here and everything else holds.
    pub fn with_cpus(n: usize) -> Self {
        let n = n.clamp(1, MAX_CPUS);
        GicV2 {
            dist_enabled: false,
            cpus: vec![CpuBank::default(); n],
            enabled: [false; NUM_INTID],
            pending: [false; NUM_INTID],
            active: [false; NUM_INTID],
            priority: [0; NUM_INTID],
            level: [false; NUM_INTID],
            edge: [false; NUM_INTID],
            targets: [0x01; NUM_INTID],
        }
    }

    /// (Consumed by the multi-vcpu run loop; only tests call it until then.)
    #[allow(dead_code)]
    pub fn num_cpus(&self) -> usize {
        self.cpus.len()
    }

    // --- per-INTID accessors that pick the bank or the shared block ----------

    fn enabled_at(&self, cpu: usize, i: usize) -> bool {
        if i < PRIVATE { self.cpus[cpu].enabled[i] } else { self.enabled[i] }
    }
    fn active_at(&self, cpu: usize, i: usize) -> bool {
        if i < PRIVATE { self.cpus[cpu].active[i] } else { self.active[i] }
    }
    fn priority_at(&self, cpu: usize, i: usize) -> u8 {
        if i < PRIVATE { self.cpus[cpu].priority[i] } else { self.priority[i] }
    }
    fn edge_at(&self, cpu: usize, i: usize) -> bool {
        if i < PRIVATE { self.cpus[cpu].edge[i] } else { self.edge[i] }
    }

    /// Effective pending on `cpu`: latched bit for edge, live line for level;
    /// SPIs additionally require this CPU in their target mask.
    fn is_pending(&self, cpu: usize, i: usize) -> bool {
        if i < PRIVATE {
            let b = &self.cpus[cpu];
            if b.edge[i] { b.pending[i] } else { b.level[i] }
        } else {
            if self.targets[i] & (1 << cpu) == 0 {
                return false;
            }
            if self.edge[i] { self.pending[i] } else { self.level[i] }
        }
    }

    fn running_prio(&self, cpu: usize) -> u8 {
        self.cpus[cpu].running.last().copied().unwrap_or(IDLE_PRIO)
    }

    /// The highest-priority INTID deliverable to `cpu`, or None. Lower priority
    /// value wins; ties break by lower INTID.
    fn highest_pending(&self, cpu: usize) -> Option<(u32, u8)> {
        if !self.dist_enabled {
            return None;
        }
        let cap = self.running_prio(cpu);
        let pmr = self.cpus[cpu].pmr;
        let mut best: Option<(u32, u8)> = None;
        for i in 0..NUM_INTID {
            if !self.enabled_at(cpu, i) || self.active_at(cpu, i) || !self.is_pending(cpu, i) {
                continue;
            }
            let p = self.priority_at(cpu, i);
            // PMR and running-priority both gate strictly (lower value passes).
            if p >= pmr || p >= cap {
                continue;
            }
            if best.is_none_or(|(_, bp)| p < bp) {
                best = Some((i as u32, p));
            }
        }
        best
    }

    /// Is an interrupt deliverable to `cpu` right now? Used to decide whether to
    /// raise that vcpu's IRQ line before entering the guest.
    pub fn irq_pending(&self, cpu: usize) -> bool {
        self.cpus[cpu].cpu_enabled && self.highest_pending(cpu).is_some()
    }

    /// Bitmask of CPUs with a deliverable interrupt — the future multi-vcpu run
    /// loop polls this after GIC MMIO to kick the right vcpu threads.
    #[allow(dead_code)]
    pub fn pending_cpus(&self) -> u32 {
        (0..self.cpus.len()).filter(|&c| self.irq_pending(c)).fold(0, |m, c| m | 1 << c)
    }

    /// Drive a level-triggered line by absolute INTID. SPIs (the devices) are
    /// shared and ignore `cpu`; private INTIDs (the per-CPU timer, PPI 27) land
    /// in that CPU's bank.
    pub fn set_level(&mut self, cpu: usize, intid: u32, high: bool) {
        let i = intid as usize;
        if i < PRIVATE {
            self.cpus[cpu].level[i] = high;
            self.cpus[cpu].edge[i] = false;
        } else if i < NUM_INTID {
            self.level[i] = high;
            self.edge[i] = false;
        }
    }

    // --- snapshot -----------------------------------------------------------

    /// Blob magic + version. Old (pre-multi-CPU) blobs started with a 0/1
    /// `dist_enabled` byte, so 0xA2 is unambiguous.
    const BLOB_MAGIC: u8 = 0xa2;
    const BLOB_V2: u8 = 2;

    /// Serialize the full controller state to a flat blob (snapshot). The software
    /// GIC has no opaque host object, so its state round-trips as plain bytes — and
    /// because we deliver interrupts ourselves, the restored timer just works.
    pub fn capture(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + self.cpus.len() * (PRIVATE * 6 + 32) + NUM_INTID * 7);
        v.push(Self::BLOB_MAGIC);
        v.push(Self::BLOB_V2);
        v.push(self.cpus.len() as u8);
        v.push(self.dist_enabled as u8);
        for b in &self.cpus {
            v.push(b.cpu_enabled as u8);
            v.push(b.pmr);
            for a in [&b.enabled, &b.pending, &b.active, &b.level, &b.edge] {
                v.extend(a.iter().map(|&x| x as u8));
            }
            v.extend_from_slice(&b.priority);
            v.extend_from_slice(&b.sgi_src);
            v.push(b.running.len().min(255) as u8);
            v.extend_from_slice(&b.running[..b.running.len().min(255)]);
        }
        for a in [&self.enabled, &self.pending, &self.active, &self.level, &self.edge] {
            v.extend(a.iter().map(|&x| x as u8));
        }
        v.extend_from_slice(&self.priority);
        v.extend_from_slice(&self.targets);
        v
    }

    /// Load state produced by [`capture`] — either the current versioned blob or
    /// a legacy single-CPU one (migrated into CPU 0, so old templates restore).
    /// Tolerant of a short blob (missing bytes read as 0), never panics.
    pub fn restore(&mut self, b: &[u8]) {
        if b.first() == Some(&Self::BLOB_MAGIC) {
            self.restore_v2(b);
        } else {
            self.restore_legacy(b);
        }
    }

    fn restore_v2(&mut self, b: &[u8]) {
        let mut c = 2usize; // skip magic + version
        let mut next = || {
            let x = b.get(c).copied().unwrap_or(0);
            c += 1;
            x
        };
        let ncpu = (next() as usize).clamp(1, MAX_CPUS);
        // Never shrink below the configured vcpu count: a blob recording fewer cpus
        // than this VM has would otherwise leave `cpus` too short, and the first
        // per-vcpu GIC access on a higher vcpu would index out of bounds and panic.
        // Read the `ncpu` banks the blob actually holds; any extra stay default.
        self.cpus = vec![CpuBank::default(); ncpu.max(self.cpus.len())];
        self.dist_enabled = next() != 0;
        for k in 0..ncpu {
            let bank = &mut self.cpus[k];
            bank.cpu_enabled = next() != 0;
            bank.pmr = next();
            for i in 0..PRIVATE {
                bank.enabled[i] = next() != 0;
            }
            for i in 0..PRIVATE {
                bank.pending[i] = next() != 0;
            }
            for i in 0..PRIVATE {
                bank.active[i] = next() != 0;
            }
            for i in 0..PRIVATE {
                bank.level[i] = next() != 0;
            }
            for i in 0..PRIVATE {
                bank.edge[i] = next() != 0;
            }
            for i in 0..PRIVATE {
                bank.priority[i] = next();
            }
            for i in 0..16 {
                bank.sgi_src[i] = next();
            }
            let n = next() as usize;
            bank.running = (0..n).map(|_| next()).collect();
        }
        for i in 0..NUM_INTID {
            self.enabled[i] = next() != 0;
        }
        for i in 0..NUM_INTID {
            self.pending[i] = next() != 0;
        }
        for i in 0..NUM_INTID {
            self.active[i] = next() != 0;
        }
        for i in 0..NUM_INTID {
            self.level[i] = next() != 0;
        }
        for i in 0..NUM_INTID {
            self.edge[i] = next() != 0;
        }
        for i in 0..NUM_INTID {
            self.priority[i] = next();
        }
        for i in 0..NUM_INTID {
            self.targets[i] = next();
        }
    }

    /// The pre-multi-CPU layout: flat single-CPU arrays over all 256 INTIDs.
    /// Private INTIDs migrate into CPU 0's bank, SPIs into the shared block.
    fn restore_legacy(&mut self, b: &[u8]) {
        let mut c = 0usize;
        let mut next = || {
            let x = b.get(c).copied().unwrap_or(0);
            c += 1;
            x
        };
        self.cpus = vec![CpuBank::default(); 1];
        self.dist_enabled = next() != 0;
        self.cpus[0].cpu_enabled = next() != 0;
        self.cpus[0].pmr = next();
        // enabled, pending, active, level, edge: NUM_INTID bytes each.
        for field in 0..5usize {
            for i in 0..NUM_INTID {
                let v = next() != 0;
                let bank = &mut self.cpus[0];
                match (field, i < PRIVATE) {
                    (0, true) => bank.enabled[i] = v,
                    (0, false) => self.enabled[i] = v,
                    (1, true) => bank.pending[i] = v,
                    (1, false) => self.pending[i] = v,
                    (2, true) => bank.active[i] = v,
                    (2, false) => self.active[i] = v,
                    (3, true) => bank.level[i] = v,
                    (3, false) => self.level[i] = v,
                    (4, true) => bank.edge[i] = v,
                    (4, false) => self.edge[i] = v,
                    _ => unreachable!(),
                }
            }
        }
        for i in 0..NUM_INTID {
            let p = next();
            if i < PRIVATE {
                self.cpus[0].priority[i] = p;
            } else {
                self.priority[i] = p;
            }
        }
        let n = next() as usize;
        self.cpus[0].running = (0..n).map(|_| next()).collect();
        self.targets = [0x01; NUM_INTID];
    }

    // --- distributor MMIO (banked registers use the accessing CPU) -----------

    pub fn dist_read(&self, cpu: usize, off: u64, size: u8) -> u64 {
        let v = self.dist_read32(cpu, off & !0x3);
        extract(v as u64, off, size)
    }

    fn dist_read32(&self, cpu: usize, off: u64) -> u32 {
        match off {
            gicd::CTLR => self.dist_enabled as u32,
            // ITLinesNumber in [4:0]; CPUNumber (ncpu-1) in [7:5].
            gicd::TYPER => {
                (((NUM_INTID / 32 - 1) as u32) & 0x1f) | ((self.cpus.len() as u32 - 1) << 5)
            }
            gicd::IIDR => 0x0200_043b, // arm implementer, GICv2-ish
            o if range(o, gicd::IGROUPR, 32) => 0, // all group 0
            o if range(o, gicd::ISENABLER, 32) || range(o, gicd::ICENABLER, 32) => {
                let start = if o < gicd::ICENABLER { gicd::ISENABLER } else { gicd::ICENABLER };
                self.bitset_read(cpu, base_index(o, start), |g, c, i| g.enabled_at(c, i))
            }
            o if range(o, gicd::ISPENDR, 32) || range(o, gicd::ICPENDR, 32) => {
                let start = if o < gicd::ICPENDR { gicd::ISPENDR } else { gicd::ICPENDR };
                self.bitset_read(cpu, base_index(o, start), |g, c, i| g.is_pending(c, i))
            }
            o if range(o, gicd::ISACTIVER, 32) || range(o, gicd::ICACTIVER, 32) => {
                let start = if o < gicd::ICACTIVER { gicd::ISACTIVER } else { gicd::ICACTIVER };
                self.bitset_read(cpu, base_index(o, start), |g, c, i| g.active_at(c, i))
            }
            o if range(o, gicd::IPRIORITYR, 64) => {
                let base = ((o - gicd::IPRIORITYR) & !0x3) as usize;
                let mut v = 0u32;
                for b in 0..4 {
                    if base + b < NUM_INTID {
                        v |= (self.priority_at(cpu, base + b) as u32) << (b * 8);
                    }
                }
                v
            }
            // ITARGETSR: private INTIDs read as "this CPU"; SPIs read their mask.
            o if range(o, gicd::ITARGETSR, 64) => {
                let base = ((o - gicd::ITARGETSR) & !0x3) as usize;
                let mut v = 0u32;
                for b in 0..4 {
                    let i = base + b;
                    if i < PRIVATE {
                        v |= ((1u32 << cpu) & 0xff) << (b * 8);
                    } else if i < NUM_INTID {
                        v |= (self.targets[i] as u32) << (b * 8);
                    }
                }
                v
            }
            o if range(o, gicd::ICFGR, 16) => {
                let base = base_index_cfg(o);
                let mut v = 0u32;
                for b in 0..16 {
                    if base + b < NUM_INTID && self.edge_at(cpu, base + b) {
                        v |= 0b10 << (b * 2);
                    }
                }
                v
            }
            _ => 0,
        }
    }

    pub fn dist_write(&mut self, cpu: usize, off: u64, size: u8, val: u64) {
        let off32 = off & !0x3;
        // Byte-granular writes matter for IPRIORITYR/ITARGETSR; handle those
        // directly, everything else is naturally 32-bit.
        if range(off, gicd::IPRIORITYR, 64) {
            let idx = (off - gicd::IPRIORITYR) as usize;
            for k in 0..size as usize {
                let i = idx + k;
                let p = (val >> (k * 8)) as u8;
                if i < PRIVATE {
                    self.cpus[cpu].priority[i] = p;
                } else if i < NUM_INTID {
                    self.priority[i] = p;
                }
            }
            return;
        }
        if range(off, gicd::ITARGETSR, 64) {
            let idx = (off - gicd::ITARGETSR) as usize;
            // Compute the mask in u16 then truncate: with 8 cpus, 1u16<<8 == 256,
            // so `(… as u8) - 1` would overflow (panic in debug, wrap in release).
            let cpu_mask = ((1u16 << self.cpus.len()) - 1) as u8;
            for k in 0..size as usize {
                let i = idx + k;
                // Private targets are read-only; SPI targets clamp to real CPUs.
                if (PRIVATE..NUM_INTID).contains(&i) {
                    self.targets[i] = (val >> (k * 8)) as u8 & cpu_mask;
                }
            }
            return;
        }
        let v = val as u32;
        match off32 {
            gicd::CTLR => self.dist_enabled = v & 1 != 0,
            o if range(o, gicd::ISENABLER, 32) => self.bits_set(cpu, o, gicd::ISENABLER, v, |g, c, i, on| {
                if i < PRIVATE { g.cpus[c].enabled[i] = on } else { g.enabled[i] = on }
            }, true),
            o if range(o, gicd::ICENABLER, 32) => self.bits_set(cpu, o, gicd::ICENABLER, v, |g, c, i, on| {
                if i < PRIVATE { g.cpus[c].enabled[i] = on } else { g.enabled[i] = on }
            }, false),
            o if range(o, gicd::ISPENDR, 32) => self.bits_set(cpu, o, gicd::ISPENDR, v, |g, c, i, on| {
                if i < PRIVATE { g.cpus[c].pending[i] = on } else { g.pending[i] = on }
            }, true),
            o if range(o, gicd::ICPENDR, 32) => self.bits_set(cpu, o, gicd::ICPENDR, v, |g, c, i, on| {
                if i < PRIVATE { g.cpus[c].pending[i] = on } else { g.pending[i] = on }
            }, false),
            o if range(o, gicd::ISACTIVER, 32) => self.bits_set(cpu, o, gicd::ISACTIVER, v, |g, c, i, on| {
                if i < PRIVATE { g.cpus[c].active[i] = on } else { g.active[i] = on }
            }, true),
            o if range(o, gicd::ICACTIVER, 32) => self.bits_set(cpu, o, gicd::ICACTIVER, v, |g, c, i, on| {
                if i < PRIVATE { g.cpus[c].active[i] = on } else { g.active[i] = on }
            }, false),
            o if range(o, gicd::ICFGR, 16) => {
                let base = base_index_cfg(o);
                for b in 0..16 {
                    let i = base + b;
                    let edge = (v >> (b * 2 + 1)) & 1 != 0;
                    // SGIs are fixed edge; PPIs are banked; SPIs shared.
                    if (16..PRIVATE).contains(&i) {
                        self.cpus[cpu].edge[i] = edge;
                    } else if (PRIVATE..NUM_INTID).contains(&i) {
                        self.edge[i] = edge;
                    }
                }
            }
            gicd::SGIR => self.sgir(cpu, v),
            _ => {}
        }
    }

    /// GICD_SGIR: post a software-generated interrupt (the IPI) from `cpu` to the
    /// targets selected by the filter: 0 = the CPUTargetList mask, 1 = all but
    /// self, 2 = self only.
    fn sgir(&mut self, cpu: usize, v: u32) {
        let sgi = (v & 0xf) as usize;
        let filter = (v >> 24) & 0x3;
        let list = ((v >> 16) & 0xff) as u8;
        for t in 0..self.cpus.len() {
            let hit = match filter {
                0 => list & (1 << t) != 0,
                1 => t != cpu,
                _ => t == cpu,
            };
            if hit {
                self.cpus[t].pending[sgi] = true;
                self.cpus[t].sgi_src[sgi] = cpu as u8;
            }
        }
    }

    // --- CPU interface MMIO (always the accessing CPU's bank) ----------------

    /// IAR has side effects (acknowledge), so reads take `&mut self`.
    pub fn cpu_read(&mut self, cpu: usize, off: u64, size: u8) -> u64 {
        let v = match off & !0x3 {
            gicc::CTLR => self.cpus[cpu].cpu_enabled as u32,
            gicc::PMR => self.cpus[cpu].pmr as u32,
            gicc::BPR => 0,
            gicc::IAR => self.acknowledge(cpu),
            gicc::RPR => self.running_prio(cpu) as u32,
            gicc::HPPIR => self.highest_pending(cpu).map(|(i, _)| i).unwrap_or(SPURIOUS),
            gicc::IIDR => 0x0002_043b,
            _ => 0,
        };
        extract(v as u64, off, size)
    }

    pub fn cpu_write(&mut self, cpu: usize, off: u64, _size: u8, val: u64) {
        let v = val as u32;
        match off & !0x3 {
            gicc::CTLR => self.cpus[cpu].cpu_enabled = v & 1 != 0,
            gicc::PMR => self.cpus[cpu].pmr = v as u8,
            gicc::EOIR => self.end_of_interrupt(cpu, v & 0x3ff),
            _ => {}
        }
    }

    /// GICC_IAR: return the highest-priority deliverable INTID, mark it active,
    /// consume its edge-pending latch, and push its priority on the running
    /// stack. For SGIs the source CPU rides in bits [12:10].
    fn acknowledge(&mut self, cpu: usize) -> u32 {
        match self.highest_pending(cpu) {
            None => SPURIOUS,
            Some((intid, prio)) => {
                let i = intid as usize;
                if i < PRIVATE {
                    let bank = &mut self.cpus[cpu];
                    bank.active[i] = true;
                    if bank.edge[i] {
                        bank.pending[i] = false;
                    }
                } else {
                    self.active[i] = true;
                    if self.edge[i] {
                        self.pending[i] = false;
                    }
                }
                self.cpus[cpu].running.push(prio);
                if i < 16 {
                    intid | ((self.cpus[cpu].sgi_src[i] as u32) << 10)
                } else {
                    intid
                }
            }
        }
    }

    /// GICC_EOIR: deactivate the INTID and pop one running-priority level.
    fn end_of_interrupt(&mut self, cpu: usize, intid: u32) {
        let i = intid as usize;
        if i < PRIVATE {
            self.cpus[cpu].active[i] = false;
        } else if i < NUM_INTID {
            self.active[i] = false;
        }
        self.cpus[cpu].running.pop();
    }

    // --- helpers ------------------------------------------------------------

    fn bitset_read(&self, cpu: usize, base: usize, f: impl Fn(&Self, usize, usize) -> bool) -> u32 {
        let mut v = 0u32;
        for b in 0..32 {
            if base + b < NUM_INTID && f(self, cpu, base + b) {
                v |= 1 << b;
            }
        }
        v
    }

    fn bits_set(
        &mut self,
        cpu: usize,
        off: u64,
        start: u64,
        val: u32,
        f: impl Fn(&mut Self, usize, usize, bool),
        on: bool,
    ) {
        let base = base_index(off, start);
        for b in 0..32 {
            if val & (1 << b) != 0 && base + b < NUM_INTID {
                f(self, cpu, base + b, on);
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

    fn enabled_gic_n(n: usize) -> GicV2 {
        let mut g = GicV2::with_cpus(n);
        g.dist_write(0, gicd::CTLR, 4, 1);
        for c in 0..n {
            g.cpu_write(c, gicc::CTLR, 4, 1);
            g.cpu_write(c, gicc::PMR, 4, 0xff);
        }
        g
    }
    fn enabled_gic() -> GicV2 {
        enabled_gic_n(1)
    }

    #[test]
    fn restore_of_a_smaller_blob_does_not_shrink_below_vcpu_count() {
        // A blob captured from a 1-cpu GIC restored into a 4-cpu GIC must keep 4
        // banks, or a per-vcpu access on a higher vcpu would index out of bounds.
        let blob = enabled_gic_n(1).capture();
        let mut g = GicV2::with_cpus(4);
        g.restore(&blob);
        assert!(g.num_cpus() >= 4, "restore shrank the bank set below the vcpu count");
        // Would panic before the fix (cpus had length 1).
        g.set_level(3, SPI, true);
        let _ = g.irq_pending(3);
    }

    #[test]
    fn enable_pend_ack_eoi_cycle() {
        let mut g = enabled_gic();
        g.dist_write(0, gicd::IPRIORITYR + SPI as u64, 1, 0xa0);
        g.dist_write(0, gicd::ISENABLER + (SPI / 32) as u64 * 4, 4, 1 << (SPI % 32));

        assert!(!g.irq_pending(0), "no line raised yet");
        g.set_level(0, SPI, true);
        assert!(g.irq_pending(0), "deliverable once the line is high");

        let iar = g.cpu_read(0, gicc::IAR, 4) as u32;
        assert_eq!(iar, SPI);
        assert!(!g.irq_pending(0), "active interrupt masks itself until EOI");

        g.set_level(0, SPI, false);
        g.cpu_write(0, gicc::EOIR, 4, SPI as u64);
        assert!(!g.irq_pending(0));
        assert_eq!(g.cpu_read(0, gicc::IAR, 4) as u32, SPURIOUS);
    }

    #[test]
    fn pmr_masks_low_priority() {
        let mut g = enabled_gic();
        g.dist_write(0, gicd::IPRIORITYR + SPI as u64, 1, 0x80);
        g.dist_write(0, gicd::ISENABLER + (SPI / 32) as u64 * 4, 4, 1 << (SPI % 32));
        g.set_level(0, SPI, true);
        g.cpu_write(0, gicc::PMR, 4, 0x80);
        assert!(!g.irq_pending(0), "priority == PMR is masked");
        g.cpu_write(0, gicc::PMR, 4, 0x81);
        assert!(g.irq_pending(0), "priority < PMR passes");
    }

    #[test]
    fn edge_timer_ppi_clears_on_ack() {
        let mut g = enabled_gic();
        let ppi = 27u32; // virtual timer
        g.cpus[0].edge[ppi as usize] = true;
        g.dist_write(0, gicd::IPRIORITYR + ppi as u64, 1, 0x80);
        g.dist_write(0, gicd::ISENABLER, 4, 1 << ppi);
        g.dist_write(0, gicd::ISPENDR, 4, 1 << ppi);
        assert!(g.irq_pending(0));
        assert_eq!(g.cpu_read(0, gicc::IAR, 4) as u32, ppi);
        g.cpu_write(0, gicc::EOIR, 4, ppi as u64);
        assert!(!g.irq_pending(0), "edge pending does not re-fire without a new event");
    }

    #[test]
    fn priority_orders_two_pending() {
        let mut g = enabled_gic();
        let (a, b) = (32u32, 40u32);
        g.dist_write(0, gicd::IPRIORITYR + a as u64, 1, 0x90);
        g.dist_write(0, gicd::IPRIORITYR + b as u64, 1, 0x50);
        g.dist_write(0, gicd::ISENABLER + 4, 4, (1 << (a % 32)) | (1 << (b % 32)));
        g.set_level(0, a, true);
        g.set_level(0, b, true);
        assert_eq!(g.cpu_read(0, gicc::IAR, 4) as u32, b);
    }

    // --- multi-CPU ----------------------------------------------------------

    /// Enable an SGI on a CPU's bank (priority mid, enabled).
    fn arm_sgi(g: &mut GicV2, cpu: usize, sgi: u32) {
        g.dist_write(cpu, gicd::IPRIORITYR + sgi as u64, 1, 0x80);
        g.dist_write(cpu, gicd::ISENABLER, 4, 1 << sgi);
    }

    #[test]
    fn sgi_targets_one_cpu_with_source_in_iar() {
        let mut g = enabled_gic_n(2);
        arm_sgi(&mut g, 0, 3);
        arm_sgi(&mut g, 1, 3);
        // CPU 0 posts SGI 3 to CPU 1 only (filter 0, list = bit 1).
        g.dist_write(0, gicd::SGIR, 4, (1 << 17) | 3);
        assert!(!g.irq_pending(0), "sender not targeted");
        assert!(g.irq_pending(1), "target sees the IPI");
        assert_eq!(g.pending_cpus(), 0b10);
        // IAR on the target reports the INTID with the source CPU in [12:10].
        assert_eq!(g.cpu_read(1, gicc::IAR, 4) as u32, 3); // src 0 → bits zero
        g.cpu_write(1, gicc::EOIR, 4, 3);
        assert!(!g.irq_pending(1));
    }

    #[test]
    fn sgi_all_but_self() {
        let mut g = enabled_gic_n(3);
        for c in 0..3 {
            arm_sgi(&mut g, c, 1);
        }
        // CPU 1 posts SGI 1 with filter 1 (all but self).
        g.dist_write(1, gicd::SGIR, 4, (1 << 24) | 1);
        assert!(g.irq_pending(0));
        assert!(!g.irq_pending(1));
        assert!(g.irq_pending(2));
        // Source CPU 1 rides in IAR bits [12:10] on a receiver.
        assert_eq!(g.cpu_read(0, gicc::IAR, 4) as u32, 1 | (1 << 10));
    }

    #[test]
    fn private_state_is_banked_per_cpu() {
        let mut g = enabled_gic_n(2);
        let ppi = 27u32; // each CPU's own virtual timer
        for c in 0..2 {
            g.dist_write(c, gicd::IPRIORITYR + ppi as u64, 1, 0x80);
            g.dist_write(c, gicd::ISENABLER, 4, 1 << ppi);
        }
        g.set_level(0, ppi, true); // only CPU 0's timer fires
        assert!(g.irq_pending(0));
        assert!(!g.irq_pending(1), "PPI is banked, not broadcast");
        // PMR is banked too: masking CPU 0 leaves CPU 1's view unchanged.
        g.cpu_write(0, gicc::PMR, 4, 0);
        assert!(!g.irq_pending(0));
        g.set_level(1, ppi, true);
        assert!(g.irq_pending(1));
    }

    #[test]
    fn spi_routes_by_itargetsr() {
        let mut g = enabled_gic_n(2);
        g.dist_write(0, gicd::IPRIORITYR + SPI as u64, 1, 0x80);
        g.dist_write(0, gicd::ISENABLER + 4, 4, 1 << (SPI % 32));
        g.set_level(0, SPI, true);
        assert!(g.irq_pending(0), "SPIs reset to CPU 0");
        assert!(!g.irq_pending(1));
        // Retarget the SPI to CPU 1.
        g.dist_write(0, gicd::ITARGETSR + SPI as u64, 1, 0x02);
        assert!(!g.irq_pending(0));
        assert!(g.irq_pending(1));
        // Reads reflect the mask; private ITARGETSR reads as "this CPU".
        assert_eq!(g.dist_read(0, gicd::ITARGETSR + SPI as u64, 1), 0x02);
        assert_eq!(g.dist_read(1, gicd::ITARGETSR, 1), 0x02); // SGI 0 from CPU 1
    }

    #[test]
    fn typer_reports_cpu_count() {
        let g = enabled_gic_n(4);
        let typer = g.dist_read(0, gicd::TYPER, 4) as u32;
        assert_eq!((typer >> 5) & 0x7, 3); // CPUNumber = ncpu - 1
    }

    #[test]
    fn snapshot_roundtrips_multi_cpu() {
        let mut g = enabled_gic_n(2);
        arm_sgi(&mut g, 1, 5);
        g.dist_write(0, gicd::SGIR, 4, (1 << 17) | 5); // SGI 5 → CPU 1
        g.dist_write(0, gicd::ITARGETSR + SPI as u64, 1, 0x02);
        g.cpu_write(1, gicc::PMR, 4, 0x90); // still above the SGI's 0x80 priority

        let blob = g.capture();
        let mut h = GicV2::new();
        h.restore(&blob);
        assert_eq!(h.num_cpus(), 2);
        assert!(h.irq_pending(1), "pending SGI survives the roundtrip");
        assert_eq!(h.cpus[1].pmr, 0x90);
        assert_eq!(h.targets[SPI as usize], 0x02);
        assert_eq!(h.capture(), blob, "capture is stable across a roundtrip");
    }

    #[test]
    fn legacy_blob_restores_into_cpu0() {
        // Hand-build a v1 blob: dist on, cpu on, pmr 0xff, then the five
        // NUM_INTID bool arrays, priorities, and an empty running stack.
        let mut b = vec![1u8, 1, 0xff];
        let mut enabled = vec![0u8; NUM_INTID];
        enabled[27] = 1; // timer PPI enabled
        enabled[33] = 1; // a SPI enabled
        let mut level = vec![0u8; NUM_INTID];
        level[33] = 1; // SPI line high
        b.extend_from_slice(&enabled); // enabled
        b.extend(vec![0u8; NUM_INTID]); // pending
        b.extend(vec![0u8; NUM_INTID]); // active
        b.extend_from_slice(&level); // level
        b.extend(vec![0u8; NUM_INTID]); // edge
        let mut prio = vec![0xffu8; NUM_INTID];
        prio[33] = 0x80;
        b.extend_from_slice(&prio);
        b.push(0); // running stack empty

        let mut g = GicV2::new();
        g.restore(&b);
        assert_eq!(g.num_cpus(), 1);
        assert!(g.cpus[0].enabled[27], "private INTID migrated to cpu0's bank");
        assert!(g.enabled[33] && g.level[33], "SPI migrated to the shared block");
        assert!(g.irq_pending(0), "the SPI is deliverable after migration");
    }

    #[test]
    fn itargetsr_write_with_max_cpus_does_not_overflow() {
        // Regression: the 8-cpu mask was `(1u16<<8) as u8 - 1`, which panics in
        // debug. One ITARGETSR write at MAX_CPUS must not panic.
        let mut g = GicV2::with_cpus(MAX_CPUS);
        g.dist_write(0, gicd::ITARGETSR, 4, 0xffff_ffff);
    }
}
