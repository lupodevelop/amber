//! The Apple Silicon backend. Implements the two traits from amber-core and
//! nothing else. Everything hard about it is concentrated in `run`: turning a
//! raw HVF exit into the shared `VmExit` vocabulary.

mod ffi;
mod gicv2;
mod sysregs;

use amber_core::hypervisor::{decode_data_abort, Hypervisor, MmioAccess, MmioOp, Vcpu, VmExit};
use amber_core::snapshot::CpuSnapshot;
use amber_core::{Error, GicInfo, GicKind, GuestMemory, Result};
use ffi::*;
use gicv2::GicV2;
use std::sync::{Arc, Mutex};

/// Whether to use the software GICv2 instead of the in-kernel vGIC. The `swgic`
/// feature is on by default and the software GIC is the default at runtime too
/// (it makes snapshot/restore/fork/exec work); `AMBER_GIC=hw` opts back to the
/// in-kernel vGIC, and a `--no-default-features` build has no software GIC at all.
fn swgic_mode() -> bool {
    cfg!(feature = "swgic") && std::env::var("AMBER_GIC").as_deref() != Ok("hw")
}

fn check(ret: hv_return_t, what: &str) -> Result<()> {
    if ret == HV_SUCCESS {
        Ok(())
    } else {
        Err(Error::Backend(format!("{what} failed: {ret:#x}")))
    }
}

fn align_up(x: u64, a: u64) -> u64 {
    if a == 0 {
        x
    } else {
        (x + a - 1) & !(a - 1)
    }
}

pub struct HvfVm {
    gic: GicInfo,
    /// The software GICv2, when running in `swgic` mode; None means the in-kernel
    /// vGIC. Shared (Arc/Mutex) because `set_irq` is called from the console reader
    /// thread while the vcpu thread reads/writes it in the run loop.
    swgic: Option<Arc<Mutex<GicV2>>>,
}

impl Hypervisor for HvfVm {
    type Vcpu = HvfVcpu;

    fn create(mem: &GuestMemory) -> Result<Self> {
        unsafe {
            check(hv_vm_create(std::ptr::null_mut()), "hv_vm_create")?;
            check(
                hv_vm_map(
                    mem.host_ptr() as *mut _,
                    mem.base(),
                    mem.len(),
                    HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC,
                ),
                "hv_vm_map",
            )?;
        }
        if swgic_mode() {
            // Software GICv2: no in-kernel vGIC. The distributor and CPU interface
            // live at the QEMU `virt` GICv2 addresses; both are emulated MMIO.
            use amber_core::layout::{GIC_CPU_BASE, GIC_CPU_SIZE, GIC_DIST_BASE, GIC_DIST_SIZE};
            log::info!("GICv2 (software): dist {GIC_DIST_BASE:#x}, cpu {GIC_CPU_BASE:#x}");
            let gic = GicInfo {
                dist_base: GIC_DIST_BASE,
                dist_size: GIC_DIST_SIZE,
                redist_base: GIC_CPU_BASE,
                redist_size: GIC_CPU_SIZE,
                kind: GicKind::V2,
            };
            Ok(HvfVm { gic, swgic: Some(Arc::new(Mutex::new(GicV2::new()))) })
        } else {
            let gic = Self::create_gic()?;
            Ok(HvfVm { gic, swgic: None })
        }
    }

    fn create_vcpu(&mut self, _id: u8) -> Result<HvfVcpu> {
        let mut handle: hv_vcpu_t = 0;
        let mut exit: *mut hv_vcpu_exit_t = std::ptr::null_mut();
        unsafe {
            check(
                hv_vcpu_create(&mut handle, &mut exit, std::ptr::null_mut()),
                "hv_vcpu_create",
            )?;
            // GICv3 routes by affinity, so the redistributor HVF allocated for
            // this vcpu is found via MPIDR. vcpu 0 -> affinity 0; bit 31 is RES1.
            check(
                hv_vcpu_set_sys_reg(handle, HV_SYS_REG_MPIDR_EL1, 0x8000_0000),
                "set MPIDR_EL1",
            )?;
        }
        Ok(HvfVcpu { handle, exit, swgic: self.swgic.clone(), vtimer_masked: false })
    }

    fn gic_info(&self) -> Option<GicInfo> {
        Some(self.gic)
    }

    fn set_irq(&self, intid: u32, level: bool) -> Result<()> {
        if let Some(gic) = &self.swgic {
            gic.lock().unwrap().set_level(intid, level);
            Ok(())
        } else {
            unsafe { check(hv_gic_set_spi(intid, level), "hv_gic_set_spi") }
        }
    }

    fn capture_gic(&self) -> Result<Vec<u8>> {
        // Software GIC: its state is plain bytes (and, unlike the vGIC, the
        // restored timer survives because we deliver it ourselves).
        if let Some(gic) = &self.swgic {
            return Ok(gic.lock().unwrap().capture());
        }
        unsafe {
            let state = hv_gic_state_create();
            if state.is_null() {
                return Err(Error::Snapshot("hv_gic_state_create returned null".into()));
            }
            let mut size = 0usize;
            check(hv_gic_state_get_size(state, &mut size), "gic state size")?;
            let mut buf = vec![0u8; size];
            check(
                hv_gic_state_get_data(state, buf.as_mut_ptr() as *mut _),
                "gic state data",
            )?;
            // `state` is an os_object; leak it (snapshot is a one-time event).
            Ok(buf)
        }
    }

    fn restore_gic(&self, blob: &[u8]) -> Result<()> {
        if let Some(gic) = &self.swgic {
            gic.lock().unwrap().restore(blob);
            return Ok(());
        }
        unsafe {
            check(
                hv_gic_set_state(blob.as_ptr() as *const _, blob.len()),
                "hv_gic_set_state",
            )
        }
    }
}

impl HvfVm {
    /// Create the GICv3. Must run after the VM exists and before any vcpu, so
    /// HVF can allocate the per-vcpu CPU-interface resources. The distributor and
    /// redistributor sizes come from the host; the bases are placed below RAM and
    /// reported up so the DTB advertises the same layout. Placed just below the
    /// PL011 (0x9000000), clear of it.
    fn create_gic() -> Result<GicInfo> {
        let (mut dist_size, mut redist_size, mut redist_align) = (0usize, 0usize, 0usize);
        unsafe {
            check(hv_gic_get_distributor_size(&mut dist_size), "gic dist size")?;
            check(
                hv_gic_get_redistributor_region_size(&mut redist_size),
                "gic redist region size",
            )?;
            check(
                hv_gic_get_redistributor_base_alignment(&mut redist_align),
                "gic redist align",
            )?;
        }

        // QEMU `virt` places the distributor at 0x8000000; keep that, it fits in
        // the 16 MiB below the PL011. The redistributor region does NOT: HVF
        // sizes it for the maximum cpu count (tens of MiB), so put it in the wide
        // gap above the PL011 and below RAM, where it has room.
        use amber_core::layout::{GIC_DIST_BASE, PL011_BASE, PL011_SIZE, RAM_BASE};
        let dist_base = GIC_DIST_BASE;
        if dist_base + dist_size as u64 > PL011_BASE {
            return Err(Error::Backend(format!(
                "GIC distributor {:#x}+{:#x} collides with PL011 at {:#x}",
                dist_base, dist_size, PL011_BASE
            )));
        }
        let redist_base = align_up(PL011_BASE + PL011_SIZE, redist_align as u64);
        if redist_base + redist_size as u64 > RAM_BASE {
            return Err(Error::Backend(format!(
                "GIC redistributor {:#x}+{:#x} does not fit below RAM at {:#x}",
                redist_base, redist_size, RAM_BASE
            )));
        }

        unsafe {
            let cfg = hv_gic_config_create();
            if cfg.is_null() {
                return Err(Error::Backend("hv_gic_config_create returned null".into()));
            }
            check(
                hv_gic_config_set_distributor_base(cfg, dist_base),
                "gic set dist base",
            )?;
            check(
                hv_gic_config_set_redistributor_base(cfg, redist_base),
                "gic set redist base",
            )?;
            check(hv_gic_create(cfg), "hv_gic_create")?;
            // cfg is an os_object; one-time, leak it rather than link libobjc.
        }

        let (mut spi_base, mut spi_count) = (0u32, 0u32);
        unsafe {
            check(
                hv_gic_get_spi_interrupt_range(&mut spi_base, &mut spi_count),
                "gic spi range",
            )?;
        }
        log::info!(
            "GICv3: dist {:#x}+{:#x}, redist {:#x}+{:#x}, SPI base {} count {}",
            dist_base, dist_size, redist_base, redist_size, spi_base, spi_count
        );
        if spi_base != amber_core::layout::GIC_SPI_BASE {
            log::warn!(
                "GIC SPI base {} != assumed {}; PL011 IRQ may be misrouted",
                spi_base, amber_core::layout::GIC_SPI_BASE
            );
        }
        Ok(GicInfo {
            dist_base,
            dist_size: dist_size as u64,
            redist_base,
            redist_size: redist_size as u64,
            kind: GicKind::V3,
        })
    }
}

impl Drop for HvfVm {
    fn drop(&mut self) {
        unsafe {
            hv_vm_destroy();
        }
    }
}

pub struct HvfVcpu {
    handle: hv_vcpu_t,
    exit: *mut hv_vcpu_exit_t,
    /// Shared software GICv2 in `swgic` mode (None for the in-kernel vGIC).
    swgic: Option<Arc<Mutex<GicV2>>>,
    /// Whether HVF's virtual timer is currently masked. In `swgic` mode we keep it
    /// masked and drive the timer line ourselves; the flag avoids redundant calls.
    vtimer_masked: bool,
}

impl HvfVcpu {
    fn reg(&self, reg: hv_reg_t) -> Result<u64> {
        let mut v = 0u64;
        unsafe { check(hv_vcpu_get_reg(self.handle, reg, &mut v), "get_reg")? };
        Ok(v)
    }
    fn set_reg(&mut self, reg: hv_reg_t, v: u64) -> Result<()> {
        unsafe { check(hv_vcpu_set_reg(self.handle, reg, v), "set_reg") }
    }
    fn get_sys(&self, reg: hv_sys_reg_t) -> Result<u64> {
        let mut v = 0u64;
        unsafe { check(hv_vcpu_get_sys_reg(self.handle, reg, &mut v), "get_sys")? };
        Ok(v)
    }

    /// Before each guest entry in swgic mode: keep HVF's vtimer masked (we own the
    /// timer), drive the virtual-timer line (PPI 27) from the guest's CNTV, and
    /// raise the vcpu IRQ line iff the software GIC has a deliverable interrupt.
    /// `hv_vcpu_set_pending_interrupt` is cleared after every run, so this re-asserts
    /// it each iteration.
    fn swgic_prep(&mut self) -> Result<()> {
        let Some(gic) = self.swgic.clone() else { return Ok(()) };
        if !self.vtimer_masked {
            unsafe { check(hv_vcpu_set_vtimer_mask(self.handle, true), "vtimer mask")? };
            self.vtimer_masked = true;
        }
        // Virtual timer output: enabled (bit0), not masked (bit1), fired (bit2).
        let ctl = self.get_sys(HV_SYS_REG_CNTV_CTL_EL0).unwrap_or(0);
        let due = ctl & 0b001 != 0 && ctl & 0b010 == 0 && ctl & 0b100 != 0;
        let pend = {
            let mut g = gic.lock().unwrap();
            g.set_level(VTIMER_INTID, due);
            g.irq_pending()
        };
        // HVF auto-clears the pending interrupt after each run, so only the `true`
        // case needs a syscall — skipping the common `false` case halves the cost.
        if pend {
            unsafe {
                check(
                    hv_vcpu_set_pending_interrupt(self.handle, HV_INTERRUPT_TYPE_IRQ, true),
                    "set pending irq",
                )?;
            }
        }
        Ok(())
    }

    /// Service a GICD/GICC MMIO access against the software GIC. Returns true if the
    /// address was in the GIC (caller steps PC and resumes); false otherwise (or in
    /// vGIC mode), so the access falls through to the run loop's device model.
    fn swgic_mmio(&mut self, access: &MmioAccess) -> Result<bool> {
        let Some(gic) = self.swgic.clone() else { return Ok(false) };
        use amber_core::layout::{GIC_CPU_BASE, GIC_CPU_SIZE, GIC_DIST_BASE, GIC_DIST_SIZE};
        let ipa = access.ipa;
        let (off, is_cpu) = if ipa >= GIC_DIST_BASE && ipa < GIC_DIST_BASE + GIC_DIST_SIZE {
            (ipa - GIC_DIST_BASE, false)
        } else if ipa >= GIC_CPU_BASE && ipa < GIC_CPU_BASE + GIC_CPU_SIZE {
            (ipa - GIC_CPU_BASE, true)
        } else {
            return Ok(false);
        };
        let mut g = gic.lock().unwrap();
        match access.op {
            MmioOp::Read { reg } => {
                let v = if is_cpu {
                    g.cpu_read(off, access.size)
                } else {
                    g.dist_read(off, access.size)
                };
                drop(g);
                if reg != 31 {
                    self.set_x(reg, v)?;
                }
            }
            MmioOp::Write { value, .. } => {
                if is_cpu {
                    g.cpu_write(off, access.size, value);
                } else {
                    g.dist_write(off, access.size, value);
                }
            }
        }
        Ok(true)
    }
}

/// The EL1 virtual timer is PPI 11, i.e. GIC INTID 27.
const VTIMER_INTID: u32 = 27;

/// Convert a mach-timebase tick count to nanoseconds. CNTVCT/CNTV_CVAL on Apple
/// Silicon share `mach_absolute_time`'s timebase, so the ratio applies directly.
#[allow(deprecated)] // libc's mach_* are fine here; mach2 is the only alternative
fn mach_ticks_to_ns(ticks: u64) -> u64 {
    let mut tb = libc::mach_timebase_info_data_t { numer: 0, denom: 0 };
    unsafe { libc::mach_timebase_info(&mut tb) };
    if tb.denom == 0 {
        return ticks;
    }
    ((ticks as u128 * tb.numer as u128) / tb.denom as u128) as u64
}

impl Vcpu for HvfVcpu {
    fn set_boot_regs(&mut self, entry: u64, dtb: u64) -> Result<()> {
        // x0 = DTB, x1..x3 = 0, PC = kernel entry. HVF does NOT start the vcpu at
        // EL1: a fresh vcpu comes up at EL0t with PC=0, so the kernel's first
        // instructions would run unprivileged and trap. Set PSTATE explicitly to
        // EL1h with DAIF masked (0x3c5), which is exactly the state the arm64
        // Linux boot protocol requires at the kernel entry.
        const PSTATE_EL1H_DAIF: u64 = 0x3c5;
        self.set_reg(HV_REG_X0, dtb)?;
        self.set_reg(HV_REG_X0 + 1, 0)?;
        self.set_reg(HV_REG_X0 + 2, 0)?;
        self.set_reg(HV_REG_X0 + 3, 0)?;
        self.set_reg(HV_REG_CPSR, PSTATE_EL1H_DAIF)?;
        self.set_reg(HV_REG_PC, entry)?;
        Ok(())
    }

    fn get_x(&self, idx: u8) -> Result<u64> {
        self.reg(HV_REG_X0 + idx as u32)
    }
    fn set_x(&mut self, idx: u8, val: u64) -> Result<()> {
        self.set_reg(HV_REG_X0 + idx as u32, val)
    }
    fn pc(&self) -> Result<u64> {
        self.reg(HV_REG_PC)
    }
    fn set_pc(&mut self, pc: u64) -> Result<()> {
        self.set_reg(HV_REG_PC, pc)
    }

    #[allow(deprecated)] // libc's mach_* are fine here; mach2 is the only alternative
    fn pending_timer_ns(&self) -> Result<Option<u64>> {
        // CNTV_CTL: ENABLE(0), IMASK(1), ISTATUS(2). An enabled, unmasked timer
        // is due when CNTVCT >= CNTV_CVAL; CNTVCT == mach_absolute_time() minus
        // the vtimer offset, in the same mach-timebase units as CVAL.
        let ctl = self.get_sys(HV_SYS_REG_CNTV_CTL_EL0)?;
        if ctl & 0b001 == 0 || ctl & 0b010 != 0 {
            return Ok(None); // disabled or masked: no timer wake to wait for
        }
        if ctl & 0b100 != 0 {
            return Ok(Some(0)); // already fired
        }
        let cval = self.get_sys(HV_SYS_REG_CNTV_CVAL_EL0)?;
        let mut offset = 0u64;
        unsafe { check(hv_vcpu_get_vtimer_offset(self.handle, &mut offset), "vtimer offset")? };
        let now = unsafe { libc::mach_absolute_time() }.wrapping_sub(offset);
        Ok(Some(if cval <= now {
            0
        } else {
            mach_ticks_to_ns(cval - now)
        }))
    }

    #[allow(deprecated)]
    fn capture(&self) -> Result<CpuSnapshot> {
        let mut x = Vec::with_capacity(31);
        for i in 0..31 {
            x.push(self.reg(HV_REG_X0 + i)?);
        }
        let mut vtimer_offset = 0u64;
        unsafe {
            check(hv_vcpu_get_vtimer_offset(self.handle, &mut vtimer_offset), "vtimer offset")?;
        }

        // System registers: read every one HVF exposes; skip any that refuse
        // (some are conditionally present), recording (id, value) verbatim.
        let mut sysregs = Vec::with_capacity(sysregs::SYS_REGS.len());
        for &(id, _name) in sysregs::SYS_REGS {
            let mut v = 0u64;
            if unsafe { hv_vcpu_get_sys_reg(self.handle, id, &mut v) } == HV_SUCCESS {
                sysregs.push((id, v));
            }
        }

        // SIMD/FP file V0..V31.
        let mut fp = Vec::with_capacity(32);
        for q in 0..32u32 {
            let mut v = [0u8; 16];
            unsafe { hv_vcpu_get_simd_fp_reg(self.handle, q, &mut v) };
            fp.push(v);
        }

        Ok(CpuSnapshot {
            x,
            pc: self.reg(HV_REG_PC)?,
            cpsr: self.reg(HV_REG_CPSR)?,
            fpcr: self.reg(HV_REG_FPCR)?,
            fpsr: self.reg(HV_REG_FPSR)?,
            vtimer_offset,
            mono: unsafe { libc::mach_absolute_time() },
            sysregs,
            fp,
        })
    }

    fn restore(&mut self, cpu: &CpuSnapshot) -> Result<()> {
        for (i, &v) in cpu.x.iter().enumerate() {
            self.set_reg(HV_REG_X0 + i as u32, v)?;
        }

        // Set the virtual-timer offset FIRST so the counter is continuous across
        // the process boundary: at capture CNTVCT == mono - vtimer_offset; pick a
        // new offset so CNTVCT reads that same value now, then advances. The
        // guest's CNTV_CVAL is an absolute value on this timeline, so it is only
        // meaningful once the offset is right — hence the ordering (setting CVAL
        // against the default offset first is what wedged the timer).
        #[allow(deprecated)]
        let now = unsafe { libc::mach_absolute_time() };
        let captured_cntvct = cpu.mono.wrapping_sub(cpu.vtimer_offset);
        let new_offset = now.wrapping_sub(captured_cntvct);
        unsafe {
            check(hv_vcpu_set_vtimer_offset(self.handle, new_offset), "set vtimer offset")?;
            check(hv_vcpu_set_vtimer_mask(self.handle, false), "clear vtimer mask")?;
        }

        // System registers: set each captured one. Read-only ID/feature regs
        // refuse and are skipped. The virtual-timer compare/control
        // (CNTV_CVAL/CNTV_CTL) is the subtle one:
        //  - vGIC mode: do NOT write them. Pinning the stale compare value on a
        //    fresh timeline wedges HVF's internal timer (stale CVAL fires
        //    continuously -> busy-spin; a fresh one never fires), and HVF owns the
        //    timer→GIC delivery anyway, so the periodic tick does not resume — the
        //    known HVF gap that motivated the software GIC.
        //  - swgic mode: DO write them. We keep HVF's vtimer masked and deliver the
        //    timer ourselves by polling CNTV each entry, so the guest's compare
        //    value must be restored for the deadline to be read correctly. This is
        //    what makes the periodic tick survive a restore here.
        let restore_cntv = self.swgic.is_some();
        for &(id, v) in &cpu.sysregs {
            if !restore_cntv && (id == HV_SYS_REG_CNTV_CTL_EL0 || id == HV_SYS_REG_CNTV_CVAL_EL0) {
                continue;
            }
            unsafe { hv_vcpu_set_sys_reg(self.handle, id, v) };
        }

        // SIMD/FP file (V0..V31) is captured but not yet restored: setting it
        // needs a NEON-vector-by-value call stable Rust can't make over FFI. It is
        // don't-care for the resume proof (the shell uses no FP).

        self.set_reg(HV_REG_FPCR, cpu.fpcr)?;
        self.set_reg(HV_REG_FPSR, cpu.fpsr)?;
        self.set_reg(HV_REG_CPSR, cpu.cpsr)?;
        self.set_reg(HV_REG_PC, cpu.pc)?;
        Ok(())
    }

    fn run(&mut self) -> Result<VmExit> {
        // Loop, not recurse: HVC (PSCI) and trapped-sysreg exits are handled
        // here and resumed in place, and a busy boot does enough of them that
        // recursion would grow the stack without bound. Only MMIO/Idle/Shutdown/
        // Fault leave the loop and reach the backend-agnostic run loop in vm.rs.
        loop {
            self.swgic_prep()?;
            unsafe { check(hv_vcpu_run(self.handle), "hv_vcpu_run")? };
            let exit = unsafe { *self.exit };

            match exit.reason {
                HV_EXIT_REASON_EXCEPTION => {
                    let esr = exit.exception.syndrome;
                    let ipa = exit.exception.physical_address;
                    let ec = (esr >> 26) & 0x3f;

                    if log::log_enabled!(log::Level::Debug) {
                        let pc = self.pc().unwrap_or(0);
                        let cpsr = self.reg(HV_REG_CPSR).unwrap_or(0);
                        let far = exit.exception.virtual_address;
                        let el = (cpsr >> 2) & 0b11;
                        log::debug!(
                            "exit EXCEPTION ec={ec:#x} esr={esr:#x} pc={pc:#x} ipa={ipa:#x} far={far:#x} cpsr={cpsr:#x} EL{el}"
                        );
                    }

                    if ec == EC_WFX {
                        // Guest WFI/WFE: surface as Idle so the run loop can park
                        // until the timer is due or console input arrives, then
                        // resume. The deadline comes from `pending_timer_ns`.
                        // In swgic mode WFI really traps here (no in-kernel vGIC),
                        // and ELR points AT the WFI, so step past it or the guest
                        // re-executes the same WFI on resume.
                        if self.swgic.is_some() {
                            let pc = self.pc()?;
                            self.set_pc(pc + 4)?;
                        }
                        return Ok(VmExit::Idle);
                    }

                    if ec == EC_HVC64 {
                        // PSCI over the SMC Calling Convention arrives as HVC. HVF
                        // reports PC already advanced past the HVC (ELR = next
                        // insn), so we do NOT step it; doing so would skip the
                        // instruction that loads the result pointer and fault the
                        // guest. Service the call, write x0, resume.
                        let func = self.get_x(0)?;
                        match func {
                            // SYSTEM_OFF / SYSTEM_RESET: a clean exit for M0.
                            0x8400_0008 | 0x8400_0009 => return Ok(VmExit::Shutdown),
                            // PSCI_VERSION -> 1.0 (major in [31:16], minor in [15:0]).
                            0x8400_0000 => self.set_x(0, 0x0001_0000)?,
                            // FEATURES, MIGRATE_INFO, CPU_ON on a single-vcpu M0.
                            _ => self.set_x(0, (-1i64) as u64)?,
                        }
                        continue;
                    }

                    if ec == EC_MSR_TRAP {
                        // A system register HVF traps but amber does not model yet
                        // (timer, GIC sysreg interface, assorted feature regs). M0
                        // has no interrupt controller, so the honest thing is to
                        // make these inert and keep booting until the guest WFIs
                        // for a timer tick that will never come: reads yield 0,
                        // writes are dropped. Unlike HVC, ELR points AT the
                        // trapping instruction, so we step past it ourselves.
                        let direction_read = esr & 1 == 1;
                        let rt = ((esr >> 5) & 0x1f) as u8;
                        if direction_read && rt != 31 {
                            self.set_x(rt, 0)?;
                        }
                        let pc = self.pc()?;
                        self.set_pc(pc + 4)?;
                        continue;
                    }

                    let get = |i: u8| self.get_x(i).unwrap_or(0);
                    if let Some(access) = decode_data_abort(esr, ipa, get) {
                        // GICD/GICC accesses are handled in-process by the software
                        // GIC and resumed; everything else is a device the run loop
                        // models, so surface it.
                        if self.swgic_mmio(&access)? {
                            let pc = self.pc()?;
                            self.set_pc(pc + 4)?;
                            continue;
                        }
                        return Ok(VmExit::Mmio { access });
                    }
                    let pc = self.pc().unwrap_or(0);
                    return Ok(VmExit::Fault { pc, esr, ipa });
                }
                HV_EXIT_REASON_VTIMER_ACTIVATED => {
                    // swgic keeps the vtimer masked and drives the timer line from
                    // CNTV itself, so this should not fire; re-mask if it does.
                    // With the in-kernel vGIC the timer is wired internally and
                    // this is likewise unexpected — clear the auto-set mask and run.
                    let mask = self.swgic.is_some();
                    unsafe { check(hv_vcpu_set_vtimer_mask(self.handle, mask), "vtimer mask")? };
                    continue;
                }
                HV_EXIT_REASON_CANCELED => return Ok(VmExit::Idle),
                _ => {
                    let pc = self.pc().unwrap_or(0);
                    return Ok(VmExit::Fault { pc, esr: 0, ipa: 0 });
                }
            }
        }
    }
}

impl Drop for HvfVcpu {
    fn drop(&mut self) {
        unsafe {
            hv_vcpu_destroy(self.handle);
        }
    }
}
