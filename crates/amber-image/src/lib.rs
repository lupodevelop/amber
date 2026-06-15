//! `amber-image`: turn an OCI reference into a bootable rootfs.
//!
//! Pull the manifest, config, and layers from a registry; cache blobs by digest;
//! flatten the layers (honoring whiteouts) into a directory tree. Packing that
//! tree into an erofs base and wiring it to a guest is the caller's next step
//! (M1b). This crate is host-side and synchronous: it does the network and the
//! filesystem work, nothing about the VM.

pub mod cpio;
mod flatten;
mod pack;
mod registry;

use std::path::{Path, PathBuf};

pub use cpio::Cpio;
use registry::Reference;

#[derive(Debug)]
pub enum Error {
    Reference(String),
    Http(String),
    Registry(String),
    Json(String),
    Digest { want: String, got: String },
    Pack(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Reference(m) => write!(f, "bad image reference: {m}"),
            Error::Http(m) => write!(f, "http error: {m}"),
            Error::Registry(m) => write!(f, "registry error: {m}"),
            Error::Json(m) => write!(f, "json error: {m}"),
            Error::Digest { want, got } => write!(f, "digest mismatch: want {want}, got {got}"),
            Error::Pack(m) => write!(f, "pack error: {m}"),
            Error::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// What an image declares about how to run it: the bits `amber run` needs to
/// build a default command and environment when the caller does not override.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ImageConfig {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: Option<String>,
}

impl ImageConfig {
    /// The argv to run when the caller gives none: entrypoint followed by cmd.
    pub fn default_argv(&self) -> Vec<String> {
        let mut v = self.entrypoint.clone();
        v.extend(self.cmd.iter().cloned());
        v
    }
}

/// A ready-to-boot image: a packed read-only base plus its run config.
pub struct Built {
    pub base: PathBuf,
    pub config: ImageConfig,
}

/// Resolve, pull, flatten, and pack `reference` into a squashfs base, caching the
/// result by the image's content id (its config digest) under `cache_dir/bases`.
/// A cache hit skips the flatten and pack and goes straight to boot — the closest
/// thing to a warm start before snapshots exist.
///
/// With `refresh = false`, a remembered `reference -> id` mapping with a present
/// base is used **without touching the registry** (offline, fast — what `run`
/// wants). `refresh = true` always re-resolves against the registry and updates
/// the mapping (what `pull` does, so a moved tag is picked up).
pub fn build(reference: &str, cache_dir: &Path, refresh: bool) -> Result<Built> {
    let parsed = Reference::parse(reference)?;
    let bases = cache_dir.join("bases");
    std::fs::create_dir_all(&bases)?;
    let refs_path = cache_dir.join("refs.json");

    // Offline fast path: known id + present base, no network.
    if !refresh {
        if let Some(id) = ref_lookup(&refs_path, reference) {
            if let Some(built) = load_built(&bases, &id) {
                log::info!("image cache hit {} (offline)", short(&id));
                return Ok(built);
            }
        }
    }

    // Resolve against the registry to learn the current content id.
    let mut client = registry::Client::new(&parsed)?;
    let manifest = client.fetch_manifest()?;
    let id = manifest
        .config_digest
        .strip_prefix("sha256:")
        .unwrap_or(&manifest.config_digest)
        .to_string();
    ref_store(&refs_path, reference, &id);

    let base = bases.join(format!("{id}.sqfs"));
    let cfg_path = bases.join(format!("{id}.json"));
    if let Some(built) = load_built(&bases, &id) {
        log::info!("image cache hit {}", short(&id));
        return Ok(built);
    }

    log::info!("building {parsed}");
    let config = client.fetch_config(&manifest.config_digest)?;
    let blobs = cache_dir.join("blobs");
    std::fs::create_dir_all(&blobs)?;
    let mut layers = Vec::new();
    for (i, layer) in manifest.layers.iter().enumerate() {
        log::info!("layer {}/{} {}", i + 1, manifest.layers.len(), layer.digest);
        layers.push(client.fetch_blob(&layer.digest, &blobs)?);
    }

    // Concurrent builds (amberd spawns VMs in parallel) must not collide: flatten
    // into a per-build temp dir and pack to a temp file renamed into place.
    let stamp = std::process::id();
    let rootfs = bases.join(format!("rootfs-{id}.{stamp}"));
    let base_tmp = bases.join(format!("{id}.{stamp}.tmp"));
    flatten::flatten(&layers, &rootfs)?;
    pack::pack_squashfs(&rootfs, &base_tmp)?;
    std::fs::rename(&base_tmp, &base)?;
    let _ = std::fs::remove_dir_all(&rootfs);

    let json = serde_json::to_string(&config).map_err(|e| Error::Json(e.to_string()))?;
    std::fs::write(&cfg_path, json)?;

    Ok(Built { base, config })
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// Load a built image by content id if both its base and config are present.
fn load_built(bases: &Path, id: &str) -> Option<Built> {
    let base = bases.join(format!("{id}.sqfs"));
    let cfg = bases.join(format!("{id}.json"));
    if !base.exists() || !cfg.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&cfg).ok()?;
    let config = serde_json::from_str(&text).ok()?;
    Some(Built { base, config })
}

fn ref_lookup(refs_path: &Path, reference: &str) -> Option<String> {
    let text = std::fs::read_to_string(refs_path).ok()?;
    let map: std::collections::HashMap<String, String> = serde_json::from_str(&text).ok()?;
    map.get(reference).cloned()
}

/// Remember `reference -> id`, written atomically (concurrent builds race-safe).
fn ref_store(refs_path: &Path, reference: &str, id: &str) {
    let mut map: std::collections::HashMap<String, String> = std::fs::read_to_string(refs_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    map.insert(reference.to_string(), id.to_string());
    if let Ok(json) = serde_json::to_string(&map) {
        let tmp = refs_path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, refs_path);
        }
    }
}
