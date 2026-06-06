//! M0 runner. Not the real CLI yet (that is M2). Just enough to prove the boot:
//!
//!   amber boot <kernel-Image> [initramfs]
//!
//! On macOS arm64 it boots the kernel on HVF and you should see the kernel's
//! earlycon output on stdout. Off macOS arm64 it builds everything except the
//! backend and tells you so.

use amber_core::{Vm, VmConfig};
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args[1] != "boot" {
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

    match run(vm) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("run failed: {e}");
            ExitCode::FAILURE
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
