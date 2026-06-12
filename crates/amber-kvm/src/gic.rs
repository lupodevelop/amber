//! vGICv3 save/restore for KVM snapshots.
//!
//! KVM exposes the in-kernel vGICv3 state through device attributes, one group
//! per register bank (`KVM_DEV_ARM_VGIC_GRP_*`). We walk a deterministic list of
//! (group, attr) addresses, read each value out on capture, and write them back
//! on restore. The blob stores only the values — restore regenerates the same
//! address list from `(nr_irqs, vcpus)` and zips it back, so the ordering is the
//! single source of truth.
//!
//! Banks covered: the distributor (SPIs), the per-vcpu redistributor + SGI frame
//! (SGIs/PPIs), and the per-vcpu ICC system registers (CPU interface). LPIs/ITS
//! are not configured by amber, so the LPI pending tables and `GRP_LEVEL_INFO`
//! are intentionally omitted: a snapshot is taken at a quiescent point (the guest
//! parked in WFI) where level-triggered lines are deasserted.

use amber_core::{Error, Result};
use kvm_bindings::*;
use kvm_ioctls::DeviceFd;

/// One register slot: which attribute group and the attr selector (offset, or
/// `(vcpu << 32) | offset`, or `(vcpu << 32) | sysreg-encoding`).
type Addr = (u32, u64);

fn get_attr(vgic: &DeviceFd, group: u32, attr: u64) -> Result<u64> {
    let mut val: u64 = 0;
    let mut kda = kvm_device_attr {
        group,
        attr,
        addr: &mut val as *mut u64 as u64,
        flags: 0,
    };
    // SAFETY: addr points at `val`, large enough for any GIC register (≤ 64-bit),
    // and KVM writes at most that register's width into it.
    unsafe { vgic.get_device_attr(&mut kda) }
        .map_err(|e| Error::Snapshot(format!("get vgic attr g{group}/{attr:#x}: {e}")))?;
    Ok(val)
}

fn set_attr(vgic: &DeviceFd, group: u32, attr: u64, val: u64) -> Result<()> {
    let kda = kvm_device_attr {
        group,
        attr,
        addr: &val as *const u64 as u64,
        flags: 0,
    };
    vgic.set_device_attr(&kda)
        .map_err(|e| Error::Snapshot(format!("set vgic attr g{group}/{attr:#x}: {e}")))
}

/// ICC system register attr encoding (op0/op1/crn/crm/op2 packed as KVM expects).
const fn icc(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

/// The ICC_* CPU-interface registers KVM saves per vcpu (all 64-bit).
const ICC_REGS: &[u64] = &[
    icc(3, 0, 4, 6, 0),  // ICC_PMR_EL1
    icc(3, 0, 12, 8, 3), // ICC_BPR0_EL1
    // Only APnR0 exists with 5 implemented priority bits (KVM/QEMU virt); the
    // higher AP0R1-3 / AP1R1-3 are absent and KVM rejects them with EINVAL.
    icc(3, 0, 12, 8, 4),  // ICC_AP0R0_EL1
    icc(3, 0, 12, 9, 0),  // ICC_AP1R0_EL1
    icc(3, 0, 12, 12, 3), // ICC_BPR1_EL1
    icc(3, 0, 12, 12, 4), // ICC_CTLR_EL1
    icc(3, 0, 12, 12, 5), // ICC_SRE_EL1
    icc(3, 0, 12, 12, 6), // ICC_IGRPEN0_EL1
    icc(3, 0, 12, 12, 7), // ICC_IGRPEN1_EL1
];

/// Build the full, deterministic register-address list for `(nr_irqs, vcpus)`.
/// Capture and restore both regenerate this so the blob only needs the values.
fn addr_list(nr_irqs: u32, vcpus: usize) -> Vec<Addr> {
    let mut a = Vec::new();
    let dist = KVM_DEV_ARM_VGIC_GRP_DIST_REGS;

    // Distributor (SPIs only; SGI/PPI live in the redistributor). The bitmap and
    // priority/config banks skip the first words, which cover the private INTIDs
    // 0..31 and are handled per-vcpu below.
    a.push((dist, 0x0000)); // GICD_CTLR
    for base in [0x0080u64, 0x0100, 0x0200, 0x0300] {
        // IGROUPR/ISENABLER/ISPENDR/ISACTIVER: 1 bit/IRQ → 32 IRQs per word.
        for word in 1..(nr_irqs / 32) as u64 {
            a.push((dist, base + 4 * word));
        }
    }
    for word in 8..(nr_irqs / 4) as u64 {
        a.push((dist, 0x0400 + 4 * word)); // GICD_IPRIORITYR: 8 bits/IRQ
    }
    for word in 2..(nr_irqs / 16) as u64 {
        a.push((dist, 0x0C00 + 4 * word)); // GICD_ICFGR: 2 bits/IRQ
    }
    for intid in 32..nr_irqs as u64 {
        a.push((dist, 0x6000 + 8 * intid)); // GICD_IROUTER: 64-bit, per SPI
    }

    // Per-vcpu redistributor (RD frame + SGI frame) and ICC system registers.
    let redist = KVM_DEV_ARM_VGIC_GRP_REDIST_REGS;
    let sysregs = KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS;
    for cpu in 0..vcpus as u64 {
        let v = cpu << 32;
        for off in [0x0000u64, 0x0010, 0x0014] {
            // GICR_CTLR/STATUSR/WAKER. PROPBASER/PENDBASER are LPI config — amber
            // runs no LPIs/ITS, and their constrained fields don't round-trip.
            a.push((redist, v | off));
        }
        // SGI frame (offset 0x10000): the private-IRQ banks for this vcpu.
        a.push((redist, v | 0x10080)); // GICR_IGROUPR0
        a.push((redist, v | 0x10100)); // GICR_ISENABLER0
        a.push((redist, v | 0x10200)); // GICR_ISPENDR0
        a.push((redist, v | 0x10300)); // GICR_ISACTIVER0
        for word in 0..8u64 {
            a.push((redist, v | (0x10400 + 4 * word))); // GICR_IPRIORITYR0..7
        }
        a.push((redist, v | 0x10C00)); // GICR_ICFGR0
        a.push((redist, v | 0x10C04)); // GICR_ICFGR1

        for &reg in ICC_REGS {
            a.push((sysregs, v | reg));
        }
    }
    a
}

/// Fast, boot-free round-trip check: poke a few writable registers, capture,
/// clobber them, capture again to prove the clobber took, restore, and capture a
/// third time — the third blob must match the first byte-for-byte. Returns the
/// register count on success.
pub fn selftest(vgic: &DeviceFd, nr_irqs: u32, vcpus: usize) -> Result<usize> {
    let dist = KVM_DEV_ARM_VGIC_GRP_DIST_REGS;
    let pokes: &[Addr] = &[(dist, 0x0104), (dist, 0x0204), (dist, 0x0420)];
    for &(g, a) in pokes {
        set_attr(vgic, g, a, 0xa5a5_a5a5)?;
    }
    let state_a = capture(vgic, nr_irqs, vcpus)?;
    for &(g, a) in pokes {
        set_attr(vgic, g, a, 0)?;
    }
    let state_b = capture(vgic, nr_irqs, vcpus)?;
    if state_a == state_b {
        return Err(Error::Snapshot("gic selftest: clobber had no effect".into()));
    }
    restore(vgic, nr_irqs, vcpus, &state_a)?;
    let state_c = capture(vgic, nr_irqs, vcpus)?;
    if state_a != state_c {
        let addrs = addr_list(nr_irqs, vcpus);
        for (i, (g, attr)) in addrs.iter().enumerate() {
            let o = 20 + i * 8;
            let (va, vc) = (&state_a[o..o + 8], &state_c[o..o + 8]);
            if va != vc {
                return Err(Error::Snapshot(format!(
                    "gic selftest: g{g}/{attr:#x} {:#x} -> {:#x} after restore",
                    u64::from_le_bytes(va.try_into().unwrap()),
                    u64::from_le_bytes(vc.try_into().unwrap()),
                )));
            }
        }
        return Err(Error::Snapshot("gic selftest: blobs differ in header".into()));
    }
    Ok((state_a.len() - 20) / 8)
}

const MAGIC: &[u8; 4] = b"AGIC";

/// Read the whole vGIC register file into an opaque, self-describing blob.
pub fn capture(vgic: &DeviceFd, nr_irqs: u32, vcpus: usize) -> Result<Vec<u8>> {
    let addrs = addr_list(nr_irqs, vcpus);
    let mut out = Vec::with_capacity(16 + addrs.len() * 8);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&1u32.to_le_bytes()); // version
    out.extend_from_slice(&nr_irqs.to_le_bytes());
    out.extend_from_slice(&(vcpus as u32).to_le_bytes());
    out.extend_from_slice(&(addrs.len() as u32).to_le_bytes());
    for (g, attr) in addrs {
        out.extend_from_slice(&get_attr(vgic, g, attr)?.to_le_bytes());
    }
    Ok(out)
}

/// Restore the vGIC register file from a blob produced by [`capture`]. The vGIC
/// must already be created and INIT'd (it is, after `create_vcpu(0)`).
pub fn restore(vgic: &DeviceFd, nr_irqs: u32, vcpus: usize, blob: &[u8]) -> Result<()> {
    if blob.len() < 20 || &blob[0..4] != MAGIC {
        return Err(Error::Snapshot("gic blob: bad magic/length".into()));
    }
    let rd = |o: usize| u32::from_le_bytes(blob[o..o + 4].try_into().unwrap());
    let (b_irqs, b_vcpus, count) = (rd(8), rd(12), rd(16) as usize);
    if b_irqs != nr_irqs || b_vcpus as usize != vcpus {
        return Err(Error::Snapshot(format!(
            "gic blob shape mismatch: irqs {b_irqs}/{nr_irqs} vcpus {b_vcpus}/{vcpus}"
        )));
    }
    let addrs = addr_list(nr_irqs, vcpus);
    if count != addrs.len() || blob.len() < 20 + count * 8 {
        return Err(Error::Snapshot("gic blob: register count mismatch".into()));
    }
    for (i, (g, attr)) in addrs.into_iter().enumerate() {
        let o = 20 + i * 8;
        let val = u64::from_le_bytes(blob[o..o + 8].try_into().unwrap());
        // A handful of bits are RO/WI (e.g. parts of GICR_CTLR); a rejected write
        // means the kernel already holds the right value, so warn and continue.
        if let Err(e) = set_attr(vgic, g, attr, val) {
            log::warn!("kvm gic restore: skip g{g}/{attr:#x}: {e}");
        }
    }
    Ok(())
}
