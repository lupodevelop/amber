//! The backend-agnostic VM: load the kernel and DTB, wire the bus, create the
//! vcpu, and run the exit loop. This is the file that proves the seam works:
//! it names no hypervisor, only the `Hypervisor`/`Vcpu` traits.

use crate::bus::{MmioBus, Pl011};
use crate::hypervisor::{Hypervisor, MmioOp, Vcpu, VmExit};
use crate::{dtb, layout, loader, GuestMemory, Result};

pub struct VmConfig {
    pub mem_size: usize,
    pub kernel: Vec<u8>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            mem_size: 512 * 1024 * 1024,
            kernel: Vec::new(),
            initrd: None,
            // earlycon prints before the GIC exists, which is the whole M0 trick.
            cmdline: "earlycon=pl011,0x9000000 console=ttyAMA0 panic=-1".into(),
        }
    }
}

pub struct Vm {
    mem: GuestMemory,
    bus: MmioBus,
    entry: u64,
    dtb_addr: u64,
}

impl Vm {
    /// Place the kernel, initramfs, and DTB in guest RAM and build the device bus.
    pub fn prepare(cfg: &VmConfig) -> Result<Self> {
        let mem = GuestMemory::new(layout::RAM_BASE, cfg.mem_size)?;

        let kernel = loader::load_kernel(&mem, &cfg.kernel)?;
        let initrd = loader::load_initramfs(&mem, cfg.initrd.as_deref())?;

        let blob = dtb::build(&dtb::DtbParams {
            mem_size: cfg.mem_size as u64,
            cmdline: &cfg.cmdline,
            initrd,
        })?;
        let dtb_addr = mem.base() + layout::DTB_OFFSET;
        mem.write(dtb_addr, &blob)?;

        let mut bus = MmioBus::default();
        Pl011::register_on(&mut bus, Box::new(std::io::stdout()));

        Ok(Self { mem, bus, entry: kernel.entry, dtb_addr })
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.mem
    }

    /// Boot and run on backend `H` until shutdown or a fatal fault. Single vcpu
    /// for M0, so this runs on the calling thread.
    pub fn run<H: Hypervisor>(mut self) -> Result<()> {
        let mut hv = H::create(&self.mem)?;
        let mut vcpu = hv.create_vcpu(0)?;
        vcpu.set_boot_regs(self.entry, self.dtb_addr)?;

        loop {
            match vcpu.run()? {
                VmExit::Mmio { access } => {
                    match access.op {
                        MmioOp::Write { value, .. } => {
                            self.bus.write(access.ipa, access.size, value);
                        }
                        MmioOp::Read { reg } => {
                            let v = self.bus.read(access.ipa, access.size);
                            vcpu.set_x(reg, v)?;
                        }
                    }
                    // Step past the faulting load/store: arm64 instructions are
                    // 4 bytes, and a syndrome-decodable access is a single insn.
                    let pc = vcpu.pc()?;
                    vcpu.set_pc(pc + 4)?;
                }
                VmExit::Idle => {
                    // M0 has no interrupt source, so a WFI with nothing pending
                    // means early boot has gone as far as it can without a GIC.
                    log::info!("guest idle (WFI); M0 reached its ceiling without a GIC");
                    return Ok(());
                }
                VmExit::Shutdown => {
                    log::info!("guest requested shutdown");
                    return Ok(());
                }
                VmExit::Fault { pc, esr, ipa } => {
                    return Err(crate::Error::GuestFault { pc, esr, ipa });
                }
            }
        }
    }
}
