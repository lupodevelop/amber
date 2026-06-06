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
        _ => {
            eprintln!("usage:");
            eprintln!("  amber boot <kernel-Image> [initramfs]");
            eprintln!("  amber pull <image>");
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

    let cfg = VmConfig { kernel, initrd, ..Default::default() };

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
    match amber_image::pull_and_flatten(image, cache, rootfs) {
        Ok(img) => {
            println!("rootfs: {}", img.rootfs.display());
            let argv = img.config.default_argv();
            println!("default argv: {argv:?}");
            if let Some(wd) = &img.config.working_dir {
                println!("workdir: {wd}");
            }
            println!("env entries: {}", img.config.env.len());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pull failed: {e}");
            ExitCode::FAILURE
        }
    }
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
