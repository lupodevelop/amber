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
        Some("__lockdown-probe") => cmd_lockdown_probe(),
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
        Some("pause") => cmd_pause(&args),
        Some("resume") => cmd_resume(&args),
        Some("pull") => cmd_pull(&args),
        Some("disk") => cmd_disk(&args),
        Some("restore") => cmd_restore(&args),
        Some("template") => cmd_template(&args),
        Some("exec") => cmd_exec(&args),
        Some("fork") => cmd_fork(&args),
        Some("boot") => cmd_boot(&args),
        Some("help") | Some("-h") | Some("--help") | None => {
            print_help(&mut std::io::stdout());
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("amber: unknown command '{other}'\n");
            print_help(&mut std::io::stderr());
            ExitCode::FAILURE
        }
    }
}

/// The command reference, grouped by what you're doing.
fn print_help(w: &mut impl std::io::Write) {
    let _ = write!(
        w,
        "\
amber — fast, isolated microVM sandboxes on Apple Silicon

usage: amber <command> [args]

  run & exec
    run [-d] <image|template> [-- <argv>]   boot a microVM and run a command
                                            (-d: detached, prints an id)
    exec <template-dir> -- <command>        run a command in a warm fork
    template <image> <dir>                  build a ready-to-exec template
    fork [-i] <template-dir>                fork a template (-i: attach terminal)
    restore <snapshot-dir>                  resume a snapshot mid-execution

  daemon & fleet
    up | down                               start / stop the amberd daemon
    ps                                      list VMs (ID PID STATE AGE CAP RSS IMAGE)
    logs <id>                               stream a VM's output
    pause <id> | resume <id>                freeze / unfreeze a running VM
    rm <id>                                 kill a VM
    budget                                  fleet RAM: budget / reserved / real / host
    balloon <id> <MiB>                      ask a VM to give RAM back

  images & disks
    pull <image>                            pre-pull / refresh into the cache
    disk create <path> <size> [--raw]       make an ext4 data disk (mounts at /data)
    boot <kernel-Image> [initramfs] [disk]  boot a raw kernel (no OCI)

  env
    AMBER_NET=none        disable networking (on by default)
    AMBER_PORTS=8080:80   forward a host port to the guest
    AMBER_VCPUS=4         guest CPUs (1-8, default 1)
    AMBER_DISK=data.img   attach a data disk at /data (:ro for read-only)
    AMBER_VSOCK=/tmp/v.sock  guest<->host vsock over a unix socket (guest CID 3)
    AMBER_GIC=hw          use the in-kernel vGIC (no snapshot timer)

docs: README.md  ·  config: amber.toml
"
    );
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
            if vms.is_empty() {
                println!("no running VMs");
                return ExitCode::SUCCESS;
            }
            let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
            let now = proto::now_secs();
            let mib = |b: u64| format!("{}M", b / (1024 * 1024));
            // Color only on a terminal; in a pipe the output stays plain and parseable.
            let (bold, dim, rst) = if tty { ("\x1b[1m", "\x1b[2m", "\x1b[0m") } else { ("", "", "") };
            println!(
                "{bold}{:<8} {:<7} {:<8} {:>5} {:>6} {:>6}  IMAGE{rst}",
                "ID", "PID", "STATE", "AGE", "CAP", "RSS"
            );
            let paused = vms.iter().filter(|v| v.paused).count();
            for v in &vms {
                // Pad to the visible width first, then wrap in color, so the ANSI
                // escapes don't throw off column alignment.
                let cell = format!("{:<8}", if v.paused { "paused" } else { "running" });
                let state = if tty {
                    let color = if v.paused { "33" } else { "32" }; // yellow / green
                    format!("\x1b[{color}m{cell}\x1b[0m")
                } else {
                    cell
                };
                println!(
                    "{:<8} {:<7} {} {:>5} {:>6} {:>6}  {}",
                    v.id,
                    v.pid,
                    state,
                    fmt_age(now.saturating_sub(v.started)),
                    mib(v.ram_bytes),
                    mib(v.rss_bytes),
                    v.reference
                );
            }
            let foot = if paused > 0 {
                format!("{} VM(s), {paused} paused", vms.len())
            } else {
                format!("{} VM(s)", vms.len())
            };
            println!("{dim}{foot}{rst}");
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

/// Hidden self-test: apply the VMM lockdown, then verify the denies actually
/// bite (exec and filesystem writes must fail). Used by the integration suite.
fn cmd_lockdown_probe() -> ExitCode {
    if let Err(e) = (amber_core::lockdown::Policy::default()).apply() {
        eprintln!("lockdown apply failed: {e}");
        return ExitCode::FAILURE;
    }
    let exec_blocked = std::process::Command::new("/usr/bin/true").status().is_err();
    let write_blocked = std::fs::write("/tmp/amber-lockdown-probe", b"x").is_err();
    let _ = std::fs::remove_file("/tmp/amber-lockdown-probe"); // in case it got through
    if exec_blocked && write_blocked {
        println!("LOCKDOWN_OK");
        ExitCode::SUCCESS
    } else {
        eprintln!("LOCKDOWN_LEAK: exec_blocked={exec_blocked} write_blocked={write_blocked}");
        ExitCode::FAILURE
    }
}

/// `amber disk create <path> <size> [--raw]` — make a data-disk image and format
/// it ext4 (Firecracker-style, the host formats; the guest only mounts, at /data).
/// macOS has no mkfs.ext4, so formatting runs in the same Docker container the
/// kernel build uses; `--raw` (or no Docker) leaves a blank image for the guest
/// to format itself.
fn cmd_disk(args: &[String]) -> ExitCode {
    match args.get(2).map(String::as_str) {
        Some("create") => {
            let (Some(path), Some(size)) = (args.get(3), args.get(4)) else {
                eprintln!("usage: amber disk create <path> <size> [--raw]   (e.g. 1GiB)");
                return ExitCode::FAILURE;
            };
            let raw = args.iter().any(|a| a == "--raw");
            let Some(bytes) = manifest::parse_size(size) else {
                eprintln!("bad size '{size}' (try 512MiB, 2GiB, 1048576)");
                return ExitCode::FAILURE;
            };
            // Round up to a 512-byte sector so capacity is exact.
            let bytes = (bytes as u64).div_ceil(512) * 512;
            match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(f) => {
                    if let Err(e) = f.set_len(bytes) {
                        eprintln!("cannot size {path}: {e}");
                        return ExitCode::FAILURE;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    eprintln!("{path} already exists");
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!("cannot create {path}: {e}");
                    return ExitCode::FAILURE;
                }
            }
            let mib = bytes / (1024 * 1024);
            if raw {
                println!("created {path} ({mib} MiB, raw/unformatted). attach: AMBER_DISK={path}");
                return ExitCode::SUCCESS;
            }
            match mkfs_ext4(path) {
                Ok(()) => {
                    println!("created {path} ({mib} MiB, ext4). attach: AMBER_DISK={path} (mounts at /data)");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ext4 format failed ({e}); leaving a raw image (format it in the guest, or retry with Docker running)");
                    ExitCode::SUCCESS
                }
            }
        }
        _ => {
            eprintln!("usage: amber disk create <path> <size> [--raw]");
            ExitCode::FAILURE
        }
    }
}

/// Format an image ext4 via `mke2fs` in a Linux container (macOS has none).
fn mkfs_ext4(path: &str) -> std::io::Result<()> {
    let abs = std::fs::canonicalize(path)?;
    let dir = abs.parent().ok_or_else(|| std::io::Error::other("no parent dir"))?;
    let name = abs.file_name().unwrap().to_string_lossy().into_owned();
    let out = std::process::Command::new("docker")
        .args(["run", "--rm", "-v"])
        .arg(format!("{}:/work", dir.display()))
        .args(["debian:bookworm", "mke2fs", "-t", "ext4", "-F", "-q"])
        .arg(format!("/work/{name}"))
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

fn cmd_pause(args: &[String]) -> ExitCode {
    let Some(id) = args.get(2) else {
        eprintln!("usage: amber pause <id>");
        return ExitCode::FAILURE;
    };
    match daemon::pause(id) {
        Ok(()) => {
            println!("paused {id}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pause failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_resume(args: &[String]) -> ExitCode {
    let Some(id) = args.get(2) else {
        eprintln!("usage: amber resume <id>");
        return ExitCode::FAILURE;
    };
    match daemon::resume(id) {
        Ok(()) => {
            println!("resumed {id}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("resume failed: {e}");
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

    let vm = match Vm::prepare(&cfg, None) {
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
                // Template I/O caps flow to the devices via the env knobs they
                // read at construction (as plain bytes/s). An explicit env from
                // the operator wins over the manifest.
                for (var, cap) in
                    [("AMBER_DISK_BPS", &t.disk_bps), ("AMBER_NET_BPS", &t.net_bps)]
                {
                    if std::env::var_os(var).is_none() {
                        if let Some(b) = cap.as_deref().and_then(manifest::parse_size) {
                            std::env::set_var(var, b.to_string());
                        } else if cap.is_some() {
                            eprintln!("warning: bad {var} value for template '{target}', ignoring");
                        }
                    }
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

    // Data disks (persistent): AMBER_DISK=path[:ro][,path...] plus a template's
    // `disks`. Attached after the root as /dev/vdb, /dev/vdc, … Parsed before the
    // bootstrap so the init can mount them. Missing files are a hard error — a
    // typo'd disk path should fail loudly, not silently boot diskless.
    let mut data_disks: Vec<(std::path::PathBuf, bool)> = Vec::new();
    let mut parse_disk = |spec: &str| {
        let (path, writable) = match spec.strip_suffix(":ro") {
            Some(p) => (p, false),
            None => (spec, true),
        };
        data_disks.push((path.into(), writable));
    };
    if let Ok(list) = std::env::var("AMBER_DISK") {
        list.split(',').filter(|s| !s.is_empty()).for_each(&mut parse_disk);
    }
    if let Some(t) = manifest.as_ref().and_then(|m| m.template(target)) {
        t.disks.iter().for_each(|s| parse_disk(s));
    }
    for (d, _) in &data_disks {
        if !d.exists() {
            eprintln!("disk not found: {} (create one: amber disk create {} <size>)", d.display(), d.display());
            return ExitCode::FAILURE;
        }
    }

    let initrd = match build_bootstrap(&built.config, &argv, data_disks.len()) {
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
    // Guest CPUs: AMBER_VCPUS (operator) wins over the template's `vcpus`.
    cfg.vcpus = std::env::var("AMBER_VCPUS")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            manifest
                .as_ref()
                .and_then(|m| m.template(target))
                .and_then(|t| t.vcpus)
                .map(|n| n as usize)
        })
        .unwrap_or(1)
        .clamp(1, 8);
    cfg.data_disks = data_disks;
    // virtio-vsock: AMBER_VSOCK=<host-socket-path> opens a guest↔host channel.
    cfg.vsock = std::env::var("AMBER_VSOCK").ok().filter(|s| !s.is_empty()).map(Into::into);
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
    // Network backend (the host-side seam). For now AMBER_NET=capture wires the
    // bring-up backend that logs transmitted frames; smoltcp/gvproxy/vmnet/tap
    // slot in here. Default: no network device.
    // Networking is on by default (the software netstack); AMBER_NET=none opts out,
    // AMBER_NET=capture is the bring-up logger, any other value names a backend.
    let net: Option<Box<dyn amber_core::NetBackend>> = match std::env::var("AMBER_NET").as_deref() {
        Ok("none") | Ok("off") => None,
        Ok("capture") => Some(Box::new(amber_core::CaptureBackend)),
        Ok(kind) => amber_net::backend(kind),
        Err(_) => amber_net::backend("smoltcp"),
    };

    let t_prep_start = std::time::Instant::now();
    let vm = match Vm::prepare(&cfg, net) {
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
    // Provide a fresh network backend; restore_from uses it only if the template
    // had a net device (meta.net), keeping the device set aligned with the snapshot.
    let net = amber_net::backend("smoltcp");
    let mut vm = match Vm::restore_from(Path::new(dir), net) {
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

/// The in-guest agent (M7): announce readiness, read one command line, run it,
/// report its exit code with a marker the `exec` client parses, then power off.
/// A template snapshotted while this is blocked on `read` is "ready to exec".
const EXEC_AGENT: &str = "stty -echo 2>/dev/null; echo __AMBER_READY__; \
    IFS= read -r __c; sh -c \"$__c\"; __rc=$?; echo \"__AMBER_RC__$__rc\"; \
    echo 0 >/proc/sys/kernel/printk 2>/dev/null; poweroff -f";

/// `amber template <image> <dir>`: boot `image` running the exec agent and
/// snapshot it (on the software GIC) once the agent is ready — producing a
/// ready-to-exec template that `amber exec` forks.
fn cmd_template(args: &[String]) -> ExitCode {
    let (Some(image), Some(dir)) = (args.get(2), args.get(3)) else {
        eprintln!("usage: amber template <image> <dir>");
        return ExitCode::FAILURE;
    };
    // The template must restore on the software GIC (timer survives there), and we
    // snapshot once the agent has booted and is blocked reading for a command.
    std::env::set_var("AMBER_GIC", "sw");
    // Templates carry a network device so forked/exec'd sandboxes have internet;
    // the init auto-configures eth0. (The fork's restore provides a fresh backend.)
    std::env::set_var("AMBER_NET", "smoltcp");
    std::env::set_var("AMBER_SNAPSHOT", dir);
    std::env::set_var("AMBER_SNAPSHOT_AFTER_MS", "2500");
    let vm_args = vec![
        "amber".into(),
        "__vm".into(),
        image.clone(),
        "--".into(),
        "sh".into(),
        "-c".into(),
        EXEC_AGENT.into(),
    ];
    let r = cmd_vm(&vm_args);
    if matches!(r, ExitCode::SUCCESS) {
        eprintln!("template ready at {dir}: amber exec {dir} -- <command>");
    }
    r
}

/// `amber exec <template> -- <command>`: run a fresh command in a warm fork.
fn cmd_exec(args: &[String]) -> ExitCode {
    let split = args.iter().position(|a| a == "--");
    let Some(template) = args.get(2).filter(|_| split != Some(2)) else {
        eprintln!("usage: amber exec <template-dir> -- <command>");
        return ExitCode::FAILURE;
    };
    let cmd = match split {
        Some(i) => args[i + 1..].join(" "),
        None => String::new(),
    };
    if cmd.is_empty() {
        eprintln!("usage: amber exec <template-dir> -- <command>");
        return ExitCode::FAILURE;
    }
    match daemon::exec(template, &cmd) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("exec failed: {e}");
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

/// Guest asset paths. The kernel is resin (`make kernel`), built-in-everything so
/// no modules exist; with the borrowed Alpine kernel (`fetch-assets --alpine-kernel`)
/// a modules dir appears and the bootstrap insmods MODULES. busybox/musl are
/// borrowed Alpine artifacts until the single-binary bundling.
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
        // virtio-net and its failover dependencies (load deps first).
        "net/core/failover.ko",
        "drivers/net/net_failover.ko",
        "drivers/net/virtio_net.ko",
        "fs/squashfs/squashfs.ko",
        "fs/overlayfs/overlay.ko",
    ];
}

/// Build the gzipped cpio that bootstraps the image: load the virtio/fs modules,
/// mount the squashfs base read-only under a tmpfs overlay, mount any data disks
/// at /data, and exec the command inside it. Env and working dir come from the
/// image config.
fn build_bootstrap(
    config: &amber_image::ImageConfig,
    argv: &[String],
    data_disks: usize,
) -> std::io::Result<Vec<u8>> {
    let busybox = std::fs::read(guest::BUSYBOX)?;
    let musl = std::fs::read(guest::MUSL)?;
    // None => a built-in-everything kernel (resin): no modules to insmod.
    let kernel_mods = first_module_dir();

    let mut init = String::new();
    init.push_str("#!/bin/busybox sh\n");
    init.push_str("/bin/busybox --install -s /bin\n");
    init.push_str("export PATH=/bin\n");
    init.push_str("mount -t proc proc /proc\n");
    init.push_str("mount -t sysfs sysfs /sys\n");
    init.push_str("mount -t devtmpfs dev /dev\n");
    if kernel_mods.is_some() {
        for m in guest::MODULES {
            let name = Path::new(m).file_name().unwrap().to_str().unwrap();
            init.push_str(&format!("insmod /mod/{name}\n"));
        }
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
    // Data disks: mount pre-formatted ext4 images at /data, /data2, … Mount-only
    // (Firecracker-style: formatting is the host's job — `amber disk create`);
    // an unformatted or foreign disk stays accessible as the raw /dev/vdX.
    for i in 0..data_disks {
        let dev = format!("/dev/vd{}", (b'b' + i as u8) as char);
        let mp = if i == 0 { "/newroot/data".into() } else { format!("/newroot/data{}", i + 1) };
        // `noload` on the read-only fallback: an uncleanly-detached image has a
        // dirty journal, and ext4 refuses ro-mounts that would need replay.
        init.push_str(&format!(
            "mkdir -p {mp}; mount -t ext4 {dev} {mp} 2>/dev/null || \
             mount -t ext4 -o ro,noload {dev} {mp} 2>/dev/null || rmdir {mp} 2>/dev/null\n"
        ));
    }
    // Auto-configure networking when a virtio-net device is present (AMBER_NET set).
    // The static address and gateway must match amber-net's backend (guest
    // 10.0.0.2/24, gateway+resolver 10.0.0.1); resolv.conf goes into the chroot root
    // so the command sees it.
    init.push_str(
        "if [ -e /sys/class/net/eth0 ]; then \
         ip link set eth0 up; \
         ip addr add 10.0.0.2/24 dev eth0; \
         ip route add default via 10.0.0.1; \
         mkdir -p /newroot/etc; \
         echo 'nameserver 10.0.0.1' > /newroot/etc/resolv.conf; \
         fi\n",
    );
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
    // Unmount data disks cleanly before poweroff: flushes writes and leaves the
    // ext4 journal clean, so the image mounts read-only elsewhere without replay.
    if data_disks > 0 {
        init.push_str("sync\n");
        for i in 0..data_disks {
            let mp = if i == 0 { "/newroot/data".into() } else { format!("/newroot/data{}", i + 1) };
            init.push_str(&format!("umount {mp} 2>/dev/null\n"));
        }
    }
    init.push_str("poweroff -f\n");

    let mut cpio = amber_image::Cpio::new();
    for d in ["bin", "lib", "dev", "proc", "sys"] {
        cpio.dir(d, 0o755);
    }
    cpio.file("bin/busybox", &busybox, 0o755);
    cpio.file("lib/ld-musl-aarch64.so.1", &musl, 0o755);
    cpio.symlink("lib/libc.musl-aarch64.so.1", "ld-musl-aarch64.so.1");
    if let Some(kernel_mods) = &kernel_mods {
        cpio.dir("mod", 0o755);
        for m in guest::MODULES {
            let name = Path::new(m).file_name().unwrap().to_str().unwrap();
            let data = std::fs::read(kernel_mods.join(m))?;
            cpio.file(&format!("mod/{name}"), &data, 0o644);
        }
    }
    cpio.file("init", init.as_bytes(), 0o755);
    cpio.char_dev("dev/console", 0o600, 5, 1);
    cpio.char_dev("dev/null", 0o666, 1, 3);
    cpio.finish_gz()
}

/// `<MODULES_ROOT>/<version>/kernel`, or `None` when no modules ship — a built-in
/// kernel (resin) needs none, so the bootstrap skips insmod. Lets one binary boot
/// either the modular Alpine kernel or a built-in one, chosen by `assets/`.
fn first_module_dir() -> Option<std::path::PathBuf> {
    // The modules root holds a versioned dir (e.g. `6.12.81-0-virt`) and may also
    // hold a sibling `firmware` dir, so pick the entry that actually has a `kernel`
    // module tree rather than the first one read_dir happens to return.
    for e in std::fs::read_dir(guest::MODULES_ROOT).ok()?.flatten() {
        let kernel = e.path().join("kernel");
        if kernel.is_dir() {
            return Some(kernel);
        }
    }
    None
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
