//! The backend-agnostic VM: load the kernel and DTB, wire the bus, create the
//! vcpu, and run the exit loop. This is the file that proves the seam works:
//! it names no hypervisor, only the `Hypervisor`/`Vcpu` traits.

use crate::bus::{MmioBus, Pl011};
use crate::hypervisor::{Hypervisor, MmioOp, Vcpu, VmExit};
use crate::virtio::{BalloonDevice, BalloonHandle, BlkDevice, RngDevice, VirtioDevice, VirtioMmio};
use crate::{dtb, layout, loader, GuestMemory, Result};
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A virtio-mmio device placed at a window with an assigned interrupt.
struct VirtioDev {
    base: u64,
    size: u64,
    intid: u32,
    mmio: VirtioMmio,
}

impl VirtioDev {
    fn new(index: usize, dev: Box<dyn VirtioDevice>) -> Self {
        Self {
            base: layout::VIRTIO_MMIO_BASE + index as u64 * layout::VIRTIO_MMIO_STRIDE,
            size: layout::VIRTIO_MMIO_STRIDE,
            intid: layout::GIC_SPI_BASE + layout::VIRTIO_SPI_BASE + index as u32,
            mmio: VirtioMmio::new(dev),
        }
    }
    fn contains(&self, ipa: u64) -> bool {
        ipa >= self.base && ipa < self.base + self.size
    }
}

/// Append a virtio-balloon device and return a handle to drive it plus its INTID.
fn push_balloon(virtio: &mut Vec<VirtioDev>) -> (BalloonHandle, u32) {
    let target = Arc::new(AtomicU64::new(0));
    let dirty = Arc::new(AtomicBool::new(false));
    let i = virtio.len();
    let dev = BalloonDevice::new(target, dirty);
    let handle = dev.handle();
    let intid = layout::GIC_SPI_BASE + layout::VIRTIO_SPI_BASE + i as u32;
    virtio.push(VirtioDev::new(i, Box::new(dev)));
    (handle, intid)
}

pub struct VmConfig {
    pub mem_size: usize,
    pub kernel: Vec<u8>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    /// Host image file to back the guest's virtio-blk root device (e.g. a
    /// squashfs base). None means no block device.
    pub disk: Option<PathBuf>,
    /// If set, snapshot the VM to `dir` once it has run for `after`, then stop.
    pub snapshot: Option<SnapshotReq>,
    /// A control-channel fd (from amberd) carrying balloon targets, etc.
    pub control_fd: Option<RawFd>,
}

/// A request to capture a snapshot after the guest has run for a while.
#[derive(Clone)]
pub struct SnapshotReq {
    pub after: std::time::Duration,
    pub dir: PathBuf,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            mem_size: 512 * 1024 * 1024,
            kernel: Vec::new(),
            initrd: None,
            // console=ttyAMA0 is the real tty (app output, interactive). `quiet`
            // suppresses the verbose boot dmesg, which otherwise streams to the
            // PL011 one char per MMIO exit — thousands of vmexits that dominate
            // boot time. earlycon is left off here for the same reason (add it
            // back for boot debugging).
            cmdline: "console=ttyAMA0 quiet".into(),
            disk: None,
            snapshot: None,
            control_fd: None,
        }
    }
}

pub struct Vm {
    mem: GuestMemory,
    bus: MmioBus,
    pl011: Pl011,
    virtio: Vec<VirtioDev>,
    balloon: Option<(BalloonHandle, u32)>,
    control_fd: Option<RawFd>,
    /// Warm-pool gate: when set, `run` finishes the restore, signals readiness on
    /// the control fd, and blocks for a one-byte "go" before resuming the guest —
    /// so the expensive work is pre-staged and a fork is just the go.
    paused: bool,
    snapshot: Option<SnapshotReq>,
    disk_path: Option<PathBuf>,
    /// When set, `run` restores this captured state instead of booting a kernel.
    restore: Option<crate::snapshot::Loaded>,
    entry: u64,
    dtb_addr: u64,
    // Kept so the device tree can be built in `run`, once the backend has
    // created its interrupt controller and we know the real GIC layout.
    mem_size: u64,
    cmdline: String,
    initrd: Option<(u64, u64)>,
}

impl Vm {
    /// Place the kernel and initramfs in guest RAM and wire the device bus. The
    /// DTB is deferred to `run`, because its interrupt-controller node depends on
    /// the GIC the backend creates.
    pub fn prepare(cfg: &VmConfig, net: Option<Box<dyn crate::net::NetBackend>>) -> Result<Self> {
        let mem = GuestMemory::new(layout::RAM_BASE, cfg.mem_size)?;

        let kernel = loader::load_kernel(&mem, &cfg.kernel)?;
        let initrd = loader::load_initramfs(&mem, cfg.initrd.as_deref())?;
        let dtb_addr = mem.base() + layout::DTB_OFFSET;

        // Block device first (so it is /dev/vda) if there is a disk, then an
        // entropy source, then the network (if any). Indices set each device's
        // MMIO window and interrupt.
        let mut virtio: Vec<VirtioDev> = Vec::new();
        if let Some(path) = &cfg.disk {
            let i = virtio.len();
            virtio.push(VirtioDev::new(i, Box::new(BlkDevice::open(path)?)));
        }
        let i = virtio.len();
        virtio.push(VirtioDev::new(i, Box::new(RngDevice::open()?)));
        if let Some(backend) = net {
            let i = virtio.len();
            virtio.push(VirtioDev::new(i, Box::new(crate::net::NetDevice::new(backend))));
        }
        let balloon = push_balloon(&mut virtio);

        Ok(Self {
            mem,
            bus: MmioBus::default(),
            pl011: Pl011::new(Box::new(std::io::stdout())),
            virtio,
            balloon: Some(balloon),
            control_fd: cfg.control_fd,
            paused: false,
            snapshot: cfg.snapshot.clone(),
            disk_path: cfg.disk.clone(),
            restore: None,
            entry: kernel.entry,
            dtb_addr,
            mem_size: cfg.mem_size as u64,
            cmdline: cfg.cmdline.clone(),
            initrd,
        })
    }

    /// Build a VM from a snapshot directory: load its RAM, re-open its disk and
    /// devices, and stash the captured GIC + vcpu state for `run` to apply
    /// instead of booting.
    pub fn restore_from(dir: &std::path::Path) -> Result<Self> {
        let loaded = crate::snapshot::read(dir)?;
        // Copy-on-write map of the snapshot's RAM: no up-front copy, and every fork
        // of this template shares the untouched pages. This is the fork fast path.
        let mem = GuestMemory::from_snapshot_cow(loaded.meta.mem_base, &dir.join("mem.bin"))?;

        let mut virtio: Vec<VirtioDev> = Vec::new();
        if let Some(path) = &loaded.meta.disk {
            let i = virtio.len();
            virtio.push(VirtioDev::new(i, Box::new(BlkDevice::open(std::path::Path::new(path))?)));
        }
        let i = virtio.len();
        virtio.push(VirtioDev::new(i, Box::new(RngDevice::open()?)));
        let balloon = push_balloon(&mut virtio);

        Ok(Self {
            mem,
            bus: MmioBus::default(),
            pl011: Pl011::new(Box::new(std::io::stdout())),
            virtio,
            balloon: Some(balloon),
            control_fd: None,
            paused: false,
            snapshot: None,
            disk_path: loaded.meta.disk.clone().map(PathBuf::from),
            restore: Some(loaded),
            entry: 0,
            dtb_addr: 0,
            mem_size: 0,
            cmdline: String::new(),
            initrd: None,
        })
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.mem
    }

    /// Attach a control fd and (optionally) arm the warm-pool pause gate. Used by
    /// the daemon when pre-staging a pooled fork: the worker restores, signals
    /// ready on `fd`, and waits for a go byte before the guest's first instruction.
    pub fn with_control(mut self, fd: RawFd, paused: bool) -> Self {
        self.control_fd = Some(fd);
        self.paused = paused;
        self
    }

    /// Boot and run on backend `H` until shutdown or a fatal fault.
    ///
    /// Two threads. The vcpu runs synchronously on this one. A second thread
    /// reads host stdin and feeds the UART: with a GIC present the host
    /// hypervisor parks the vcpu inside `run` on a guest WFI and wakes it on an
    /// interrupt, so console input has to arrive as an interrupt. The reader
    /// pushes bytes into the PL011 and raises its GIC line, which wakes the vcpu;
    /// that is what makes the console interactive rather than output-only.
    pub fn run<H: Hypervisor + Sync>(self) -> Result<()> {
        let Vm {
            mem,
            mut bus,
            mut pl011,
            mut virtio,
            balloon,
            control_fd,
            paused,
            snapshot,
            disk_path,
            restore,
            entry,
            dtb_addr,
            mem_size,
            cmdline,
            initrd,
        } = self;

        let mut hv = H::create(&mem)?;

        // Give each device its view of guest RAM (it reads rings/buffers).
        for d in &mut virtio {
            d.mmio.attach(mem.ram());
        }

        let mut vcpu = hv.create_vcpu(0)?;

        if let Some(loaded) = &restore {
            // Restore path: the RAM already holds the booted kernel + DTB; put the
            // interrupt controller and vcpu back where they were and resume.
            // The GIC blob format is per-kind (the software GICv2 and the in-kernel
            // vGICv3 are incompatible), so refuse a snapshot taken under a different
            // backend rather than feeding garbage to restore_gic.
            if let (Some(want), Some(have)) = (&loaded.meta.gic_kind, hv.gic_info()) {
                if want != have.kind.as_str() {
                    return Err(crate::Error::Snapshot(format!(
                        "snapshot GIC is {want} but this backend is {}; restore with the matching AMBER_GIC",
                        have.kind.as_str()
                    )));
                }
            }
            hv.restore_gic(&loaded.gic)?;
            vcpu.restore(&loaded.cpu)?;
            // Restore host-side device state that is not in guest RAM. The PL011
            // mask especially: without it, console input cannot interrupt the guest.
            if loaded.dev.pl011.len() == 6 {
                let r = &loaded.dev.pl011;
                pl011.set_regs([r[0], r[1], r[2], r[3], r[4], r[5]]);
            }
            // virtio queue state (ring addresses, ready, consumed index) so a
            // post-restore disk/rng/balloon kick resumes instead of hanging.
            for (d, s) in virtio.iter_mut().zip(&loaded.dev.virtio) {
                d.mmio.restore(s);
            }
        } else {
            // Boot path: seed entropy, build the device tree to match the GIC,
            // and set the arm64 boot registers.
            let mut rng_seed = [0u8; 64];
            let _ = std::fs::File::open("/dev/urandom")
                .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut rng_seed));
            let virtio_nodes: Vec<(u64, u64, u32)> = virtio
                .iter()
                .map(|d| (d.base, d.size, d.intid - layout::GIC_SPI_BASE))
                .collect();
            let blob = dtb::build(&dtb::DtbParams {
                mem_size,
                cmdline: &cmdline,
                initrd,
                gic: hv.gic_info(),
                virtio: &virtio_nodes,
                rng_seed: &rng_seed,
            })?;
            mem.write(dtb_addr, &blob)?;
            vcpu.set_boot_regs(entry, dtb_addr)?;
        }

        // Warm-pool gate. All the costly work (VM + GIC + register restore) is now
        // done; if this is a pooled fork, signal the daemon we are ready and block
        // for a one-byte "go" before the guest runs. A fork is then just this
        // handshake, not a spawn. (The same control fd carries balloon targets
        // afterward, read by the control thread below — the go byte comes first.)
        if paused {
            if let Some(fd) = control_fd {
                let ready = [1u8];
                unsafe { libc::write(fd, ready.as_ptr() as *const libc::c_void, 1) };
                let mut go = [0u8; 1];
                loop {
                    let n = unsafe { libc::read(fd, go.as_mut_ptr() as *mut libc::c_void, 1) };
                    // 1 = released; 0 = daemon gone (proceed); -1 = retry on EINTR.
                    if n >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        break;
                    }
                }
            }
        }

        let pl011_lo = layout::PL011_BASE;
        let pl011_hi = layout::PL011_BASE + layout::PL011_SIZE;
        let pl011_intid = layout::GIC_SPI_BASE + layout::PL011_SPI;

        let pl011 = Mutex::new(pl011);
        let running = AtomicBool::new(true);
        let hv = &hv;
        let pl011 = &pl011;
        let running = &running;
        let mem = &mem;
        let snapshot = snapshot.as_ref();
        let disk_path = disk_path.as_deref();
        let started = std::time::Instant::now();

        let balloon = balloon.as_ref();
        // The vcpu loop runs on this thread; reader threads unpark it to cut idle
        // latency when it is parked waiting for the timer (software-GIC mode).
        let vcpu_thread = std::thread::current();
        let vcpu_thread = &vcpu_thread;
        std::thread::scope(|s| {
            // Console reader: host stdin -> UART RX -> GIC line -> wake the vcpu.
            s.spawn(move || console_reader(hv, pl011, pl011_intid, running, vcpu_thread));

            // Control reader: a control-channel target -> balloon inflate. Moving
            // the target and raising the device's config interrupt makes the guest
            // give pages back (active reclaim). Only when amberd passed a fd.
            if let (Some(fd), Some((handle, intid))) = (control_fd, balloon) {
                s.spawn(move || control_reader(hv, fd, handle, *intid, running));
            }

            // vcpu loop on this thread.
            let mut step = || -> Result<bool> {
                // Deliver any frames that arrived from the network backend into the
                // guest's receive queue and raise the device interrupt.
                for d in virtio.iter_mut() {
                    if d.mmio.pump_rx() {
                        hv.set_irq(d.intid, d.mmio.irq_level())?;
                    }
                }

                // Snapshot point: the guest is stopped between runs, so its RAM
                // and registers are consistent. Capture once the deadline passes,
                // then stop (restore is a separate path).
                if let Some(req) = snapshot {
                    if started.elapsed() >= req.after {
                        let cpu = vcpu.capture()?;
                        let gic = hv.capture_gic()?;
                        let gic_kind = hv.gic_info().map(|g| g.kind);
                        let dev = crate::snapshot::DevState {
                            pl011: pl011.lock().unwrap().regs().to_vec(),
                            virtio: virtio.iter().map(|d| d.mmio.capture()).collect(),
                        };
                        crate::snapshot::write(&req.dir, mem, &cpu, &gic, disk_path, gic_kind, &dev)?;
                        log::info!("snapshot captured to {}", req.dir.display());
                        return Ok(false);
                    }
                }
                match vcpu.run()? {
                    VmExit::Mmio { access } => {
                        let ipa = access.ipa;
                        if ipa >= pl011_lo && ipa < pl011_hi {
                            let off = ipa - pl011_lo;
                            match access.op {
                                MmioOp::Write { value, .. } => {
                                    pl011.lock().unwrap().write(off, access.size, value);
                                }
                                MmioOp::Read { reg } => {
                                    let v = pl011.lock().unwrap().read(off, access.size);
                                    vcpu.set_x(reg, v)?;
                                }
                            }
                            let lvl = pl011.lock().unwrap().irq_level();
                            hv.set_irq(pl011_intid, lvl)?;
                        } else if let Some(d) = virtio.iter_mut().find(|d| d.contains(ipa)) {
                            let off = ipa - d.base;
                            match access.op {
                                MmioOp::Write { value, .. } => d.mmio.write(off, access.size, value),
                                MmioOp::Read { reg } => {
                                    let v = d.mmio.read(off, access.size);
                                    vcpu.set_x(reg, v)?;
                                }
                            }
                            hv.set_irq(d.intid, d.mmio.irq_level())?;
                        } else {
                            match access.op {
                                MmioOp::Write { value, .. } => bus.write(ipa, access.size, value),
                                MmioOp::Read { reg } => {
                                    let v = bus.read(ipa, access.size);
                                    vcpu.set_x(reg, v)?;
                                }
                            }
                        }
                        // Step past the faulting load/store: arm64 instructions
                        // are 4 bytes, and a syndrome-decodable access is one insn.
                        let pc = vcpu.pc()?;
                        vcpu.set_pc(pc + 4)?;
                        Ok(true)
                    }
                    // On a fresh boot the in-kernel vGIC handles WFI internally and
                    // this never fires; with the software GIC (and after a restore)
                    // it does. Park until the virtual timer is due, capped so a
                    // missed wake still recovers — and a reader thread unparks us
                    // the instant console/control input raises a line, so the cap is
                    // only a backstop, not the input latency.
                    VmExit::Idle => {
                        let ns = match vcpu.pending_timer_ns() {
                            Ok(Some(n)) => n.min(50_000_000),
                            _ => 50_000_000,
                        };
                        if ns > 0 {
                            std::thread::park_timeout(std::time::Duration::from_nanos(ns));
                        }
                        Ok(true)
                    }
                    VmExit::Shutdown => {
                        log::info!("guest requested shutdown");
                        Ok(false)
                    }
                    VmExit::Fault { pc, esr, ipa } => Err(crate::Error::GuestFault { pc, esr, ipa }),
                }
            };

            let mut result = Ok(());
            loop {
                match step() {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            // Let the reader thread exit so the scope can join it.
            running.store(false, Ordering::Relaxed);
            result
        })
    }
}

/// Read host stdin and deliver bytes to the guest UART, raising its GIC line so a
/// parked vcpu wakes. Polls with a timeout so it can notice shutdown; stops
/// feeding on EOF (the timer still drives the guest). Reads fd 0 directly, which
/// couples amber-core to "stdin is the console" — true for `amber boot`, and the
/// control plane will replace it later.
fn console_reader<H: Hypervisor>(
    hv: &H,
    pl011: &Mutex<Pl011>,
    intid: u32,
    running: &AtomicBool,
    vcpu: &std::thread::Thread,
) {
    let mut buf = [0u8; 64];
    while running.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, 100) };
        if n <= 0 {
            continue;
        }
        let r = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if r <= 0 {
            break; // EOF or error: stop feeding input
        }
        let level = {
            let mut p = pl011.lock().unwrap();
            for b in &buf[..r as usize] {
                p.push_rx(*b);
            }
            p.irq_level()
        };
        let _ = hv.set_irq(intid, level);
        // Wake the vcpu if it is parked in the idle handler (software-GIC mode,
        // where the line is only sampled between guest entries). With the in-kernel
        // vGIC `set_irq` already wakes it, so the unpark is just a harmless nudge.
        if level {
            vcpu.unpark();
        }
    }
}

/// Read balloon targets (8-byte LE MiB values) from the control fd and apply them:
/// move the device target and raise its config-change interrupt so the guest
/// inflates. Polls so it can notice shutdown and let the scope join.
fn control_reader<H: Hypervisor>(
    hv: &H,
    fd: RawFd,
    balloon: &BalloonHandle,
    intid: u32,
    running: &AtomicBool,
) {
    let mut buf = [0u8; 8];
    while running.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, 100) };
        if n <= 0 {
            continue;
        }
        let r = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if r != 8 {
            break; // EOF/short read: amberd closed the channel
        }
        let mib = u64::from_le_bytes(buf);
        balloon.target_pages.store(mib * 256, Ordering::Relaxed); // 1 MiB = 256 * 4 KiB
        balloon.config_dirty.store(true, Ordering::Relaxed);
        let _ = hv.set_irq(intid, true);
    }
}
