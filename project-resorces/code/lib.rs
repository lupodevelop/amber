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
        // x0 = DTB, x1..x3 = 0, PC = kernel entry. HVF starts the vcpu at EL1
        // with interrupts masked, which is what the arm64 boot protocol wants.
        self.set_reg(HV_REG_X0, dtb)?;
        self.set_reg(HV_REG_X0 + 1, 0)?;
        self.set_reg(HV_REG_X0 + 2, 0)?;
        self.set_reg(HV_REG_X0 + 3, 0)?;
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
        unsafe { check(hv_vcpu_run(self.handle), "hv_vcpu_run")? };
        let exit = unsafe { *self.exit };

        match exit.reason {
            HV_EXIT_REASON_EXCEPTION => {
                let esr = exit.exception.syndrome;
                let ipa = exit.exception.physical_address;
                let ec = (esr >> 26) & 0x3f;

                if ec == EC_HVC64 {
                    // PSCI arrives as HVC. SYSTEM_OFF (func id 0x8400_0008) is the
                    // one we care about for a clean exit; treat the rest as noop.
                    let func = self.get_x(0)?;
                    if func == 0x8400_0008 {
                        return Ok(VmExit::Shutdown);
                    }
                    // Unimplemented PSCI call: return NOT_SUPPORTED and continue.
                    self.set_x(0, (-1i64) as u64)?;
                    // HVC does not auto-advance PC on HVF; step past it.
                    let pc = self.pc()?;
                    self.set_pc(pc + 4)?;
                    // Re-run by reporting Idle-free: recurse once.
                    return self.run();
                }

                let get = |i: u8| self.get_x(i).unwrap_or(0);
                if let Some(access) = decode_data_abort(esr, ipa, get) {
                    return Ok(VmExit::Mmio { access });
                }
                let pc = self.pc().unwrap_or(0);
                Ok(VmExit::Fault { pc, esr, ipa })
            }
            HV_EXIT_REASON_VTIMER_ACTIVATED => Ok(VmExit::Idle),
            HV_EXIT_REASON_CANCELED => Ok(VmExit::Idle),
            _ => {
                let pc = self.pc().unwrap_or(0);
                Ok(VmExit::Fault { pc, esr: 0, ipa: 0 })
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
