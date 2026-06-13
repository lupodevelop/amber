//! CPU templates: normalize the guest-visible feature registers.
//!
//! arm64 advertises CPU features in the `ID_AA64*` system registers. Both backends
//! virtualize these — HVF and KVM trap the guest's reads and return what the VMM
//! sets — so masking a feature field here removes it from the guest's view (it
//! falls back to a software path). A template is a set of read-modify-write masks
//! applied to the boot vcpu before the first instruction; the snapshot captures
//! the result, so a fork inherits the same CPU. Useful for a deterministic CPU
//! regardless of host, and for restoring a snapshot on a different machine.
//!
//! Registers are addressed by the bare ARM encoding `(op0<<14)|(op1<<11)|
//! (crn<<7)|(crm<<3)|op2` — the value HVF's `hv_sys_reg_t` uses directly and the
//! low bits KVM wraps in its `ARM64_SYSREG` index.

/// `(op0<<14)|(op1<<11)|(crn<<7)|(crm<<3)|op2`.
const fn enc(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

pub const ID_AA64ISAR0_EL1: u32 = enc(3, 0, 0, 6, 0);

/// One read-modify-write on a feature register: `new = (old & and_mask) | or_value`.
#[derive(Clone, Copy, Debug)]
pub struct Override {
    pub reg: u32,
    pub and_mask: u64,
    pub or_value: u64,
}

/// A named set of feature-register overrides.
#[derive(Clone, Debug)]
pub struct CpuTemplate {
    pub name: &'static str,
    pub overrides: &'static [Override],
}

// ID_AA64ISAR0_EL1 crypto fields: AES[7:4], SHA1[11:8], SHA2[15:12] and
// SHA3[35:32], SM3[39:36], SM4[43:40]. Clearing them hides the crypto extensions.
const CRYPTO_FIELDS: u64 = (0xfff << 4) | (0xfff << 32);

const NO_CRYPTO: &[Override] = &[Override {
    reg: ID_AA64ISAR0_EL1,
    and_mask: !CRYPTO_FIELDS,
    or_value: 0,
}];

/// Built-in templates. `host` is passthrough (the default).
const TEMPLATES: &[CpuTemplate] = &[
    CpuTemplate { name: "host", overrides: &[] },
    CpuTemplate { name: "no-crypto", overrides: NO_CRYPTO },
];

/// Look up a template by name (case-sensitive).
pub fn by_name(name: &str) -> Option<&'static CpuTemplate> {
    TEMPLATES.iter().find(|t| t.name == name)
}

/// The names of all built-in templates, for help text.
pub fn names() -> impl Iterator<Item = &'static str> {
    TEMPLATES.iter().map(|t| t.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isar0_encoding_is_canonical() {
        // ID_AA64ISAR0_EL1 = S3_0_C0_C6_0 → 0xc030 (matches HVF's hv_sys_reg_t).
        assert_eq!(ID_AA64ISAR0_EL1, 0xc030);
    }

    #[test]
    fn no_crypto_clears_only_crypto_fields() {
        let ov = NO_CRYPTO[0];
        // A register with crypto bits set and a non-crypto field (bit 20, atomics).
        let host = (0x1 << 4) | (0x2 << 8) | (0x1 << 32) | (0x2 << 20);
        let masked = (host & ov.and_mask) | ov.or_value;
        assert_eq!(masked & CRYPTO_FIELDS, 0, "crypto fields cleared");
        assert_eq!(masked & (0xf << 20), 0x2 << 20, "non-crypto field preserved");
    }

    #[test]
    fn host_is_passthrough() {
        assert!(by_name("host").unwrap().overrides.is_empty());
        assert!(by_name("nope").is_none());
    }
}
