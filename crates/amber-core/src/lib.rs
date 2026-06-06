//! `amber-core`: the backend-agnostic microVM. It boots an arm64 Linux `Image`
//! behind a hypervisor boundary and talks to it through a tiny device model. It
//! names no hypervisor: a backend (amber-hvf, later amber-kvm) implements the
//! [`hypervisor::Hypervisor`] and [`hypervisor::Vcpu`] traits and nothing else.
//!
//! This is the M0 core. The flesh (virtio devices, image layer, snapshot, the
//! budget scheduler) is added in later milestones, all above this seam.

pub mod bus;
pub mod dtb;
pub mod hypervisor;
pub mod loader;
pub mod memory;
mod vm;

pub use hypervisor::{Hypervisor, Vcpu};
pub use memory::GuestMemory;
pub use vm::{Vm, VmConfig};

/// Where a backend placed its interrupt controller, so the device tree can
/// advertise the matching addresses. Today only GICv3 (HVF now, KVM at M8). The
/// sizes come from the host, not baked constants, because the host decides them.
#[derive(Debug, Clone, Copy)]
pub struct GicInfo {
    pub dist_base: u64,
    pub dist_size: u64,
    pub redist_base: u64,
    pub redist_size: u64,
}

/// The guest-physical memory map. The QEMU `virt` layout, which is the reference
/// every arm64 microVM follows and what the kernel's `earlycon` expects. Devices
/// sit below RAM; RAM starts at 1 GiB.
pub mod layout {
    /// Guest RAM base. 2 MiB-aligned, so a kernel placed at `KERNEL_OFFSET` above
    /// it is also 2 MiB-aligned as the arm64 boot protocol requires.
    pub const RAM_BASE: u64 = 0x4000_0000;

    /// Kernel load offset within RAM. 2 MiB, leaving the first 2 MiB of RAM for
    /// the DTB. The kernel reserves its own FDT region early, before it allocates
    /// downward, so the DTB sitting below the kernel is safe.
    pub const KERNEL_OFFSET: u64 = 0x0020_0000;

    /// DTB offset within RAM. 1 MiB: 8-byte aligned, inside the first 512 MiB the
    /// boot protocol requires, and clear of the kernel at `KERNEL_OFFSET`.
    pub const DTB_OFFSET: u64 = 0x0010_0000;

    /// GICv2 distributor. Present in the DTB for a well-formed tree; not backed
    /// until M0.5.
    pub const GIC_DIST_BASE: u64 = 0x0800_0000;
    pub const GIC_DIST_SIZE: u64 = 0x0001_0000;
    /// GICv2 cpu interface.
    pub const GIC_CPU_BASE: u64 = 0x0801_0000;
    pub const GIC_CPU_SIZE: u64 = 0x0001_0000;

    /// PL011 UART. The earlycon the kernel prints through before any interrupt
    /// controller is alive: `earlycon=pl011,0x9000000`.
    pub const PL011_BASE: u64 = 0x0900_0000;
    pub const PL011_SIZE: u64 = 0x0000_1000;

    /// GICv3 shared peripheral interrupts start at INTID 32. The PL011 is wired
    /// to SPI 1, so its absolute INTID is `GIC_SPI_BASE + 1`. The DTB advertises
    /// the relative number (1); the backend injects the absolute INTID.
    pub const GIC_SPI_BASE: u32 = 32;
    pub const PL011_SPI: u32 = 1;
}

/// Everything that can go wrong below the seam. Structured, not stringly-typed at
/// the boundaries that a caller branches on (`GuestFault` carries the registers a
/// debugger wants); the rest carry a message because there is nothing to branch on.
#[derive(Debug)]
pub enum Error {
    /// A backend (HVF/KVM) call failed. The string names the call and the code.
    Backend(String),
    /// `mmap`/`munmap` of guest RAM failed.
    Mmap(std::io::Error),
    /// Loading the kernel `Image` or initramfs into guest RAM failed.
    Loader(String),
    /// Building the flattened device tree failed.
    Fdt(String),
    /// The guest took an exit the run loop cannot handle. Fatal to the VM. The
    /// registers are what you need to find the offending instruction.
    GuestFault { pc: u64, esr: u64, ipa: u64 },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Backend(m) => write!(f, "backend error: {m}"),
            Error::Mmap(e) => write!(f, "guest memory mmap failed: {e}"),
            Error::Loader(m) => write!(f, "kernel load failed: {m}"),
            Error::Fdt(m) => write!(f, "device tree build failed: {m}"),
            Error::GuestFault { pc, esr, ipa } => write!(
                f,
                "unhandled guest fault: pc={pc:#x} esr={esr:#x} ipa={ipa:#x}"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Mmap(e) => Some(e),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
