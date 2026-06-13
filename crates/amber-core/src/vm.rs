//! The backend-agnostic VM: load the kernel and DTB, wire the bus, create the
//! vcpu, and run the exit loop. This is the file that proves the seam works:
//! it names no hypervisor, only the `Hypervisor`/`Vcpu` traits.

use crate::bus::{MmioBus, Pl011};
use crate::hypervisor::{Hypervisor, MmioAccess, Vcpu, VmExit};
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
    /// Guest CPUs. Secondaries boot via PSCI CPU_ON, one host thread each.
    pub vcpus: usize,
    pub kernel: Vec<u8>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    /// Host image file to back the guest's virtio-blk root device (e.g. a
    /// squashfs base). None means no block device.
    pub disk: Option<PathBuf>,
    /// Data disks attached after the root (`/dev/vdb`, `/dev/vdc`, …) as
    /// `(path, writable)`; writes persist to the backing files. A `run`-time
    /// feature, separate from the snapshot/template model (root-only).
    pub data_disks: Vec<(PathBuf, bool)>,
    /// virtio-vsock: the host Unix-socket base path for the guest↔host channel,
    /// if enabled. The guest gets CID 3.
    pub vsock: Option<PathBuf>,
    /// If set, snapshot the VM to `dir` once it has run for `after`, then stop.
    pub snapshot: Option<SnapshotReq>,
    /// A control-channel fd (from amberd) carrying balloon targets, etc.
    pub control_fd: Option<RawFd>,
}

/// A request to capture a snapshot once the guest is ready. Triggered either by
/// a console marker (deterministic — fires the instant the guest prints it) or,
/// if no marker is set, after a wall-clock delay.
#[derive(Clone)]
pub struct SnapshotReq {
    pub after: std::time::Duration,
    pub dir: PathBuf,
    /// Console-output sentinel; when the guest's TX ends with these bytes, snapshot.
    pub marker: Option<Vec<u8>>,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            mem_size: 512 * 1024 * 1024,
            vcpus: 1,
            kernel: Vec::new(),
            initrd: None,
            // console=ttyAMA0 is the real tty (app output, interactive). `quiet`
            // suppresses the verbose boot dmesg, which otherwise streams to the
            // PL011 one char per MMIO exit — thousands of vmexits that dominate
            // boot time. earlycon is left off here for the same reason (add it
            // back for boot debugging).
            cmdline: "console=ttyAMA0 quiet".into(),
            disk: None,
            data_disks: Vec::new(),
            vsock: None,
            snapshot: None,
            control_fd: None,
        }
    }
}

pub struct Vm {
    mem: GuestMemory,
    vcpus: usize,
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
        // Block devices first and contiguous, so the kernel names them vda, vdb…
        // in attach order: the read-only root, then any writable data disks.
        let mut virtio: Vec<VirtioDev> = Vec::new();
        if let Some(path) = &cfg.disk {
            let i = virtio.len();
            virtio.push(VirtioDev::new(i, Box::new(BlkDevice::open(path)?)));
        }
        for (path, writable) in &cfg.data_disks {
            let i = virtio.len();
            let dev = if *writable {
                BlkDevice::open_writable(path)?
            } else {
                BlkDevice::open(path)?
            };
            virtio.push(VirtioDev::new(i, Box::new(dev)));
        }
        let i = virtio.len();
        virtio.push(VirtioDev::new(i, Box::new(RngDevice::open()?)));
        if let Some(backend) = net {
            let i = virtio.len();
            virtio.push(VirtioDev::new(i, Box::new(crate::net::NetDevice::new(backend))));
        }
        if let Some(path) = &cfg.vsock {
            match crate::vsock::UdsBackend::new(path.clone()) {
                Some(b) => {
                    let i = virtio.len();
                    virtio.push(VirtioDev::new(i, Box::new(crate::vsock::VsockDevice::new(3, Box::new(b)))));
                }
                None => log::warn!("vsock: cannot bind {}, disabled", path.display()),
            }
        }
        let balloon = push_balloon(&mut virtio);

        Ok(Self {
            mem,
            vcpus: cfg.vcpus.clamp(1, 8),
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
    pub fn restore_from(
        dir: &std::path::Path,
        net: Option<Box<dyn crate::net::NetBackend>>,
        vsock: Option<PathBuf>,
    ) -> Result<Self> {
        let loaded = crate::snapshot::read(dir)?;
        // Copy-on-write map of the snapshot's RAM: no up-front copy, and every fork
        // of this template shares the untouched pages. This is the fork fast path.
        let mem = GuestMemory::from_snapshot_cow(loaded.meta.mem_base, &dir.join("mem.bin"))?;

        // Recreate the same device set the template had, in the same order, so the
        // restored virtio queue state lines up. The network device gets a fresh
        // backend (its host state is not snapshotted); the guest's driver state
        // (eth0 config) is in restored RAM, so the fork has working net at once.
        let mut virtio: Vec<VirtioDev> = Vec::new();
        if let Some(path) = &loaded.meta.disk {
            let i = virtio.len();
            virtio.push(VirtioDev::new(i, Box::new(BlkDevice::open(std::path::Path::new(path))?)));
        }
        let i = virtio.len();
        virtio.push(VirtioDev::new(i, Box::new(RngDevice::open()?)));
        if loaded.meta.net {
            if let Some(backend) = net {
                let i = virtio.len();
                virtio.push(VirtioDev::new(i, Box::new(crate::net::NetDevice::new(backend))));
            }
        }
        // vsock sits after net and before balloon, matching prepare()'s order so
        // the restored virtio queues line up. A fresh UDS backend; the guest's
        // driver state is in restored RAM, and the agent dials only post-restore.
        if loaded.meta.vsock {
            if let Some(path) = vsock {
                match crate::vsock::UdsBackend::new(path.clone()) {
                    Some(b) => {
                        let i = virtio.len();
                        virtio.push(VirtioDev::new(
                            i,
                            Box::new(crate::vsock::VsockDevice::new(3, Box::new(b))),
                        ));
                    }
                    None => log::warn!("vsock: cannot bind {} on restore", path.display()),
                }
            } else {
                log::warn!("snapshot has a vsock device but no AMBER_VSOCK given; device set will mismatch");
            }
        }
        let balloon = push_balloon(&mut virtio);

        Ok(Self {
            mem,
            vcpus: loaded.meta.vcpus.clamp(1, 8),
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
            vcpus,
            bus,
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

        let hv = H::create(&mem, vcpus)?;

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
                vcpus,
                cmdline: &cmdline,
                initrd,
                gic: hv.gic_info(),
                virtio: &virtio_nodes,
                rng_seed: &rng_seed,
            })?;
            mem.write(dtb_addr, &blob)?;
            vcpu.set_boot_regs(entry, dtb_addr)?;
        }

        // Lockdown: everything the VM needs is open (RAM, disk fd, control fd,
        // console, bound listeners); drop the ability to acquire more before any
        // guest instruction runs. The snapshot dir stays writable for capture;
        // new sockets only if a network device (the netstack dials at runtime) or
        // a vsock device (its UDS backend dials per guest connection) exists.
        // Platform mechanism behind lockdown::Policy (macOS: seatbelt).
        let policy = crate::lockdown::Policy {
            write_paths: snapshot.iter().map(|r| r.dir.clone()).collect(),
            net: virtio.iter().any(|d| matches!(d.mmio.device_id(), 1 | 19)),
        };
        if let Err(e) = policy.apply() {
            return Err(crate::Error::Backend(format!("vmm lockdown: {e}")));
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

        let pl011_intid = layout::GIC_SPI_BASE + layout::PL011_SPI;

        // Arm the console snapshot trigger (if the request carries a marker) so the
        // loop captures the instant the guest prints it.
        if let Some(req) = snapshot.as_ref() {
            if let Some(m) = &req.marker {
                pl011.arm_marker(m.clone());
            }
        }

        // Shared device model: secondary vcpus take MMIO exits on their own
        // threads, so every device sits behind a Mutex. MMIO is rare relative to
        // guest execution; contention is not a hot path.
        let pl011 = Mutex::new(pl011);
        let virtio = Mutex::new(virtio);
        let bus = Mutex::new(bus);
        let running = AtomicBool::new(true);
        let hv = &hv;
        let pl011 = &pl011;
        let virtio = &virtio;
        let bus = &bus;
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
        // Live pause gate: the control reader sets this on PAUSE; the vcpu loop
        // blocks on it until RESUME. Separate from the warm-pool startup gate above.
        let pause = (Mutex::new(false), std::sync::Condvar::new());
        let pause = &pause;
        // Snapshot quiesce barrier (only armed for a multi-vcpu snapshot).
        let snap_coord = SnapCoord {
            want: AtomicBool::new(false),
            states: Mutex::new(vec![None; vcpus.saturating_sub(1)]),
            cv: std::sync::Condvar::new(),
        };
        let snap_coord = &snap_coord;
        let restore = &restore;
        std::thread::scope(|s| {
            // Console reader: host stdin -> UART RX -> GIC line -> wake the vcpu.
            s.spawn(move || console_reader(hv, pl011, pl011_intid, running, vcpu_thread));

            // Control reader: dispatches amberd control frames — balloon targets
            // (active reclaim) and live pause/resume. Runs whenever amberd passed a fd.
            if let Some(fd) = control_fd {
                s.spawn(move || control_reader(hv, fd, balloon, running, pause));
            }

            // Secondary vcpus: one thread each, created on its own thread (the
            // backend binds a vcpu to its creating thread). On a fresh boot each
            // parks inside `Vcpu::run` until the guest PSCI-CPU_ONs it; on a
            // restore it resumes from its captured state immediately.
            for id in 1..vcpus {
                let init = restore.as_ref().and_then(|l| l.cpus.get(id - 1));
                s.spawn(move || {
                    secondary_loop::<H>(hv, id as u8, init, pl011, pl011_intid, virtio, bus, running, pause, snap_coord)
                });
            }

            // Snapshot kicker: where an idle guest blocks in the kernel (KVM's
            // WFI), nothing returns to the loop to notice the snapshot deadline.
            // Once it passes, kick the vcpu(s) out of `run` so the loop captures;
            // repeat to beat the race of a kick landing just before `run` re-enters.
            // A no-op on backends whose idle path already returns (HVF).
            if let Some(req) = snapshot {
                let after = req.after;
                s.spawn(move || {
                    // Past the fallback deadline, kick the vcpu out of a blocking
                    // KVM_RUN so the loop can capture even if a console marker
                    // never arrives. Polls `running` so teardown joins promptly
                    // (and a marker that fires earlier ends this at once).
                    loop {
                        if !running.load(Ordering::Relaxed) {
                            break;
                        }
                        if started.elapsed() >= after {
                            hv.kick();
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                });
            }

            // Primary vcpu loop on this thread.
            let mut step = || -> Result<bool> {
                // Live pause: block the vcpu here while paused (the control reader
                // flips the flag and notifies). Guest RAM and registers stay put.
                {
                    let (m, cv) = pause;
                    let mut p = m.lock().unwrap();
                    while *p {
                        p = cv.wait(p).unwrap();
                    }
                }

                // Deliver any frames that arrived from the network backend into the
                // guest's receive queue and raise the device interrupt.
                for d in virtio.lock().unwrap().iter_mut() {
                    if d.mmio.pump_rx() {
                        hv.set_irq(d.intid, d.mmio.irq_level())?;
                    }
                }

                // Snapshot point: the guest is stopped between runs, so its RAM
                // and registers are consistent. Capture once the deadline passes,
                // then stop (restore is a separate path). prepare() guarantees
                // vcpus == 1 when a snapshot is armed.
                if let Some(req) = snapshot {
                    // Fire on the console marker if one is armed (deterministic),
                    // else on the wall-clock deadline.
                    let due = (req.marker.is_some() && pl011.lock().unwrap().marked())
                        || started.elapsed() >= req.after;
                    if due {
                        log::info!("snapshot: trigger reached, capturing vcpu state");
                        // Quiesce: every secondary must be out of guest execution
                        // (registers self-captured, parked) before RAM is dumped.
                        let mut cpus = vec![vcpu.capture()?];
                        if vcpus > 1 {
                            snap_coord.want.store(true, Ordering::Relaxed);
                            hv.set_yield(true);
                            let mut st = snap_coord.states.lock().unwrap();
                            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                            while st.iter().any(|x| x.is_none()) {
                                if std::time::Instant::now() > deadline {
                                    return Err(crate::Error::Snapshot(
                                        "vcpus did not quiesce for snapshot".into(),
                                    ));
                                }
                                let (g, _) = snap_coord
                                    .cv
                                    .wait_timeout(st, std::time::Duration::from_millis(100))
                                    .unwrap();
                                st = g;
                            }
                            cpus.extend(st.iter().map(|x| x.clone().unwrap()));
                        }
                        let gic = hv.capture_gic()?;
                        let gic_kind = hv.gic_info().map(|g| g.kind);
                        let v = virtio.lock().unwrap();
                        let dev = crate::snapshot::DevState {
                            pl011: pl011.lock().unwrap().regs().to_vec(),
                            virtio: v.iter().map(|d| d.mmio.capture()).collect(),
                        };
                        let has_net = v.iter().any(|d| d.mmio.device_id() == 1);
                        let has_vsock = v.iter().any(|d| d.mmio.device_id() == 19);
                        drop(v);
                        crate::snapshot::write(
                            &req.dir, mem, &cpus, &gic, disk_path, gic_kind, &dev, has_net, has_vsock,
                        )?;
                        log::info!("snapshot captured to {} ({} vcpus)", req.dir.display(), cpus.len());
                        return Ok(false);
                    }
                }
                match vcpu.run()? {
                    VmExit::Mmio { access } => {
                        dispatch_mmio::<H>(hv, &mut vcpu, &access, pl011, pl011_intid, virtio, bus)?;
                        Ok(true)
                    }
                    // On a fresh boot the in-kernel vGIC handles WFI internally and
                    // this never fires; with the software GIC (and after a restore)
                    // it does. Park until the virtual timer is due, capped so a
                    // missed wake still recovers — and a reader thread unparks us
                    // the instant console/control input raises a line, so the cap is
                    // only a backstop, not the input latency.
                    VmExit::Idle => {
                        // Cap the park hard when a device (the network backend) is
                        // awaiting async host-side replies, so they reach the guest
                        // in ~ms; otherwise park until the timer is due (50 ms cap).
                        let cap = if virtio.lock().unwrap().iter().any(|d| d.mmio.wants_poll()) {
                            1_000_000
                        } else {
                            50_000_000
                        };
                        let ns = match vcpu.pending_timer_ns() {
                            Ok(Some(n)) => n.min(cap),
                            _ => cap,
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
            while running.load(Ordering::Relaxed) {
                match step() {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            // Let the reader and secondary-vcpu threads exit so the scope joins.
            running.store(false, Ordering::Relaxed);
            hv.request_stop();
            result
        })
    }
}

/// Multi-vcpu snapshot coordination: the primary raises `want`, every secondary
/// captures its own registers at its next exit and parks; the primary proceeds
/// once all slots are filled (the snapshot then stops the VM).
struct SnapCoord {
    want: AtomicBool,
    /// states[i] = vcpu i+1's captured registers.
    states: Mutex<Vec<Option<crate::snapshot::CpuSnapshot>>>,
    cv: std::sync::Condvar,
}

/// Service one MMIO exit against the shared devices, from any vcpu thread.
/// Backend-neutral: a read's value goes back via `complete_mmio_read`, and
/// `advance_mmio` steps past the instruction (HVF) or no-ops (KVM auto-advances).
fn dispatch_mmio<H: Hypervisor>(
    hv: &H,
    vcpu: &mut H::Vcpu,
    access: &MmioAccess,
    pl011: &Mutex<Pl011>,
    pl011_intid: u32,
    virtio: &Mutex<Vec<VirtioDev>>,
    bus: &Mutex<MmioBus>,
) -> Result<()> {
    let pl011_lo = layout::PL011_BASE;
    let pl011_hi = layout::PL011_BASE + layout::PL011_SIZE;
    let ipa = access.ipa;
    if ipa >= pl011_lo && ipa < pl011_hi {
        let off = ipa - pl011_lo;
        match access.write {
            Some(value) => pl011.lock().unwrap().write(off, access.size, value),
            None => {
                let v = pl011.lock().unwrap().read(off, access.size);
                vcpu.complete_mmio_read(v)?;
            }
        }
        let lvl = pl011.lock().unwrap().irq_level();
        hv.set_irq(pl011_intid, lvl)?;
    } else if let Some(d) = virtio.lock().unwrap().iter_mut().find(|d| d.contains(ipa)) {
        let off = ipa - d.base;
        match access.write {
            Some(value) => d.mmio.write(off, access.size, value),
            None => {
                let v = d.mmio.read(off, access.size);
                vcpu.complete_mmio_read(v)?;
            }
        }
        hv.set_irq(d.intid, d.mmio.irq_level())?;
    } else {
        match access.write {
            Some(value) => bus.lock().unwrap().write(ipa, access.size, value),
            None => {
                let v = bus.lock().unwrap().read(ipa, access.size);
                vcpu.complete_mmio_read(v)?;
            }
        }
    }
    vcpu.advance_mmio()
}

/// A secondary vcpu's whole life: create on this thread, park inside `run` until
/// the guest PSCI-CPU_ONs it (or resume from `init` on a restore), then service
/// exits against the shared devices, honoring the pause gate and the snapshot
/// quiesce barrier. Any fault or shutdown from a secondary stops the VM.
#[allow(clippy::too_many_arguments)]
fn secondary_loop<H: Hypervisor>(
    hv: &H,
    id: u8,
    init: Option<&crate::snapshot::CpuSnapshot>,
    pl011: &Mutex<Pl011>,
    pl011_intid: u32,
    virtio: &Mutex<Vec<VirtioDev>>,
    bus: &Mutex<MmioBus>,
    running: &AtomicBool,
    pause: &(Mutex<bool>, std::sync::Condvar),
    snap: &SnapCoord,
) {
    // Stopping from a secondary must also pull the primary out of guest
    // execution (it may be blocked inside its own `run`), hence request_stop.
    let stop = || {
        running.store(false, Ordering::Relaxed);
        hv.request_stop();
    };
    let mut vcpu = match hv.create_vcpu(id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("vcpu{id}: create failed: {e}");
            stop();
            return;
        }
    };
    if let Some(state) = init {
        if let Err(e) = vcpu.restore(state) {
            log::error!("vcpu{id}: restore failed: {e}");
            stop();
            return;
        }
    }
    while running.load(Ordering::Relaxed) {
        // Snapshot quiesce: deposit this vcpu's registers and park; the VM stops
        // once the capture is written, so this parks until shutdown.
        if snap.want.load(Ordering::Relaxed) {
            let mut st = snap.states.lock().unwrap();
            if st[id as usize - 1].is_none() {
                match vcpu.capture() {
                    Ok(s) => st[id as usize - 1] = Some(s),
                    Err(e) => {
                        log::error!("vcpu{id}: capture failed: {e}");
                        drop(st);
                        stop();
                        continue;
                    }
                }
                snap.cv.notify_all();
            }
            let _ = snap
                .cv
                .wait_timeout(st, std::time::Duration::from_millis(100))
                .unwrap();
            continue;
        }
        // Live pause gate (same one the primary blocks on).
        {
            let (m, cv) = pause;
            let mut p = m.lock().unwrap();
            while *p && running.load(Ordering::Relaxed) {
                let (g, _) = cv.wait_timeout(p, std::time::Duration::from_millis(100)).unwrap();
                p = g;
            }
        }
        match vcpu.run() {
            Ok(VmExit::Mmio { access }) => {
                if let Err(e) =
                    dispatch_mmio::<H>(hv, &mut vcpu, &access, pl011, pl011_intid, virtio, bus)
                {
                    log::error!("vcpu{id}: mmio: {e}");
                    stop();
                }
            }
            // WFI (or a not-yet-started park timeout): wait out the timer, with a
            // tight cap — an IPI posted to this parked CPU is only noticed on
            // wake, so the cap bounds rescheduling latency (the cost: ~500
            // wakeups/s per idle secondary).
            Ok(VmExit::Idle) => {
                let ns = match vcpu.pending_timer_ns() {
                    Ok(Some(n)) => n.min(2_000_000),
                    _ => 2_000_000,
                };
                if ns > 0 {
                    std::thread::park_timeout(std::time::Duration::from_nanos(ns));
                }
            }
            Ok(VmExit::Shutdown) => {
                log::info!("vcpu{id}: guest requested shutdown");
                stop();
            }
            Ok(VmExit::Fault { pc, esr, ipa }) => {
                log::error!("vcpu{id}: fault pc={pc:#x} esr={esr:#x} ipa={ipa:#x}");
                stop();
            }
            Err(e) => {
                log::error!("vcpu{id}: {e}");
                stop();
            }
        }
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
/// Read exactly `buf.len()` bytes from a blocking fd; false on EOF/error.
fn read_exact_fd(fd: RawFd, buf: &mut [u8]) -> bool {
    let mut off = 0;
    while off < buf.len() {
        let r = unsafe {
            libc::read(fd, buf[off..].as_mut_ptr() as *mut libc::c_void, buf.len() - off)
        };
        if r <= 0 {
            return false;
        }
        off += r as usize;
    }
    true
}

/// Dispatch amberd control frames (one opcode byte + payload): balloon targets
/// (active reclaim, only if this VM has a balloon) and live pause/resume.
fn control_reader<H: Hypervisor>(
    hv: &H,
    fd: RawFd,
    balloon: Option<&(BalloonHandle, u32)>,
    running: &AtomicBool,
    pause: &(Mutex<bool>, std::sync::Condvar),
) {
    while running.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, 100) };
        if n <= 0 {
            continue;
        }
        let mut op = [0u8; 1];
        if unsafe { libc::read(fd, op.as_mut_ptr() as *mut libc::c_void, 1) } != 1 {
            break; // EOF: amberd closed the channel
        }
        match op[0] {
            crate::control::BALLOON => {
                let mut buf = [0u8; 8];
                if !read_exact_fd(fd, &mut buf) {
                    break;
                }
                if let Some((handle, intid)) = balloon {
                    let mib = u64::from_le_bytes(buf);
                    handle.target_pages.store(mib * 256, Ordering::Relaxed); // 1 MiB = 256 * 4 KiB
                    handle.config_dirty.store(true, Ordering::Relaxed);
                    let _ = hv.set_irq(*intid, true);
                }
            }
            crate::control::PAUSE => {
                *pause.0.lock().unwrap() = true;
                // Make every vcpu return to its loop and block on the gate. HVF
                // uses the yield flag (compute-bound vcpus exit at the next forced
                // exit); KVM needs a kick to break a WFI-halted vcpu out of a
                // blocking KVM_RUN. The gate then parks it until RESUME, so one
                // kick is enough. Each call is a no-op on the other backend.
                hv.set_yield(true);
                hv.kick();
            }
            crate::control::RESUME => {
                *pause.0.lock().unwrap() = false;
                hv.set_yield(false);
                pause.1.notify_all();
            }
            _ => {}
        }
    }
}
