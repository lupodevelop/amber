//! Raw FFI to Hypervisor.framework, arm64 (<Hypervisor/hv.h>, hv_vcpu.h).
//!
//! Kept in one isolated module on purpose: it is the only part of amber that is
//! unsafe and SDK-version-sensitive. If you would rather not hand-maintain this,
//! the `applevisor` crate wraps the same API and `lib.rs` could be retargeted to
//! it without touching anything in amber-core.
//!
//! Constants marked VERIFY are the enum discriminants most likely to drift
//! between SDK versions. Check them against the headers in
//! `$(xcrun --show-sdk-path)/usr/include/Hypervisor/` before trusting a build.

#![allow(non_camel_case_types, non_upper_case_globals, dead_code)]

use libc::{c_int, c_void};

pub type hv_return_t = c_int;
pub const HV_SUCCESS: hv_return_t = 0;

pub type hv_vcpu_t = u64;
pub type hv_ipa_t = u64;

// hv_memory_flags_t bitfield.
pub const HV_MEMORY_READ: u64 = 1 << 0;
pub const HV_MEMORY_WRITE: u64 = 1 << 1;
pub const HV_MEMORY_EXEC: u64 = 1 << 2;

// hv_reg_t. X0..X30 are 0..=30. VERIFY: PC and CPSR discriminants.
pub type hv_reg_t = u32;
pub const HV_REG_X0: hv_reg_t = 0;
pub const HV_REG_PC: hv_reg_t = 31; // VERIFY
pub const HV_REG_CPSR: hv_reg_t = 34; // VERIFY

// hv_exit_reason_t. VERIFY all four against hv_vcpu_types.h.
pub type hv_exit_reason_t = u32;
pub const HV_EXIT_REASON_CANCELED: hv_exit_reason_t = 0; // VERIFY
pub const HV_EXIT_REASON_EXCEPTION: hv_exit_reason_t = 1; // VERIFY
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: hv_exit_reason_t = 2; // VERIFY
pub const HV_EXIT_REASON_UNKNOWN: hv_exit_reason_t = 3; // VERIFY

#[repr(C)]
#[derive(Clone, Copy)]
pub struct hv_vcpu_exit_exception_t {
    pub syndrome: u64,         // ESR
    pub virtual_address: u64,  // far
    pub physical_address: u64, // faulting IPA for a stage-2 abort
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct hv_vcpu_exit_t {
    pub reason: hv_exit_reason_t,
    pub exception: hv_vcpu_exit_exception_t,
}

pub type hv_vm_config_t = *mut c_void; // null is fine on arm64
pub type hv_vcpu_config_t = *mut c_void; // null is fine

extern "C" {
    pub fn hv_vm_create(config: hv_vm_config_t) -> hv_return_t;
    pub fn hv_vm_destroy() -> hv_return_t;
    pub fn hv_vm_map(addr: *mut c_void, ipa: hv_ipa_t, size: usize, flags: u64) -> hv_return_t;

    pub fn hv_vcpu_create(
        vcpu: *mut hv_vcpu_t,
        exit: *mut *mut hv_vcpu_exit_t,
        config: hv_vcpu_config_t,
    ) -> hv_return_t;
    pub fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;

    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: u64) -> hv_return_t;
}

/// ESR exception class for a trapped HVC (PSCI calls arrive this way).
/// EC == 0x16 for HVC executed at EL1. VERIFY against the ARM ARM if unsure.
pub const EC_HVC64: u64 = 0x16;

/// ESR exception class for a trapped MSR/MRS/System instruction in AArch64.
/// EC == 0x18. HVF traps sysregs amber does not model yet; the run loop makes
/// them inert so an M0 boot can reach its no-GIC ceiling instead of faulting.
pub const EC_MSR_TRAP: u64 = 0x18;

/// ESR exception class for a trapped WFI/WFE. EC == 0x01. HVF delivers a guest
/// WFI as this exception rather than a CANCELED/VTIMER idle exit, so the run loop
/// treats it as the idle signal: the guest is parked waiting for an interrupt
/// that, without a GIC, will not arrive. That is the M0 ceiling.
pub const EC_WFX: u64 = 0x01;
