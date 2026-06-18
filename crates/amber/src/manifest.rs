//! `amber.toml`: the declarative fleet manifest. Templates name an image and its
//! resources so `amber run <template>` works by name; the fleet section sets the
//! RAM budget and warm-pool size the daemon enforces.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize, Default)]
pub struct Manifest {
    #[serde(default)]
    pub fleet: Fleet,
    #[serde(default)]
    pub template: HashMap<String, Template>,
}

#[derive(Deserialize, Default)]
pub struct Fleet {
    /// Hard ceiling for the sum of live VMs. Parsed; enforced once the budget
    /// scheduler exists (M5).
    pub ram_budget: Option<String>,
    /// How many warm forks to keep pre-staged per template (M4 warm pool). The
    /// pool tops up to this after each fork, bounded by the RAM budget. Default 1.
    pub pool_size: Option<usize>,
}

#[derive(Deserialize, Default)]
pub struct Template {
    pub image: String,
    /// Per-VM memory ceiling, e.g. "512MiB".
    pub ram_cap: Option<String>,
    /// I/O rate caps in bytes/second, e.g. "50MiB" (token bucket, 1 s burst).
    pub disk_bps: Option<String>,
    pub net_bps: Option<String>,
    /// Writable data disks attached as /dev/vdb, /dev/vdc, … (host image paths).
    #[serde(default)]
    pub disks: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Guest vCPUs (default 1).
    pub vcpus: Option<u32>,
}

impl Manifest {
    /// Load `amber.toml` from the current directory, if present.
    pub fn load() -> Option<Manifest> {
        Self::load_from(Path::new("amber.toml"))
    }

    pub fn load_from(path: &Path) -> Option<Manifest> {
        let text = std::fs::read_to_string(path).ok()?;
        match toml::from_str(&text) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("warning: ignoring {}: {e}", path.display());
                None
            }
        }
    }

    pub fn template(&self, name: &str) -> Option<&Template> {
        self.template.get(name)
    }
}

/// Parse a size like "512MiB", "4GiB", "1048576", "2GB". IEC (KiB/MiB/GiB) and
/// SI (KB/MB/GB) suffixes; a bare number is bytes. Case-insensitive.
pub fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.trim().parse().ok()?;
    if !num.is_finite() || num < 0.0 {
        return None;
    }
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kib" => 1024.0,
        "m" | "mib" => 1024.0 * 1024.0,
        "g" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "kb" => 1_000.0,
        "mb" => 1_000_000.0,
        "gb" => 1_000_000_000.0,
        _ => return None,
    };
    let bytes = (num * mult) as usize;
    (bytes > 0).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn parse_size_iec_units() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("512b"), Some(512));
        assert_eq!(parse_size("1k"), Some(1024));
        assert_eq!(parse_size("1KiB"), Some(1024));
        assert_eq!(parse_size("1m"), Some(1024 * 1024));
        assert_eq!(parse_size("1g"), Some(1024 * 1024 * 1024));
    }

    #[test]
    fn parse_size_si_units() {
        assert_eq!(parse_size("1kb"), Some(1000));
        assert_eq!(parse_size("1mb"), Some(1_000_000));
    }

    #[test]
    fn parse_size_decimals_and_whitespace() {
        assert_eq!(parse_size("2.5m"), Some(2_621_440));
        assert_eq!(parse_size("  10 MiB "), Some(10 * 1024 * 1024));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert_eq!(parse_size("0"), None); // zero is not a useful size
        assert_eq!(parse_size("-5m"), None); // negative
        assert_eq!(parse_size("abc"), None); // no leading number
        assert_eq!(parse_size("5x"), None); // unknown unit
        assert_eq!(parse_size("1e400"), None); // 'e400' is not a known unit
    }
}
