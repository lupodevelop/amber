//! M0 runner. Not the real CLI yet (that is M2). Just enough to prove the boot:
//!
//!   amber boot <kernel-Image> [initramfs]
//!
//! On macOS arm64 it boots the kernel on HVF and you should see the kernel's
//! earlycon output on stdout. Off macOS arm64 it builds everything except the
//! backend and tells you so.

use amber_core::{Vm, VmConfig};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("boot") => cmd_boot(&args),
        Some("pull") => cmd_pull(&args),
        Some("run") => cmd_run(&args),
        _ => {
            eprintln!("usage:");
            eprintln!("  amber run <image> [-- <argv>...]");
            eprintln!("  amber pull <image>");
            eprintln!("  amber boot <kernel-Image> [initramfs] [disk]");
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
    let cache = Path::new("amber-cache/blobs");
    let rootfs = Path::new("amber-cache/rootfs");
    let img = match amber_image::pull_and_flatten(image, cache, rootfs) {
        Ok(img) => img,
        Err(e) => {
            eprintln!("pull failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("rootfs: {}", img.rootfs.display());
    println!("default argv: {:?}", img.config.default_argv());
    if let Some(wd) = &img.config.working_dir {
        println!("workdir: {wd}");
    }
    println!("env entries: {}", img.config.env.len());

    let base = Path::new("amber-cache/base.sqfs");
    match amber_image::pack_squashfs(&img.rootfs, base) {
        Ok(()) => {
            let sz = std::fs::metadata(base).map(|m| m.len()).unwrap_or(0);
            println!("base: {} ({} KiB)", base.display(), sz / 1024);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pack failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `amber run <image> [-- argv...]`: pull and flatten the image, pack a squashfs
/// base, generate a bootstrap initramfs that mounts it under a tmpfs overlay and
/// execs the command, then boot. Cold boot, run once, tear down.
fn cmd_run(args: &[String]) -> ExitCode {
    let Some(image) = args.get(2) else {
        eprintln!("usage: amber run <image> [-- <argv>...]");
        return ExitCode::FAILURE;
    };
    // argv after a "--" separator overrides the image's default command.
    let user_argv: Vec<String> = match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].to_vec(),
        None => Vec::new(),
    };

    let cache = Path::new("amber-cache/blobs");
    let rootfs = Path::new("amber-cache/rootfs");
    let base = Path::new("amber-cache/base.sqfs");

    let img = match amber_image::pull_and_flatten(image, cache, rootfs) {
        Ok(img) => img,
        Err(e) => {
            eprintln!("pull failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = amber_image::pack_squashfs(&img.rootfs, base) {
        eprintln!("pack failed: {e}");
        return ExitCode::FAILURE;
    }

    let argv = if user_argv.is_empty() {
        img.config.default_argv()
    } else {
        user_argv
    };
    if argv.is_empty() {
        eprintln!("no command: the image declares none and none was given after --");
        return ExitCode::FAILURE;
    }

    let initrd = match build_bootstrap(&img.config, &argv) {
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

    let cfg = VmConfig {
        kernel,
        initrd: Some(initrd),
        disk: Some(base.to_path_buf()),
        ..Default::default()
    };
    let vm = match Vm::prepare(&cfg) {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("prepare failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let _raw = RawTerm::enable();
    match run(vm) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("run failed: {e}");
            ExitCode::FAILURE
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
