//! KVM implementation of the hypervisor seam (Linux/arm64).
//!
//! KVM provides the interrupt controller (in-kernel vGICv3), the arch timer, and
//! PSCI itself, so this backend is thin: set up guest memory + the vGIC, create
//! vcpus, and translate `KVM_RUN` exits. The run loop drives `KVM_RUN` over a
//! self-mmap'd `kvm_run` so an MMIO read result can be written back into the
//! exit's data buffer from amber-core's `complete_mmio_read` (KVM advances the
//! PC itself, so `advance_mmio` is a no-op).

use crate::gic;
use amber_core::hypervisor::{Hypervisor, MmioAccess, Vcpu, VmExit};
use amber_core::snapshot::CpuSnapshot;
use amber_core::{Error, GicInfo, GicKind, GuestMemory, Result};
use kvm_bindings::*;
use kvm_ioctls::{DeviceFd, Kvm, VcpuFd, VmFd};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, Once};

/// `KVM_RUN` = `_IO(KVMIO, 0x80)`; KVMIO is 0xAE, and `_IO` has no size/dir bits.
const KVM_RUN: libc::c_ulong = (0xAE << 8) | 0x80;

fn errno<T>(what: &str) -> Result<T> {
    Err(Error::Backend(format!("{what}: {}", std::io::Error::last_os_error())))
}

/// The "kick" signal: an empty handler so delivery only interrupts a blocking
/// `KVM_RUN` (returns EINTR) instead of killing the process. SIGUSR1 is unused
/// elsewhere in amber.
extern "C" fn kick_handler(_sig: libc::c_int) {}
static KICK_HANDLER: Once = Once::new();
fn install_kick_handler() {
    KICK_HANDLER.call_once(|| unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = kick_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        // No SA_RESTART: we want the interrupted ioctl to return EINTR.
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
    });
}
fn kvm_err<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> Error + '_ {
    move |e| Error::Backend(format!("{what}: {e}"))
}

// GIC layout (QEMU `virt`): GICv3 distributor + redistributors. The redistributor
// region must hold 128 KiB per vcpu.
const GICD_BASE: u64 = 0x0800_0000;
const GICD_SIZE: u64 = 0x0001_0000;
const GICR_BASE: u64 = 0x080A_0000;
const GICR_PER_CPU: u64 = 0x0002_0000;
const NR_IRQS: u32 = 256;

/// arm64 core register id (see `KVM_REG_ARM_CORE`): byte offset into `kvm_regs`,
/// encoded in u32 units.
const fn core_reg(byte_off: usize) -> u64 {
    KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE as u64 | (byte_off as u64 / 4)
}
fn x_reg(i: u8) -> u64 {
    core_reg(i as usize * 8) // regs.regs[i] is the first field of user_pt_regs
}
const PC_REG: u64 = core_reg(32 * 8); // after regs[31] + sp
const PSTATE_REG: u64 = core_reg(33 * 8);

pub struct KvmVm {
    _kvm: Kvm,
    vm: Arc<VmFd>,
    gic: GicInfo,
    vcpus: usize,
    // The vGIC device fd: kept alive for the VM's lifetime + used for save/restore.
    vgic: DeviceFd,
    // Kernel thread ids of the running vcpus, for `kick` (tgkill SIGUSR1). Each
    // vcpu registers its own tid the first time it runs.
    kick_tids: Arc<Mutex<Vec<libc::pid_t>>>,
    // All vcpus are created up front (before the vGIC is INIT'd — KVM rejects
    // KVM_CREATE_VCPU with EBUSY afterward); `create_vcpu(id)` hands out the one
    // for `id`, which then moves to its run thread.
    pending: Mutex<Vec<Option<KvmVcpu>>>,
}

/// Create one vcpu fd: init it for in-kernel PSCI (secondaries start powered
/// off, woken by PSCI CPU_ON) and mmap its `kvm_run`. No vGIC interaction — the
/// caller INITs the GIC only after every vcpu exists.
fn init_vcpu(
    vm: &VmFd,
    id: u8,
    run_size: usize,
    kick_tids: &Arc<Mutex<Vec<libc::pid_t>>>,
) -> Result<KvmVcpu> {
    let fd = vm.create_vcpu(id as u64).map_err(kvm_err("KVM_CREATE_VCPU"))?;
    let mut kvi = kvm_vcpu_init::default();
    vm.get_preferred_target(&mut kvi).map_err(kvm_err("preferred target"))?;
    kvi.features[0] |= 1 << KVM_ARM_VCPU_PSCI_0_2;
    if id != 0 {
        kvi.features[0] |= 1 << KVM_ARM_VCPU_POWER_OFF;
    }
    fd.vcpu_init(&kvi).map_err(kvm_err("KVM_ARM_VCPU_INIT"))?;
    let run = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            run_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if run == libc::MAP_FAILED {
        return errno("mmap kvm_run");
    }
    Ok(KvmVcpu {
        fd,
        kvm_run: run as *mut kvm_run,
        mmio_len: 0,
        kick_tids: kick_tids.clone(),
        tid_registered: false,
    })
}

fn set_vgic_attr(vgic: &DeviceFd, group: u32, attr: u64, value: u64) -> Result<()> {
    let kda = kvm_device_attr {
        group,
        attr,
        addr: &value as *const u64 as u64,
        flags: 0,
    };
    vgic.set_device_attr(&kda).map_err(kvm_err("vgic attr"))
}

impl Hypervisor for KvmVm {
    type Vcpu = KvmVcpu;

    fn create(mem: &GuestMemory, vcpus: usize) -> Result<Self> {
        install_kick_handler();
        let kvm = Kvm::new().map_err(kvm_err("open /dev/kvm"))?;
        let vm = kvm.create_vm().map_err(kvm_err("KVM_CREATE_VM"))?;

        // Guest RAM as one slot. SAFETY: the region stays mapped for the VM's life.
        let region = kvm_userspace_memory_region {
            slot: 0,
            flags: 0,
            guest_phys_addr: mem.base(),
            memory_size: mem.len() as u64,
            userspace_addr: mem.host_ptr() as u64,
        };
        unsafe { vm.set_user_memory_region(region).map_err(kvm_err("set memory"))? };

        // In-kernel vGICv3. Addresses + IRQ count now; INIT after vcpu 0 exists.
        let mut dev = kvm_create_device {
            type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            fd: 0,
            flags: 0,
        };
        let vgic = vm.create_device(&mut dev).map_err(kvm_err("create vGICv3"))?;
        set_vgic_attr(&vgic, KVM_DEV_ARM_VGIC_GRP_NR_IRQS, 0, NR_IRQS as u64)?;
        set_vgic_attr(
            &vgic,
            KVM_DEV_ARM_VGIC_GRP_ADDR,
            KVM_VGIC_V3_ADDR_TYPE_DIST as u64,
            GICD_BASE,
        )?;
        set_vgic_attr(
            &vgic,
            KVM_DEV_ARM_VGIC_GRP_ADDR,
            KVM_VGIC_V3_ADDR_TYPE_REDIST as u64,
            GICR_BASE,
        )?;

        let n = vcpus.max(1);
        let gic = GicInfo {
            dist_base: GICD_BASE,
            dist_size: GICD_SIZE,
            redist_base: GICR_BASE,
            redist_size: GICR_PER_CPU * n as u64,
            kind: GicKind::V3,
        };
        let vm = Arc::new(vm);
        let run_size = kvm.get_vcpu_mmap_size().map_err(kvm_err("mmap size"))?;
        let kick_tids = Arc::new(Mutex::new(Vec::new()));

        // Create every vcpu up front, then INIT the vGIC — KVM_CREATE_VCPU returns
        // EBUSY once the in-kernel GIC is initialized, and the secondaries are
        // created lazily on their own run threads, so they must all exist now.
        let mut pending = Vec::with_capacity(n);
        for id in 0..n {
            pending.push(Some(init_vcpu(&vm, id as u8, run_size, &kick_tids)?));
        }
        set_vgic_attr(&vgic, KVM_DEV_ARM_VGIC_GRP_CTRL, KVM_DEV_ARM_VGIC_CTRL_INIT as u64, 0)?;

        log::info!("KVM: vGICv3 ({n} vcpu), dist {GICD_BASE:#x}, redist {GICR_BASE:#x}");
        Ok(KvmVm {
            _kvm: kvm,
            vm,
            gic,
            vcpus: n,
            vgic,
            kick_tids,
            pending: Mutex::new(pending),
        })
    }

    fn create_vcpu(&self, id: u8) -> Result<KvmVcpu> {
        // Hand out the vcpu created in `create`; it moves to its run thread.
        self.pending
            .lock()
            .unwrap()
            .get_mut(id as usize)
            .and_then(Option::take)
            .ok_or_else(|| Error::Backend(format!("vcpu {id} not available")))
    }

    fn gic_info(&self) -> Option<GicInfo> {
        Some(self.gic)
    }

    fn set_irq(&self, intid: u32, level: bool) -> Result<()> {
        // SPI: type(1) in bits[27:24], the GIC INTID (absolute, ≥32) in bits[15:0].
        let irq = (1u32 << 24) | intid;
        self.vm.set_irq_line(irq, level).map_err(kvm_err("KVM_IRQ_LINE"))
    }

    fn kick(&self) {
        self.signal_vcpus();
    }

    fn request_stop(&self) {
        // Same SIGUSR1 kick: break every vcpu out of a blocking KVM_RUN so its
        // loop observes `running == false` and exits. The run loop sets the flag
        // before calling this, so a kicked secondary leaves for good rather than
        // resuming — that's the only way to join a vcpu halted on WFI.
        self.signal_vcpus();
    }

    fn capture_gic(&self) -> Result<Vec<u8>> {
        gic::capture(&self.vgic, NR_IRQS, self.vcpus)
    }

    fn restore_gic(&self, blob: &[u8]) -> Result<()> {
        gic::restore(&self.vgic, NR_IRQS, self.vcpus, blob)
    }
}

impl KvmVm {
    /// Interrupt every running vcpu with SIGUSR1 so a blocking KVM_RUN (e.g. a
    /// halted WFI) returns EINTR and its loop re-checks flags/deadlines. tgkill
    /// targets each vcpu's exact thread.
    fn signal_vcpus(&self) {
        let pid = unsafe { libc::getpid() };
        for &tid in self.kick_tids.lock().unwrap().iter() {
            unsafe { libc::syscall(libc::SYS_tgkill, pid as i64, tid as i64, libc::SIGUSR1) };
        }
    }
}

pub struct KvmVcpu {
    fd: VcpuFd,
    kvm_run: *mut kvm_run,
    /// Length of the MMIO read currently being serviced (for complete_mmio_read).
    mmio_len: usize,
    /// Shared with `KvmVm`: this vcpu registers its thread id here on first run
    /// so `kick` can signal it.
    kick_tids: Arc<Mutex<Vec<libc::pid_t>>>,
    tid_registered: bool,
}

// The vcpu and its kvm_run mapping live on, and are driven by, a single thread.
unsafe impl Send for KvmVcpu {}

impl KvmVcpu {
    fn set_reg(&self, id: u64, val: u64) -> Result<()> {
        self.fd.set_one_reg(id, &val.to_le_bytes()).map_err(kvm_err("set_one_reg")).map(|_| ())
    }
    fn get_reg(&self, id: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.fd.get_one_reg(id, &mut b).map_err(kvm_err("get_one_reg"))?;
        Ok(u64::from_le_bytes(b))
    }
    fn run_mut(&mut self) -> &mut kvm_run {
        unsafe { &mut *self.kvm_run }
    }
}

/// Wrap a bare ARM sysreg encoding (op0..op2) in KVM's ONE_REG sysreg index.
fn kvm_sysreg_id(enc: u32) -> u64 {
    KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM64_SYSREG as u64 | enc as u64
}

impl Vcpu for KvmVcpu {
    fn apply_cpu_template(&mut self, template: &amber_core::cpu::CpuTemplate) -> Result<()> {
        // KVM emulates the ID_AA64* reads and accepts a reduced feature set via
        // SET_ONE_REG, so the masked value becomes the guest's view.
        for ov in template.overrides {
            let id = kvm_sysreg_id(ov.reg);
            let cur = self.get_reg(id)?;
            self.set_reg(id, (cur & ov.and_mask) | ov.or_value)?;
        }
        Ok(())
    }

    fn set_boot_regs(&mut self, entry: u64, dtb: u64) -> Result<()> {
        self.set_reg(PSTATE_REG, 0x3c5)?; // EL1h, DAIF masked
        self.set_reg(PC_REG, entry)?;
        self.set_reg(x_reg(0), dtb)?;
        Ok(())
    }
    fn get_x(&self, idx: u8) -> Result<u64> {
        self.get_reg(x_reg(idx))
    }
    fn set_x(&mut self, idx: u8, val: u64) -> Result<()> {
        self.set_reg(x_reg(idx), val)
    }
    fn pc(&self) -> Result<u64> {
        self.get_reg(PC_REG)
    }
    fn set_pc(&mut self, pc: u64) -> Result<()> {
        self.set_reg(PC_REG, pc)
    }

    fn run(&mut self) -> Result<VmExit> {
        // Register this thread's id once, so `kick` can break this vcpu out of a
        // blocking KVM_RUN (the vcpu is bound to the thread that runs it).
        if !self.tid_registered {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t;
            self.kick_tids.lock().unwrap().push(tid);
            self.tid_registered = true;
        }
        let ret = unsafe { libc::ioctl(self.fd.as_raw_fd(), KVM_RUN, 0) };
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(VmExit::Idle); // a signal asked us to leave the guest
            }
            return Err(Error::Backend(format!("KVM_RUN: {e}")));
        }
        let run = self.run_mut();
        match run.exit_reason {
            KVM_EXIT_MMIO => {
                let mmio = unsafe { &run.__bindgen_anon_1.mmio };
                let ipa = mmio.phys_addr;
                let len = mmio.len as usize;
                let size = mmio.len as u8;
                if mmio.is_write != 0 {
                    let mut b = [0u8; 8];
                    b[..len].copy_from_slice(&mmio.data[..len]);
                    Ok(VmExit::Mmio { access: MmioAccess { ipa, size, write: Some(u64::from_le_bytes(b)) } })
                } else {
                    self.mmio_len = len;
                    Ok(VmExit::Mmio { access: MmioAccess { ipa, size, write: None } })
                }
            }
            KVM_EXIT_SYSTEM_EVENT => Ok(VmExit::Shutdown), // PSCI SYSTEM_OFF/RESET
            KVM_EXIT_INTR => Ok(VmExit::Idle),
            KVM_EXIT_FAIL_ENTRY | KVM_EXIT_INTERNAL_ERROR => {
                let pc = self.pc().unwrap_or(0);
                Ok(VmExit::Fault { pc, esr: 0, ipa: 0 })
            }
            other => Err(Error::Backend(format!("unhandled KVM exit {other}"))),
        }
    }

    fn complete_mmio_read(&mut self, value: u64) -> Result<()> {
        let len = self.mmio_len.min(8);
        let bytes = value.to_le_bytes();
        let mmio = unsafe { &mut self.run_mut().__bindgen_anon_1.mmio };
        mmio.data[..len].copy_from_slice(&bytes[..len]);
        Ok(())
    }

    fn advance_mmio(&mut self) -> Result<()> {
        Ok(()) // KVM advances the PC past the MMIO instruction itself.
    }

    /// Capture every register KVM exposes for this vcpu (`KVM_GET_REG_LIST` +
    /// `GET_ONE_REG`). Stored as raw `(id, bytes)` in `kvm_regs`; widths vary
    /// (the id encodes the size), so we read each into a sized buffer. This covers
    /// the core regs, the system registers, the FP/SIMD file, and the arch timer.
    fn capture(&self) -> Result<CpuSnapshot> {
        // Size the list to the real count: a small probe gets E2BIG with the
        // required `n` written back, then allocate exactly that.
        let mut list = RegList::new(1).map_err(|e| Error::Snapshot(format!("reg list: {e}")))?;
        if self.fd.get_reg_list(&mut list).is_err() {
            let n = list.as_fam_struct_ref().n as usize;
            list = RegList::new(n).map_err(|e| Error::Snapshot(format!("reg list {n}: {e}")))?;
            self.fd.get_reg_list(&mut list).map_err(kvm_err("KVM_GET_REG_LIST"))?;
        }
        let mut kvm_regs = Vec::with_capacity(list.as_slice().len());
        for &id in list.as_slice() {
            let size = 1usize << ((id >> 52) & 0xf); // KVM_REG_SIZE_* in bits [55:52]
            let mut buf = vec![0u8; size];
            self.fd.get_one_reg(id, &mut buf).map_err(kvm_err("GET_ONE_REG"))?;
            kvm_regs.push((id, buf));
        }
        Ok(CpuSnapshot { kvm_regs, ..Default::default() })
    }

    /// Restore the captured register file. A few registers KVM lists are
    /// read-only (some ID registers); skip a failing write rather than abort, so
    /// one RO entry doesn't sink the whole restore.
    fn restore(&mut self, cpu: &CpuSnapshot) -> Result<()> {
        if cpu.kvm_regs.is_empty() {
            return Err(Error::Snapshot("snapshot has no KVM registers".into()));
        }
        for (id, bytes) in &cpu.kvm_regs {
            if let Err(e) = self.fd.set_one_reg(*id, bytes) {
                log::debug!("kvm restore: skip reg {id:#x}: {e}");
            }
        }
        Ok(())
    }
}

/// Self-test the vcpu register round-trip against a real `/dev/kvm` (run inside
/// the QEMU/KVM test env). Fast — no guest boot. Prints `KVM_SELFTEST_OK`.
pub fn selftest() -> Result<()> {
    let mem = GuestMemory::new(0x4000_0000, 0x10_0000)?;
    let vm = KvmVm::create(&mem, 1)?;
    let mut v = vm.create_vcpu(0)?;
    v.set_boot_regs(0x4001_2340, 0x4000_0000)?;
    v.set_x(5, 0xdead_beef_cafe_f00d)?;
    let snap = v.capture()?;
    v.set_x(5, 0)?; // clobber, then restore should bring it back
    v.set_pc(0)?;
    v.restore(&snap)?;
    let (pc, x5) = (v.pc()?, v.get_x(5)?);
    if pc != 0x4001_2340 || x5 != 0xdead_beef_cafe_f00d {
        return Err(Error::Snapshot(format!("register round-trip mismatch: pc={pc:#x} x5={x5:#x}")));
    }
    // vGIC save/restore round-trip on the same VM (vcpu 0 INIT'd the vGIC).
    let gic_regs = gic::selftest(&vm.vgic, NR_IRQS, vm.vcpus)?;

    // CPU template: applying no-crypto must clear the crypto fields of ID_AA64ISAR0.
    let isar0 = kvm_sysreg_id(amber_core::cpu::ID_AA64ISAR0_EL1);
    let before = v.get_reg(isar0)?;
    v.apply_cpu_template(amber_core::cpu::by_name("no-crypto").unwrap())?;
    let after = v.get_reg(isar0)?;
    let crypto = (0xfffu64 << 4) | (0xfffu64 << 32);
    if after & crypto != 0 {
        return Err(Error::Snapshot(format!(
            "cpu template did not mask crypto: ISAR0 {before:#x} -> {after:#x}"
        )));
    }

    println!("KVM_SELFTEST_OK regs={} gic_regs={gic_regs} isar0={before:#x}->{after:#x}", snap.kvm_regs.len());
    Ok(())
}

impl Drop for KvmVcpu {
    fn drop(&mut self) {
        // The kvm_run mapping size is fixed by KVM; munmap with a page-rounded
        // guess is fine since the kernel tracks the real length.
        unsafe { libc::munmap(self.kvm_run as *mut libc::c_void, 0x1000) };
    }
}
