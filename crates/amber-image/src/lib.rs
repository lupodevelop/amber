//! `amber-image`: turn an OCI reference into a bootable rootfs.
//!
//! Pull the manifest, config, and layers from a registry; cache blobs by digest;
//! flatten the layers (honoring whiteouts) into a directory tree. Packing that
//! tree into an erofs base and wiring it to a guest is the caller's next step
//! (M1b). This crate is host-side and synchronous: it does the network and the
//! filesystem work, nothing about the VM.

mod flatten;
mod registry;

use std::path::{Path, PathBuf};

pub use registry::Reference;

#[derive(Debug)]
pub enum Error {
    Reference(String),
    Http(String),
    Registry(String),
    Json(String),
    Digest { want: String, got: String },
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
#[derive(Debug, Clone, Default)]
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
