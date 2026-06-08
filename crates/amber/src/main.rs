//! M0 runner. Not the real CLI yet (that is M2). Just enough to prove the boot:
//!
//!   amber boot <kernel-Image> [initramfs]
//!
//! On macOS arm64 it boots the kernel on HVF and you should see the kernel's
//! earlycon output on stdout. Off macOS arm64 it builds everything except the
//! backend and tells you so.

mod daemon;
mod manifest;
mod proto;

use amber_core::{Vm, VmConfig};
use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // `run` routes through amberd if one is up, else runs in-process.
        Some("run") => cmd_run(&args),
        // Internal worker: run one VM in-process (spawned by amberd).
        Some("__vm") => cmd_vm(&args),
        Some("serve") => match daemon::serve() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("amberd failed: {e}");
                ExitCode::FAILURE
            }
        },
        Some("up") => cmd_up(),
        Some("down") => cmd_down(),
        Some("ps") => cmd_ps(),
        Some("rm") => cmd_rm(&args),
        Some("logs") => cmd_logs(&args),
        Some("budget") => cmd_budget(),
        Some("balloon") => cmd_balloon(&args),
        Some("pull") => cmd_pull(&args),
        Some("restore") => cmd_restore(&args),
        Some("fork") => cmd_fork(&args),
        Some("boot") => cmd_boot(&args),
        _ => {
            eprintln!("usage:");
            eprintln!("  amber run [-d] <image|template> [-- <argv>...]");
            eprintln!("  amber up | down | ps | rm <id> | logs <id>");
            eprintln!("  amber pull <image>");
            eprintln!("  amber boot <kernel-Image> [initramfs] [disk]");
            ExitCode::FAILURE
        }
    }
}

/// Route `run`: `-d` detaches (daemon only); otherwise through amberd if
/// reachable (interactive), else in-process.
fn cmd_run(args: &[String]) -> ExitCode {
    // Split flags/target (before `--`) from the command argv (after it).
    let split = args.iter().position(|a| a == "--");
    let head = match split {
        Some(i) => &args[2..i],
        None => &args[2..],
    };
    let argv: Vec<String> = match split {
        Some(i) => args[i + 1..].to_vec(),
        None => Vec::new(),
    };
    let detached = head.iter().any(|a| a == "-d" || a == "--detach");
    let Some(target) = head.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("usage: amber run [-d] <image|template> [-- <argv>...]");
        return ExitCode::FAILURE;
    };

    if detached {
        if !daemon::running() {
            eprintln!("-d needs amberd (run 'amber up')");
            return ExitCode::FAILURE;
        }
        return match daemon::run_detached(target, &argv) {
            Ok(id) => {
                println!("{id}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("run failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if !daemon::running() {
        return cmd_vm(args);
    }
    // Raw mode so keystrokes reach the guest unbuffered (the guest tty echoes).
    let _raw = RawTerm::enable();
    match daemon::run_client(target, &argv) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("run failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_balloon(args: &[String]) -> ExitCode {
    let (Some(id), Some(mib)) = (args.get(2), args.get(3).and_then(|s| s.parse::<u64>().ok())) else {
        eprintln!("usage: amber balloon <id> <MiB>");
        return ExitCode::FAILURE;
    };
    match daemon::balloon(id, mib) {
        Ok(()) => {
            println!("balloon {id}: reclaiming toward {mib} MiB");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("balloon failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_budget() -> ExitCode {
    if !daemon::running() {
        eprintln!("no amberd (run 'amber up')");
        return ExitCode::FAILURE;
    }
    match daemon::budget() {
        Ok((budget, used, rss, machine)) => {
            let mib = |b: u64| b / (1024 * 1024);
            if budget == 0 {
                println!("budget: unlimited    reserved: {} MiB    real: {} MiB", mib(used), mib(rss));
            } else {
                println!(
                    "budget: {} MiB    reserved: {} MiB    free: {} MiB    real: {} MiB",
                    mib(budget),
                    mib(used),
                    mib(budget.saturating_sub(used)),
                    mib(rss)
                );
            }
            if machine > 0 {
                println!(
                    "machine: {} MiB    free on host: {} MiB",
                    mib(machine),
                    mib(machine.saturating_sub(if budget == 0 { rss } else { budget }))
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("budget failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_logs(args: &[String]) -> ExitCode {
    let Some(id) = args.get(2) else {
        eprintln!("usage: amber logs <id>");
        return ExitCode::FAILURE;
    };
    match daemon::logs(id) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("logs failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_up() -> ExitCode {
    if daemon::running() {
        println!("amberd already running");
        return ExitCode::SUCCESS;
    }
    let exe = std::env::current_exe().expect("current_exe");
    let _ = std::fs::create_dir_all("amber-cache");
    let log = match std::fs::File::create("amber-cache/amberd.log") {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot open amberd log: {e}");
            return ExitCode::FAILURE;
        }
    };
    let err = log.try_clone().expect("clone log fd");
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err);
    // Detach into its own session so it outlives the launching shell/TTY.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Err(e) = cmd.spawn() {
        eprintln!("failed to start amberd: {e}");
        return ExitCode::FAILURE;
    }
    // Wait briefly for the socket to come up.
    for _ in 0..40 {
        if daemon::running() {
            println!("amberd started ({})", proto::socket_path().display());
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    eprintln!("amberd did not come up; see amber-cache/amberd.log");
    ExitCode::FAILURE
}

fn cmd_down() -> ExitCode {
    if !daemon::running() {
        println!("amberd not running");
        return ExitCode::SUCCESS;
    }
    match daemon::shutdown() {
        Ok(()) => {
            println!("amberd stopped");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("down failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_ps() -> ExitCode {
    if !daemon::running() {
        eprintln!("no amberd (run 'amber up')");
        return ExitCode::FAILURE;
    }
    match daemon::list() {
        Ok(vms) => {
            let now = proto::now_secs();
            println!("{:<8} {:<8} {:<6} {:<8} {:<8} IMAGE", "ID", "PID", "AGE", "CAP", "RSS");
            for v in vms {
                println!(
                    "{:<8} {:<8} {:<6} {:<8} {:<8} {}",
                    v.id,
                    v.pid,
                    fmt_age(now.saturating_sub(v.started)),
                    format!("{}M", v.ram_bytes / (1024 * 1024)),
                    format!("{}M", v.rss_bytes / (1024 * 1024)),
                    v.reference
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ps failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_rm(args: &[String]) -> ExitCode {
    let Some(id) = args.get(2) else {
        eprintln!("usage: amber rm <id>");
        return ExitCode::FAILURE;
    };
    match daemon::kill(id) {
        Ok(()) => {
            println!("killed {id}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rm failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_boot(args: &[String]) -> ExitCode {
    if args.len() < 3 {
        eprintln!("usage: amber boot <kernel-Image> [initramfs]");
        return ExitCode::FAILURE;
    }
    let kernel = match std::fs::read(&args[2]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read kernel {}: {e}", args[2]);
            return ExitCode::FAILURE;
        }
    };
    let initrd = args.get(3).and_then(|p| std::fs::read(p).ok());
    let disk = args.get(4).map(std::path::PathBuf::from);

    let cfg = VmConfig { kernel, initrd, disk, ..Default::default() };

    let vm = match Vm::prepare(&cfg) {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("prepare failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Put the host terminal in raw mode so keystrokes reach the guest console
    // unbuffered and unprocessed; the guest's tty does the echo and line editing.
    // Restored on drop. No-op when stdin is not a terminal (piped input).
    let _raw = RawTerm::enable();

    match run(vm) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("run failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_pull(args: &[String]) -> ExitCode {
    let Some(image) = args.get(2) else {
        eprintln!("usage: amber pull <image>");
        return ExitCode::FAILURE;
    };
    // refresh = true: always re-resolve against the registry and update the
    // reference -> id mapping, so a moved tag is picked up.
    match amber_image::build(image, Path::new("amber-cache"), true) {
        Ok(built) => {
            let sz = std::fs::metadata(&built.base).map(|m| m.len()).unwrap_or(0);
            println!("base: {} ({} KiB)", built.base.display(), sz / 1024);
            println!("default argv: {:?}", built.config.default_argv());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pull failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Run one VM in-process: pull and flatten the image, pack a squashfs base,
/// generate a bootstrap initramfs that mounts it under a tmpfs overlay and execs
/// the command, then boot. Cold boot, run once, tear down. This is the worker
/// amberd spawns (`__vm`), and also the standalone path when no daemon is up.
fn cmd_vm(args: &[String]) -> ExitCode {
    let Some(target) = args.get(2) else {
        eprintln!("usage: amber run <image|template> [-- <argv>...]");
        return ExitCode::FAILURE;
    };
    // argv after a "--" separator overrides the image's default command.
    let user_argv: Vec<String> = match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].to_vec(),
        None => Vec::new(),
    };

    // A bare arg names an `amber.toml` template if one matches; otherwise it is an
    // OCI reference. A template contributes its image, ram_cap, and env.
    let manifest = manifest::Manifest::load();
    let (oci_ref, mem_size, extra_env): (String, Option<usize>, HashMap<String, String>) =
        match manifest.as_ref().and_then(|m| m.template(target)) {
            Some(t) => {
                log::info!("template '{target}' -> {}", t.image);
                let mem = t.ram_cap.as_deref().and_then(manifest::parse_size);
                if t.ram_cap.is_some() && mem.is_none() {
                    eprintln!("warning: bad ram_cap for template '{target}', using default");
                }
                (t.image.clone(), mem, t.env.clone())
            }
            None => (target.clone(), None, HashMap::new()),
        };

    let t0 = std::time::Instant::now();
    // Resolve, pull, flatten, and pack — cached by image content id, so repeated
    // runs of the same image skip straight to boot.
    let mut built = match amber_image::build(&oci_ref, Path::new("amber-cache"), false) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("build failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let t_build = t0.elapsed();
    // Template env overrides the image's, by key.
    for (k, v) in &extra_env {
        let prefix = format!("{k}=");
        built.config.env.retain(|e| !e.starts_with(&prefix));
        built.config.env.push(format!("{k}={v}"));
    }

    let argv = if user_argv.is_empty() {
        built.config.default_argv()
    } else {
        user_argv
    };
    if argv.is_empty() {
        eprintln!("no command: the image declares none and none was given after --");
        return ExitCode::FAILURE;
    }

    let initrd = match build_bootstrap(&built.config, &argv) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("bootstrap initramfs: {e}");
            return ExitCode::FAILURE;
        }
    };
    let kernel = match std::fs::read(guest::KERNEL) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read kernel {}: {e}", guest::KERNEL);
            return ExitCode::FAILURE;
        }
    };

    let mut cfg = VmConfig {
        kernel,
        initrd: Some(initrd),
        disk: Some(built.base.clone()),
        ..Default::default()
    };
    if let Some(bytes) = mem_size {
        cfg.mem_size = bytes;
    }
    // Control channel from amberd (balloon targets, etc.), if it passed one.
    cfg.control_fd = std::env::var("AMBER_CONTROL_FD").ok().and_then(|s| s.parse().ok());
    // Snapshot trigger (M3, de-risk): AMBER_SNAPSHOT=<dir> captures the VM after
    // AMBER_SNAPSHOT_AFTER_MS (default 2000) and stops.
    if let Ok(dir) = std::env::var("AMBER_SNAPSHOT") {
        let ms: u64 = std::env::var("AMBER_SNAPSHOT_AFTER_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2000);
        cfg.snapshot = Some(amber_core::SnapshotReq {
            after: std::time::Duration::from_millis(ms),
            dir: dir.into(),
        });
    }
    // Restore earlycon + verbose boot dmesg for debugging (off by default because
    // the dmesg streams char-per-MMIO-exit and roughly doubles boot time).
    if std::env::var("AMBER_VERBOSE").is_ok() {
        cfg.cmdline = "earlycon=pl011,0x9000000 console=ttyAMA0".into();
    }
    let t_prep_start = std::time::Instant::now();
    let vm = match Vm::prepare(&cfg) {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("prepare failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let t_prep = t_prep_start.elapsed();

    let _raw = RawTerm::enable();
    let t_run_start = std::time::Instant::now();
    let result = run(vm);
    let t_run = t_run_start.elapsed();

    if std::env::var("AMBER_TIME").is_ok() {
        eprintln!(
            "amber timing: build={:.0}ms prep={:.0}ms boot+run+teardown={:.0}ms total={:.0}ms",
            t_build.as_secs_f64() * 1e3,
            t_prep.as_secs_f64() * 1e3,
            t_run.as_secs_f64() * 1e3,
            (t_build + t_prep + t_run).as_secs_f64() * 1e3,
        );
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("run failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Restore a VM from a snapshot directory and resume it (M3, de-risk).
fn cmd_restore(args: &[String]) -> ExitCode {
    let Some(dir) = args.get(2) else {
        eprintln!("usage: amber restore <snapshot-dir>");
        return ExitCode::FAILURE;
    };
    let mut vm = match Vm::restore_from(Path::new(dir)) {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("restore failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    // When the daemon stages a pooled fork it passes a control fd and AMBER_PAUSED:
    // the worker signals ready and waits for the go byte before resuming the guest.
    if let Some(fd) = std::env::var("AMBER_CONTROL_FD").ok().and_then(|s| s.parse().ok()) {
        let paused = std::env::var("AMBER_PAUSED").as_deref() == Ok("1");
        vm = vm.with_control(fd, paused);
    }
    let _raw = RawTerm::enable();
    match run(vm) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("run failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `amber fork [-i] <template>`: hand off a warm fork of a template snapshot.
/// `-i` attaches the terminal to the resumed guest; otherwise it runs detached.
fn cmd_fork(args: &[String]) -> ExitCode {
    let rest: Vec<&String> = args[2..].iter().collect();
    let interactive = rest.iter().any(|a| a.as_str() == "-i");
    let Some(template) = rest.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("usage: amber fork [-i] <template-dir>");
        return ExitCode::FAILURE;
    };
    if interactive {
        let _raw = RawTerm::enable();
        match daemon::fork_interactive(template) {
            Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
            Err(e) => {
                drop(_raw);
                eprintln!("fork failed: {e}");
                ExitCode::FAILURE
            }
        }
    } else {
        match daemon::fork(template) {
            Ok(id) => {
                println!("{id}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("fork failed: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

/// Guest assets borrowed from the Alpine `virt` artifacts for M1. These move into
/// a bundled, built-in-everything kernel later (the single-binary promise).
mod guest {
    pub const KERNEL: &str = "assets/Image";
    pub const BUSYBOX: &str = "assets/irx/bin/busybox";
    pub const MUSL: &str = "assets/irx/lib/ld-musl-aarch64.so.1";
    pub const MODULES_ROOT: &str = "assets/irx/lib/modules";
    // (subpath under <modules_root>/<version>/kernel, load order matters)
    pub const MODULES: &[&str] = &[
        "drivers/virtio/virtio_mmio.ko",
        "drivers/block/virtio_blk.ko",
        "drivers/virtio/virtio_balloon.ko",
        "drivers/char/hw_random/rng-core.ko",
        "drivers/char/hw_random/virtio-rng.ko",
        "fs/squashfs/squashfs.ko",
        "fs/overlayfs/overlay.ko",
    ];
}

/// Build the gzipped cpio that bootstraps the image: load the virtio/fs modules,
/// mount the squashfs base read-only under a tmpfs overlay, and exec the command
/// inside it. Env and working dir come from the image config.
fn build_bootstrap(config: &amber_image::ImageConfig, argv: &[String]) -> std::io::Result<Vec<u8>> {
    let busybox = std::fs::read(guest::BUSYBOX)?;
    let musl = std::fs::read(guest::MUSL)?;
    let kernel_mods = first_module_dir()?;

    let mut init = String::new();
    init.push_str("#!/bin/busybox sh\n");
    init.push_str("/bin/busybox --install -s /bin\n");
    init.push_str("export PATH=/bin\n");
    init.push_str("mount -t proc proc /proc\n");
    init.push_str("mount -t sysfs sysfs /sys\n");
    init.push_str("mount -t devtmpfs dev /dev\n");
    for m in guest::MODULES {
        let name = Path::new(m).file_name().unwrap().to_str().unwrap();
        init.push_str(&format!("insmod /mod/{name}\n"));
    }
    init.push_str("mkdir -p /base /scratch /newroot\n");
    init.push_str("mount -t squashfs -o ro /dev/vda /base\n");
    init.push_str("mount -t tmpfs tmpfs /scratch\n");
    init.push_str("mkdir -p /scratch/u /scratch/w\n");
    init.push_str(
        "mount -t overlay overlay -o lowerdir=/base,upperdir=/scratch/u,workdir=/scratch/w /newroot\n",
    );
    init.push_str("mount -t devtmpfs dev /newroot/dev\n");
    init.push_str("mount -t proc proc /newroot/proc\n");
    init.push_str("mount -t sysfs sysfs /newroot/sys\n");
    // Image environment crosses chroot (it inherits our env).
    for e in &config.env {
        init.push_str(&format!("export {}\n", sh_quote(e)));
    }
    // Run the command inside the new root at the image's working dir.
    let cwd = config.working_dir.clone().unwrap_or_else(|| "/".into());
    let inner = format!(
        "cd {}; exec {}",
        sh_quote(&cwd),
        argv.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ")
    );
    init.push_str(&format!("chroot /newroot /bin/sh -c {}\n", sh_quote(&inner)));
    init.push_str("poweroff -f\n");

    let mut cpio = amber_image::Cpio::new();
    for d in ["bin", "lib", "mod", "dev", "proc", "sys"] {
        cpio.dir(d, 0o755);
    }
    cpio.file("bin/busybox", &busybox, 0o755);
    cpio.file("lib/ld-musl-aarch64.so.1", &musl, 0o755);
    cpio.symlink("lib/libc.musl-aarch64.so.1", "ld-musl-aarch64.so.1");
    for m in guest::MODULES {
        let name = Path::new(m).file_name().unwrap().to_str().unwrap();
        let data = std::fs::read(kernel_mods.join(m))?;
        cpio.file(&format!("mod/{name}"), &data, 0o644);
    }
    cpio.file("init", init.as_bytes(), 0o755);
    cpio.char_dev("dev/console", 0o600, 5, 1);
    cpio.char_dev("dev/null", 0o666, 1, 3);
    cpio.finish_gz()
}

/// `<MODULES_ROOT>/<version>/kernel` for the first kernel version present.
fn first_module_dir() -> std::io::Result<std::path::PathBuf> {
    let mut entries = std::fs::read_dir(guest::MODULES_ROOT)?;
    let first = entries
        .next()
        .ok_or_else(|| std::io::Error::other("no kernel modules dir"))??;
    Ok(first.path().join("kernel"))
}

/// Human-friendly age from whole seconds: `5s`, `3m`, `2h`.
fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// POSIX single-quote escaping: wrap in '...' and escape embedded quotes.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// RAII raw-mode for the host terminal, restored on drop.
struct RawTerm {
    saved: Option<libc::termios>,
}

impl RawTerm {
    fn enable() -> Self {
        unsafe {
            if libc::isatty(0) == 1 {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) == 0 {
                    let saved = t;
                    libc::cfmakeraw(&mut t);
                    libc::tcsetattr(0, libc::TCSANOW, &t);
                    return RawTerm { saved: Some(saved) };
                }
            }
        }
        RawTerm { saved: None }
    }
}

impl Drop for RawTerm {
    fn drop(&mut self) {
        if let Some(t) = self.saved {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run(vm: Vm) -> amber_core::Result<()> {
    log::info!("booting on Hypervisor.framework");
    vm.run::<amber_hvf::HvfVm>()
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn run(_vm: Vm) -> amber_core::Result<()> {
    Err(amber_core::Error::Backend(
        "no hypervisor backend on this target; amber-hvf needs macOS arm64, amber-kvm is M8".into(),
    ))
}
