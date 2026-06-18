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
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
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
    /// KVM backend: the full register file as `(KVM reg id, little-endian bytes)`
    /// from `KVM_GET_REG_LIST` (the HVF shape above doesn't fit KVM's id space or
    /// its variable-width registers). Empty for an HVF snapshot; the two never mix
    /// (the gic_kind guard refuses a cross-backend restore).
    #[serde(default)]
    pub kvm_regs: Vec<(u64, Vec<u8>)>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Meta {
    pub mem_base: u64,
    pub mem_size: u64,
    /// Host path of the virtio-blk backing image to re-open on restore.
    pub disk: Option<String>,
    /// The interrupt-controller kind the `gic.bin` blob belongs to ("v2"/"v3").
    /// The two formats are incompatible, so a restore must use the same backend
    /// GIC. `None` on snapshots taken before this was recorded.
    #[serde(default)]
    pub gic_kind: Option<String>,
    /// Whether a virtio-net device was present. A restore must recreate the same
    /// device set (so the virtio queue state lines up), so this drives whether the
    /// restored VM gets a network device.
    #[serde(default)]
    pub net: bool,
    /// Whether a virtio-vsock device was present — same reason as `net`: the
    /// restore must recreate it (with a fresh backend) in the same slot so the
    /// virtio queue order matches.
    #[serde(default)]
    pub vsock: bool,
    /// Guest CPUs captured. vcpu 0 lives in `cpu.json`, the rest in `cpu<i>.json`;
    /// a restore recreates all of them already running. Pre-SMP snapshots omit it.
    #[serde(default = "one")]
    pub vcpus: usize,
}

fn one() -> usize {
    1
}

/// Host-side device-emulation state that does not live in guest RAM and so must
/// be captured separately. Today: the PL011 config registers (so console input
/// works after a restore). Virtio queue registers are a further follow-up.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct DevState {
    /// PL011 [cr, imsc, ibrd, fbrd, lcr_h, ifls]; empty on pre-DevState snapshots.
    pub pl011: Vec<u32>,
    /// One entry per virtio-mmio device, in creation order. Empty on older
    /// snapshots (then the devices restore to their reset state).
    pub virtio: Vec<VirtioDevState>,
}

/// Host-side virtio-mmio state not in guest RAM: the negotiated status and, per
/// virtqueue, its ring addresses, ready flag, and the host's consumed index — all
/// reset by a fresh device, so a post-restore kick would otherwise hang.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct VirtioDevState {
    pub status: u32,
    pub interrupt_status: u32,
    /// Each queue as [num, ready, desc, avail, used, last_avail].
    pub queues: Vec<[u64; 6]>,
}

/// Everything a snapshot directory holds except the bulk RAM (loaded separately).
pub struct Loaded {
    pub meta: Meta,
    /// vcpu 0's registers (the primary).
    pub cpu: CpuSnapshot,
    /// Secondary vcpus' registers (cpu1..), in order. Empty on 1-vcpu snapshots.
    pub cpus: Vec<CpuSnapshot>,
    pub gic: Vec<u8>,
    pub dev: DevState,
}

fn snap_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Snapshot(e.to_string())
}

/// Write a snapshot directory from the captured state. `cpus[0]` is the primary
/// (written as `cpu.json`); the rest land in `cpu<i>.json`.
#[allow(clippy::too_many_arguments)]
pub fn write(
    dir: &Path,
    mem: &GuestMemory,
    cpus: &[CpuSnapshot],
    gic: &[u8],
    disk: Option<&Path>,
    gic_kind: Option<crate::GicKind>,
    dev: &DevState,
    net: bool,
    vsock: bool,
) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(snap_err)?;
    std::fs::write(dir.join("dev.json"), serde_json::to_vec(dev).map_err(snap_err)?)
        .map_err(snap_err)?;

    // Raw guest RAM. SAFETY: the region is valid for `len` bytes and every vcpu is
    // stopped (the snapshot barrier), so no concurrent writes.
    let ram = unsafe { std::slice::from_raw_parts(mem.host_ptr(), mem.len()) };
    std::fs::write(dir.join("mem.bin"), ram).map_err(snap_err)?;
    std::fs::write(dir.join("gic.bin"), gic).map_err(snap_err)?;

    let meta = Meta {
        mem_base: mem.base(),
        mem_size: mem.len() as u64,
        disk: disk.map(|p| p.to_string_lossy().into_owned()),
        gic_kind: gic_kind.map(|k| k.as_str().to_string()),
        net,
        vsock,
        vcpus: cpus.len().max(1),
    };
    std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).map_err(snap_err)?)
        .map_err(snap_err)?;
    for (i, cpu) in cpus.iter().enumerate() {
        let name = if i == 0 { "cpu.json".to_string() } else { format!("cpu{i}.json") };
        std::fs::write(dir.join(name), serde_json::to_vec(cpu).map_err(snap_err)?)
            .map_err(snap_err)?;
    }
    Ok(())
}

/// Read the metadata, cpu state(s), and GIC blob (not the bulk RAM) from a snapshot.
pub fn read(dir: &Path) -> Result<Loaded> {
    let meta: Meta =
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).map_err(snap_err)?)
            .map_err(snap_err)?;
    let cpu = serde_json::from_slice(&std::fs::read(dir.join("cpu.json")).map_err(snap_err)?)
        .map_err(snap_err)?;
    let mut cpus = Vec::new();
    for i in 1..meta.vcpus {
        let b = std::fs::read(dir.join(format!("cpu{i}.json"))).map_err(snap_err)?;
        cpus.push(serde_json::from_slice(&b).map_err(snap_err)?);
    }
    let gic = std::fs::read(dir.join("gic.bin")).map_err(snap_err)?;
    // dev.json is newer than the first snapshots; default it if absent, but a
    // present-but-corrupt file is an error (silently defaulting it would restore
    // devices to reset state and hang the guest after restore).
    let dev = match std::fs::read(dir.join("dev.json")) {
        Ok(b) => serde_json::from_slice(&b).map_err(snap_err)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DevState::default(),
        Err(e) => return Err(snap_err(e)),
    };
    Ok(Loaded { meta, cpu, cpus, gic, dev })
}

/// Load the snapshot's RAM image into `mem`.
pub fn load_mem(dir: &Path, mem: &GuestMemory) -> Result<()> {
    let bytes = std::fs::read(dir.join("mem.bin")).map_err(snap_err)?;
    mem.write(mem.base(), &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("amber-snap-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn meta_defaults_missing_fields() {
        // A pre-gic_kind/pre-net snapshot only had base+size.
        let m: Meta = serde_json::from_str(r#"{"mem_base":1024,"mem_size":2048}"#).unwrap();
        assert_eq!(m.mem_base, 1024);
        assert_eq!(m.mem_size, 2048);
        assert_eq!(m.gic_kind, None);
        assert!(!m.net);
        assert_eq!(m.disk, None);
        assert_eq!(m.vcpus, 1, "pre-SMP snapshots default to one vcpu");
    }

    #[test]
    fn meta_roundtrips() {
        let m = Meta {
            mem_base: 0x4000_0000,
            mem_size: 256 << 20,
            disk: Some("/tmp/base.img".into()),
            gic_kind: Some("v2".into()),
            net: true,
            vsock: false,
            vcpus: 2,
        };
        let back: Meta = serde_json::from_slice(&serde_json::to_vec(&m).unwrap()).unwrap();
        assert_eq!(back.mem_base, m.mem_base);
        assert_eq!(back.gic_kind.as_deref(), Some("v2"));
        assert!(back.net);
        assert_eq!(back.vcpus, 2);
    }

    #[test]
    fn devstate_roundtrips() {
        let d = DevState {
            pl011: vec![1, 2, 3, 4, 5, 6],
            virtio: vec![VirtioDevState {
                status: 0xb,
                interrupt_status: 1,
                queues: vec![[8, 1, 0x1000, 0x2000, 0x3000, 5]],
            }],
        };
        let back: DevState = serde_json::from_slice(&serde_json::to_vec(&d).unwrap()).unwrap();
        assert_eq!(back.pl011, d.pl011);
        assert_eq!(back.virtio.len(), 1);
        assert_eq!(back.virtio[0].queues[0], [8, 1, 0x1000, 0x2000, 0x3000, 5]);
    }

    #[test]
    fn read_defaults_devstate_when_absent() {
        let dir = tmpdir("nodev");
        std::fs::write(dir.join("meta.json"), br#"{"mem_base":0,"mem_size":0}"#).unwrap();
        std::fs::write(dir.join("cpu.json"), serde_json::to_vec(&CpuSnapshot::default()).unwrap()).unwrap();
        std::fs::write(dir.join("gic.bin"), b"").unwrap();
        // no dev.json
        let loaded = read(&dir).unwrap();
        assert!(loaded.dev.pl011.is_empty());
        assert!(loaded.dev.virtio.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_errors_on_corrupt_devstate() {
        let dir = tmpdir("baddev");
        std::fs::write(dir.join("meta.json"), br#"{"mem_base":0,"mem_size":0}"#).unwrap();
        std::fs::write(dir.join("cpu.json"), serde_json::to_vec(&CpuSnapshot::default()).unwrap()).unwrap();
        std::fs::write(dir.join("gic.bin"), b"").unwrap();
        std::fs::write(dir.join("dev.json"), b"{not json").unwrap();
        // A present-but-corrupt dev.json must error, not silently default.
        assert!(read(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_mem_rejects_image_larger_than_ram() {
        let dir = tmpdir("big");
        std::fs::write(dir.join("mem.bin"), vec![0u8; 0x2000]).unwrap();
        let mem = GuestMemory::new(0x4000_0000, 0x1000).unwrap(); // RAM smaller than image
        assert!(load_mem(&dir, &mem).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_mem_loads_fitting_image() {
        let dir = tmpdir("fit");
        let mut img = vec![0u8; 0x1000];
        img[0..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        std::fs::write(dir.join("mem.bin"), &img).unwrap();
        let mem = GuestMemory::new(0x4000_0000, 0x1000).unwrap();
        load_mem(&dir, &mem).unwrap();
        assert_eq!(mem.ram().read_u32(0x4000_0000), 0xefbe_adde);
        std::fs::remove_dir_all(&dir).ok();
    }
}
