//! The Apple Silicon backend. Implements the two traits from amber-core and
//! nothing else. Everything hard about it is concentrated in `run`: turning a
//! raw HVF exit into the shared `VmExit` vocabulary.

mod ffi;

use amber_core::hypervisor::{decode_data_abort, Hypervisor, Vcpu, VmExit};
use amber_core::{Error, GicInfo, GuestMemory, Result};
use ffi::*;

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
        let gic = Self::create_gic()?;
        Ok(HvfVm { gic })
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
        Ok(HvfVcpu { handle, exit })
    }

    fn gic_info(&self) -> Option<GicInfo> {
        Some(self.gic)
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

        log::info!(
            "GICv3: dist {:#x}+{:#x}, redist {:#x}+{:#x}",
            dist_base, dist_size, redist_base, redist_size
        );
        Ok(GicInfo {
            dist_base,
            dist_size: dist_size as u64,
            redist_base,
            redist_size: redist_size as u64,
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

    /// Park the host thread on a guest WFI until the virtual timer is due, capped
    /// so the loop stays responsive. CNTV_CTL tells us whether the timer is armed
    /// and whether it has already fired; CNTV_CVAL is the deadline in the same
    /// mach-timebase units as `mach_absolute_time() - vtimer_offset` (= CNTVCT).
    #[allow(deprecated)] // libc's mach_* are fine here; mach2 is the only alternative
    fn park_on_wfi(&self) -> Result<()> {
        const CAP_NS: u64 = 100_000_000; // 100 ms ceiling

        let ctl = self.get_sys(HV_SYS_REG_CNTV_CTL_EL0)?;
        let enabled = ctl & 0b001 != 0; // ENABLE
        let imask = ctl & 0b010 != 0; // IMASK
        let istatus = ctl & 0b100 != 0; // condition already met

        if istatus {
            // A tick is already pending; resume immediately so the GIC delivers it.
            return Ok(());
        }

        let dur_ns = if enabled && !imask {
            let cval = self.get_sys(HV_SYS_REG_CNTV_CVAL_EL0)?;
            let mut offset = 0u64;
            unsafe { check(hv_vcpu_get_vtimer_offset(self.handle, &mut offset), "vtimer offset")? };
            let now = unsafe { libc::mach_absolute_time() }.wrapping_sub(offset);
            if cval <= now {
                return Ok(());
            }
            mach_ticks_to_ns(cval - now).min(CAP_NS)
        } else {
            // No armed timer (and no other wake source yet): sleep a slice rather
            // than spin, then re-check.
            CAP_NS
        };

        let ts = libc::timespec {
            tv_sec: (dur_ns / 1_000_000_000) as libc::time_t,
            tv_nsec: (dur_ns % 1_000_000_000) as _,
        };
        unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
        Ok(())
    }
}

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

    fn run(&mut self) -> Result<VmExit> {
        // Loop, not recurse: HVC (PSCI) and trapped-sysreg exits are handled
        // here and resumed in place, and a busy boot does enough of them that
        // recursion would grow the stack without bound. Only MMIO/Idle/Shutdown/
        // Fault leave the loop and reach the backend-agnostic run loop in vm.rs.
        loop {
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
                        // Guest WFI/WFE: idle until something is due. With the
                        // vGIC the timer is internal, so we park the host thread
                        // until the virtual timer's deadline, then resume; HVF's
                        // GIC delivers the tick on the next run. Bounded so we
                        // stay responsive (and, later, can poll console input).
                        self.park_on_wfi()?;
                        continue;
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
                        return Ok(VmExit::Mmio { access });
                    }
                    let pc = self.pc().unwrap_or(0);
                    return Ok(VmExit::Fault { pc, esr, ipa });
                }
                HV_EXIT_REASON_VTIMER_ACTIVATED => {
                    // With the vGIC the timer is wired internally and this exit
                    // should not occur; if it does, clear the mask HVF auto-set
                    // and keep running rather than treating it as idle.
                    unsafe { check(hv_vcpu_set_vtimer_mask(self.handle, false), "vtimer unmask")? };
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
