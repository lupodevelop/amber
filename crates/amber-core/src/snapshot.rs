//! The on-disk snapshot format — an "amber".
//!
//! A snapshot is a directory: `mem.bin` (raw guest RAM), `gic.bin` (the backend's
//! interrupt-controller state blob), `cpu.json` (vcpu registers), and `meta.json`
//! (the memory layout). Capture is backend-driven (the register set is HVF- or
//! KVM-specific) through [`crate::Vcpu::capture`] / [`crate::Hypervisor::capture_gic`];
//! this module owns the format and the I/O. Restore (mapping it back into a fresh
//! VM) comes next.

use crate::{Error, GuestMemory, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Captured vcpu state. Backend-agnostic shape: general registers, the special
/// registers amber reads directly, an opaque (id, value) list of system
/// registers, and the SIMD/FP file. KVM would fill the same shape with its own
/// register ids.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct CpuSnapshot {
    pub x: Vec<u64>, // x0..x30
    pub pc: u64,
    pub cpsr: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    pub vtimer_offset: u64,
    /// Host monotonic clock (mach ticks) at capture, so the backend can keep the
    /// guest's virtual counter continuous across the process boundary on restore.
    pub mono: u64,
    pub sysregs: Vec<(u16, u64)>,
    pub fp: Vec<[u8; 16]>, // V0..V31
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Meta {
    pub mem_base: u64,
    pub mem_size: u64,
    /// Host path of the virtio-blk backing image to re-open on restore.
    pub disk: Option<String>,
}

/// Everything a snapshot directory holds except the bulk RAM (loaded separately).
pub struct Loaded {
    pub meta: Meta,
    pub cpu: CpuSnapshot,
    pub gic: Vec<u8>,
}

fn snap_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Snapshot(e.to_string())
}

/// Write a snapshot directory from the captured state.
pub fn write(
    dir: &Path,
    mem: &GuestMemory,
    cpu: &CpuSnapshot,
    gic: &[u8],
    disk: Option<&Path>,
) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(snap_err)?;

    // Raw guest RAM. SAFETY: the region is valid for `len` bytes and the guest is
    // stopped, so no concurrent writes.
    let ram = unsafe { std::slice::from_raw_parts(mem.host_ptr(), mem.len()) };
    std::fs::write(dir.join("mem.bin"), ram).map_err(snap_err)?;
    std::fs::write(dir.join("gic.bin"), gic).map_err(snap_err)?;

    let meta = Meta {
        mem_base: mem.base(),
        mem_size: mem.len() as u64,
        disk: disk.map(|p| p.to_string_lossy().into_owned()),
    };
    std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).map_err(snap_err)?)
        .map_err(snap_err)?;
    std::fs::write(dir.join("cpu.json"), serde_json::to_vec(cpu).map_err(snap_err)?)
        .map_err(snap_err)?;
    Ok(())
}

/// Read the metadata, cpu state, and GIC blob (not the bulk RAM) from a snapshot.
pub fn read(dir: &Path) -> Result<Loaded> {
    let meta = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).map_err(snap_err)?)
        .map_err(snap_err)?;
    let cpu = serde_json::from_slice(&std::fs::read(dir.join("cpu.json")).map_err(snap_err)?)
        .map_err(snap_err)?;
    let gic = std::fs::read(dir.join("gic.bin")).map_err(snap_err)?;
    Ok(Loaded { meta, cpu, gic })
}

/// Load the snapshot's RAM image into `mem`.
pub fn load_mem(dir: &Path, mem: &GuestMemory) -> Result<()> {
    let bytes = std::fs::read(dir.join("mem.bin")).map_err(snap_err)?;
    mem.write(mem.base(), &bytes)
}
