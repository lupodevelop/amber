//! `amber.toml`: the declarative fleet manifest. Templates name an image and its
//! resources so `amber run <template>` works by name. This is the M2 manifest
//! slice — the daemon, warm pools, and budget enforcement come later; fields for
//! them are parsed (forward-compatible) but not yet acted on.

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
}

#[derive(Deserialize, Default)]
pub struct Template {
    pub image: String,
    /// Per-VM memory ceiling, e.g. "512MiB".
    pub ram_cap: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,

    // Forward-compatible: parsed now, acted on in later milestones.
    pub vcpus: Option<u32>,
    pub net: Option<String>,
    pub warm_pool: Option<u32>,
    pub snapshot: Option<String>,
    pub reuse: Option<bool>,
    pub timeout: Option<String>,
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
    Some((num * mult) as usize)
}
