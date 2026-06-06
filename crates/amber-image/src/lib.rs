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
pub use pack::pack_squashfs;
pub use registry::Reference;

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

/// A pulled-and-flattened image: the rootfs tree on disk plus its run config.
pub struct Image {
    pub rootfs: PathBuf,
    pub config: ImageConfig,
}

/// Pull `reference`, caching blobs under `cache_dir`, and flatten its layers into
/// `rootfs` (created fresh). Returns the rootfs path and the image's run config.
pub fn pull_and_flatten(reference: &str, cache_dir: &Path, rootfs: &Path) -> Result<Image> {
    let reference = Reference::parse(reference)?;
    log::info!("pulling {}", reference);

    let mut client = registry::Client::new(&reference)?;
    let manifest = client.fetch_manifest()?;
    let config = client.fetch_config(&manifest.config_digest)?;

    std::fs::create_dir_all(cache_dir)?;
    let mut layer_paths = Vec::new();
    for (i, layer) in manifest.layers.iter().enumerate() {
        log::info!("layer {}/{} {}", i + 1, manifest.layers.len(), layer.digest);
        layer_paths.push(client.fetch_blob(&layer.digest, cache_dir)?);
    }

    flatten::flatten(&layer_paths, rootfs)?;
    log::info!("flattened {} layers into {}", layer_paths.len(), rootfs.display());

    Ok(Image {
        rootfs: rootfs.to_path_buf(),
        config,
    })
}

/// A ready-to-boot image: a packed read-only base plus its run config.
pub struct Built {
    pub base: PathBuf,
    pub config: ImageConfig,
}

/// Resolve, pull, flatten, and pack `reference` into a squashfs base, caching the
/// result by the image's content id (its config digest) under `cache_dir/bases`.
/// A cache hit skips the network, flatten, and pack entirely and goes straight to
/// boot — the closest thing to a warm start before snapshots exist.
pub fn build(reference: &str, cache_dir: &Path) -> Result<Built> {
    let reference = Reference::parse(reference)?;
    let mut client = registry::Client::new(&reference)?;
    let manifest = client.fetch_manifest()?;

    // The config digest is the content id: same image -> same base, cached.
    let id = manifest
        .config_digest
        .strip_prefix("sha256:")
        .unwrap_or(&manifest.config_digest);
    let bases = cache_dir.join("bases");
    std::fs::create_dir_all(&bases)?;
    let base = bases.join(format!("{id}.sqfs"));
    let cfg_path = bases.join(format!("{id}.json"));

    if base.exists() && cfg_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&cfg_path) {
            if let Ok(config) = serde_json::from_str(&text) {
                log::info!("image cache hit {}", &id[..id.len().min(12)]);
                return Ok(Built { base, config });
            }
        }
    }

    log::info!("building {reference}");
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
