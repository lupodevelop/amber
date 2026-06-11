//! The seam. The only thing a hypervisor backend implements.
//!
//! Both targets are arm64, so the two backends differ in exactly two places:
//! how guest memory is mapped, and how a vcpu is run and its exit decoded.
//! Everything else is shared code above this file.

use crate::memory::GuestMemory;
use crate::Result;

/// A configured but not-yet-running VM, owned by a backend.
pub trait Hypervisor: Sized {
    type Vcpu: Vcpu;

    /// Create the VM for `vcpus` guest CPUs and map `mem` into guest-physical
    /// space at its base. The count sizes the interrupt controller.
    fn create(mem: &GuestMemory, vcpus: usize) -> Result<Self>;

    /// Create vcpu `id`. Takes `&self`: each vcpu is created on the OS thread
    /// that will run it (HVF binds a vcpu to its creating thread), and secondary
    /// vcpu threads hold only a shared reference to the VM.
    fn create_vcpu(&self, id: u8) -> Result<Self::Vcpu>;

    /// The interrupt controller the backend created, if any, so the device tree
    /// can describe it. A backend with no GIC returns None and the guest boots
    /// without functional interrupts (the M0 ceiling).
    fn gic_info(&self) -> Option<crate::GicInfo> {
        None
    }

    /// Set the level of a shared peripheral interrupt (SPI) by its GIC INTID.
    /// Called by the run loop when a device's line changes, e.g. the PL011 when
    /// its receive FIFO fills or drains. No-op on a backend without a GIC.
    fn set_irq(&self, _intid: u32, _level: bool) -> Result<()> {
        Ok(())
    }

    /// Capture the interrupt controller's state as an opaque blob for a snapshot.
    fn capture_gic(&self) -> Result<Vec<u8>> {
        Err(crate::Error::Snapshot("backend has no GIC capture".into()))
    }

    /// Restore the interrupt controller from a captured blob (after `create`).
    fn restore_gic(&self, _blob: &[u8]) -> Result<()> {
        Err(crate::Error::Snapshot("backend has no GIC restore".into()))
    }

    /// Ask every vcpu to leave its `run` loop promptly (the VM is going down).
    /// The run loop calls it once after the primary stops, so secondary vcpu
    /// threads blocked in guest execution return and can be joined.
    fn request_stop(&self) {}

    /// While on, every vcpu returns from `run` (as Idle) at its next forced exit
    /// instead of resuming in place — so the run loop's gates (snapshot quiesce,
    /// pause) are observed even by compute-bound vcpus that never exit on their
    /// own. Off restores normal resume-in-place behavior.
    fn set_yield(&self, _on: bool) {}
}

pub trait Vcpu {
    /// arm64 boot protocol: PC at the kernel entry, x0 at the DTB address,
    /// x1..x3 zero, started at EL1 with interrupts masked. The backend sets the
    /// architectural defaults; this call sets the two values amber controls.
    fn set_boot_regs(&mut self, entry: u64, dtb: u64) -> Result<()>;

    /// Read a general register x0..x30 by index. Needed to pull the value of an
    /// MMIO store out of the source register the faulting instruction used.
    fn get_x(&self, idx: u8) -> Result<u64>;

    /// Write a general register x0..x30 by index. Needed to deliver the result of
    /// an MMIO load into the destination register before resuming.
    fn set_x(&mut self, idx: u8, val: u64) -> Result<()>;

    /// Read the program counter.
    fn pc(&self) -> Result<u64>;

    /// Set the program counter. Used to step past a faulting MMIO instruction.
    fn set_pc(&mut self, pc: u64) -> Result<()>;

    /// Run until the next exit. Synchronous. One OS thread per vcpu, so this call
    /// blocks that thread and nothing async ever touches the hot path.
    fn run(&mut self) -> Result<VmExit>;

    /// Deliver the value a device computed for the last MMIO read. HVF writes it
    /// into the syndrome's transfer register; KVM into the exit's data buffer.
    fn complete_mmio_read(&mut self, value: u64) -> Result<()>;

    /// Advance past the faulting MMIO instruction. HVF steps PC by 4; KVM already
    /// advanced it, so this is a no-op there.
    fn advance_mmio(&mut self) -> Result<()>;

    /// Nanoseconds until the guest's virtual timer is next due, for the run loop
    /// to bound how long it parks on a WFI. `Some(0)` means already due, `None`
    /// means no armed timer (park until another event, e.g. console input).
    fn pending_timer_ns(&self) -> Result<Option<u64>> {
        Ok(None)
    }

    /// Capture this vcpu's register state for a snapshot (taken while stopped).
    fn capture(&self) -> Result<crate::snapshot::CpuSnapshot> {
        Err(crate::Error::Snapshot("backend has no vcpu capture".into()))
    }

    /// Restore this vcpu's register state from a snapshot, in place of booting.
    fn restore(&mut self, _cpu: &crate::snapshot::CpuSnapshot) -> Result<()> {
        Err(crate::Error::Snapshot("backend has no vcpu restore".into()))
    }
}

/// The shared exit vocabulary. Each backend translates its raw exit into one of
/// these so the run loop in `vm.rs` is backend-independent.
#[derive(Debug, Clone, Copy)]
pub enum VmExit {
    /// A trapped access to an unbacked guest-physical address: a device register.
    Mmio { access: MmioAccess },
    /// WFI/WFE: the guest has nothing to do. Park or wake on an event.
    Idle,
    /// PSCI SYSTEM_OFF (or equivalent). Clean shutdown.
    Shutdown,
    /// Anything the backend could not classify. The run loop treats it as fatal.
    Fault { pc: u64, esr: u64, ipa: u64 },
}

#[derive(Debug, Clone, Copy)]
pub struct MmioAccess {
    pub ipa: u64,
    /// Access width in bytes: 1, 2, 4, or 8.
    pub size: u8,
    /// `Some(value)` for a guest write; `None` for a read (the device computes a
    /// value, which the run loop hands back via [`Vcpu::complete_mmio_read`]).
    /// Backend-neutral: the destination register is HVF-internal (KVM hides it).
    pub write: Option<u64>,
}

/// ESR_EL2 data-abort decoding, shared by both backends. On a data abort the
/// backend hands us the raw syndrome and the faulting IPA; we turn it into a
/// `MmioAccess`. Returns None if the instruction syndrome is not valid (ISV=0),
/// which means we cannot emulate it from the syndrome alone.
///
/// ESR layout: EC = bits[31:26], ISS = bits[24:0].
/// Data abort from a lower EL: EC == 0x24.
/// ISS for a data abort: ISV[24], SAS[23:22], SSE[21], SRT[20:16], WnR[6].
/// Returns the access plus the syndrome's transfer register (`srt`), which the
/// HVF backend stashes to deliver a read result / source a write value. KVM does
/// not use this path (its MMIO exit carries the data directly).
pub fn decode_data_abort(esr: u64, ipa: u64, get_x: impl Fn(u8) -> u64) -> Option<(MmioAccess, u8)> {
    const EC_DATA_ABORT_LOWER_EL: u64 = 0x24;
    let ec = (esr >> 26) & 0x3f;
    if ec != EC_DATA_ABORT_LOWER_EL {
        return None;
    }
    let iss = esr & 0x1ff_ffff;
    let isv = (iss >> 24) & 1;
    if isv == 0 {
        return None; // syndrome not usable; would need instruction decode
    }
    let sas = (iss >> 22) & 0b11;
    let size = 1u8 << sas; // 0->1, 1->2, 2->4, 3->8 bytes
    let srt = ((iss >> 16) & 0x1f) as u8;
    let wnr = (iss >> 6) & 1;

    let write = if wnr == 1 { Some(get_x(srt)) } else { None };
    Some((MmioAccess { ipa, size, write }, srt))
}
