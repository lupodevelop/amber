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
pub const HV_REG_FPCR: hv_reg_t = 32;
pub const HV_REG_FPSR: hv_reg_t = 33;
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
    // Force the listed vcpus out of hv_vcpu_run from another thread — the timer
    // preemption thread uses it to deliver a tick to a compute-bound guest.
    pub fn hv_vcpus_exit(vcpus: *const hv_vcpu_t, vcpu_count: u32) -> hv_return_t;

    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: u64) -> hv_return_t;

    pub fn hv_vcpu_get_sys_reg(vcpu: hv_vcpu_t, reg: hv_sys_reg_t, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_sys_reg(vcpu: hv_vcpu_t, reg: hv_sys_reg_t, value: u64) -> hv_return_t;

    // VTimer. CNTVCT_EL0 == mach_absolute_time() - vtimer_offset, so the offset
    // lets the WFI handler compute how long until the next tick is due.
    pub fn hv_vcpu_get_vtimer_offset(vcpu: hv_vcpu_t, vtimer_offset: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: hv_vcpu_t, vtimer_is_masked: bool) -> hv_return_t;

    // GICv3 (macOS 15+). The config object is built, applied with hv_gic_create
    // after the VM exists but before any vcpu, then discarded.
    pub fn hv_gic_config_create() -> hv_gic_config_t;
    pub fn hv_gic_config_set_distributor_base(config: hv_gic_config_t, base: hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_config_set_redistributor_base(config: hv_gic_config_t, base: hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_create(config: hv_gic_config_t) -> hv_return_t;
    pub fn hv_gic_get_distributor_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_region_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_base_alignment(align: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> hv_return_t;
    // Set a shared peripheral interrupt's level by absolute INTID.
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t;
    // Raise/lower the vcpu's IRQ (or FIQ) line directly. The software GICv2 uses
    // this to deliver interrupts without an in-kernel vGIC. NOTE: HVF clears the
    // pending interrupt after each hv_vcpu_run, so it must be set before every run.
    pub fn hv_vcpu_set_pending_interrupt(
        vcpu: hv_vcpu_t,
        irq_type: hv_interrupt_type_t,
        pending: bool,
    ) -> hv_return_t;

    // --- snapshot capture ---
    // SIMD/FP registers Q0..Q31, 16 bytes each. The `value` out-pointer makes the
    // ABI a plain 16-byte write (no NEON-vector-by-value hazard); used for capture.
    pub fn hv_vcpu_get_simd_fp_reg(vcpu: hv_vcpu_t, reg: hv_simd_fp_reg_t, value: *mut [u8; 16]) -> hv_return_t;
    // GIC state is an opaque object: create a snapshot, query its size, copy bytes.
    pub fn hv_gic_state_create() -> hv_gic_state_t;
    pub fn hv_gic_state_get_size(state: hv_gic_state_t, size: *mut usize) -> hv_return_t;
    pub fn hv_gic_state_get_data(state: hv_gic_state_t, data: *mut c_void) -> hv_return_t;

    // --- snapshot restore ---
    pub fn hv_gic_set_state(data: *const c_void, size: usize) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, offset: u64) -> hv_return_t;
    // NOTE: hv_vcpu_set_simd_fp_reg takes the value by NEON vector, which stable
    // Rust cannot pass across FFI (simd_ffi is nightly). Restoring V0..V31 needs a
    // small C shim; deferred — the FP file is don't-care for the resume proof.
}

/// SIMD/FP register id; Q0..Q31 are 0..=31.
pub type hv_simd_fp_reg_t = u32;
/// Opaque GIC state object (struct hv_gic_state_s *).
pub type hv_gic_state_t = *mut c_void;

// hv_sys_reg_t == uint16_t. Only the few amber reads/writes are named here.
pub type hv_sys_reg_t = u16;
pub const HV_SYS_REG_MPIDR_EL1: hv_sys_reg_t = 0xc005;
pub const HV_SYS_REG_CNTV_CTL_EL0: hv_sys_reg_t = 0xdf19; // bit0 ENABLE, bit1 IMASK, bit2 ISTATUS
pub const HV_SYS_REG_CNTV_CVAL_EL0: hv_sys_reg_t = 0xdf1a;

// hv_interrupt_type_t: IRQ = 0, FIQ = 1.
pub type hv_interrupt_type_t = u32;
pub const HV_INTERRUPT_TYPE_IRQ: hv_interrupt_type_t = 0;

/// Opaque GIC configuration object (struct hv_gic_config_s *).
pub type hv_gic_config_t = *mut c_void;

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
