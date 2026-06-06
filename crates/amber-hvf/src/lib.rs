//! The Apple Silicon backend. Implements the two traits from amber-core and
//! nothing else. Everything hard about it is concentrated in `run`: turning a
//! raw HVF exit into the shared `VmExit` vocabulary.

mod ffi;

use amber_core::hypervisor::{decode_data_abort, Hypervisor, Vcpu, VmExit};
use amber_core::{Error, GuestMemory, Result};
use ffi::*;

fn check(ret: hv_return_t, what: &str) -> Result<()> {
    if ret == HV_SUCCESS {
        Ok(())
    } else {
        Err(Error::Backend(format!("{what} failed: {ret:#x}")))
    }
}

pub struct HvfVm;

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
        Ok(HvfVm)
    }

    fn create_vcpu(&mut self, _id: u8) -> Result<HvfVcpu> {
        let mut handle: hv_vcpu_t = 0;
        let mut exit: *mut hv_vcpu_exit_t = std::ptr::null_mut();
        unsafe {
            check(
                hv_vcpu_create(&mut handle, &mut exit, std::ptr::null_mut()),
                "hv_vcpu_create",
            )?;
        }
        Ok(HvfVcpu { handle, exit })
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
                        // Guest WFI/WFE. HVF surfaces it as an exception, not a
                        // CANCELED/VTIMER idle exit. With no GIC there is no
                        // interrupt to wake it, so this is the M0 idle ceiling.
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
                        return Ok(VmExit::Mmio { access });
                    }
                    let pc = self.pc().unwrap_or(0);
                    return Ok(VmExit::Fault { pc, esr, ipa });
                }
                HV_EXIT_REASON_VTIMER_ACTIVATED => return Ok(VmExit::Idle),
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
