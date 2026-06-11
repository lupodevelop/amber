//! The Linux/KVM backend: implements amber-core's `Hypervisor`/`Vcpu` traits via
//! `/dev/kvm` (through the rust-vmm `kvm-ioctls` crate). Linux-only; on every
//! other target this crate is empty so the workspace still builds.
//!
//! Unlike the HVF backend, KVM provides the interrupt controller (in-kernel
//! vGICv3) and the arch timer itself, with a complete save/restore surface — so
//! there is no software GIC, no timer-preemption thread, and the post-restore
//! timer simply works. The backend is correspondingly thin.

#![cfg(target_os = "linux")]

mod backend;
pub use backend::{selftest, KvmVm};
