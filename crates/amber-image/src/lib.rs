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

use std::io;
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
    // A reference that names a file on disk is a local `docker save` tar, not a
    // registry ref: load it offline (no network, no push).
    if Path::new(reference).is_file() {
        return build_local(reference, cache_dir);
    }
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

/// Build from a local `docker save` tar (an OCI archive carrying a classic
/// `manifest.json`). Lets a dev boot a locally-built image with no registry.
fn build_local(tar_path: &str, cache_dir: &Path) -> Result<Built> {
    let bases = cache_dir.join("bases");
    let blobs = cache_dir.join("blobs");
    std::fs::create_dir_all(&bases)?;
    std::fs::create_dir_all(&blobs)?;
    let refs_path = cache_dir.join("refs.json");

    // manifest.json: the config blob and the ordered layer blobs, as tar members
    // (`blobs/sha256/<hex>`). The config digest (its hex) is the content id.
    let (config_member, layer_members) = read_save_manifest(tar_path)?;
    let id = config_member.rsplit('/').next().unwrap_or(&config_member).to_string();

    if let Some(built) = load_built(&bases, &id) {
        ref_store(&refs_path, tar_path, &id);
        log::info!("local image cache hit {}", short(&id));
        return Ok(built);
    }

    log::info!("building local image {tar_path}");
    let mut want: std::collections::HashSet<&str> =
        layer_members.iter().map(String::as_str).collect();
    want.insert(&config_member);
    let extracted = extract_members(tar_path, &want, &blobs)?;
    let member = |m: &str| -> Result<PathBuf> {
        extracted
            .get(m)
            .cloned()
            .ok_or_else(|| Error::Registry(format!("image tar missing blob {m}")))
    };

    let config = registry::parse_config(&std::fs::read(member(&config_member)?)?)?;
    let layers: Vec<PathBuf> = layer_members.iter().map(|m| member(m)).collect::<Result<_>>()?;

    let stamp = std::process::id();
    let rootfs = bases.join(format!("rootfs-{id}.{stamp}"));
    let base = bases.join(format!("{id}.sqfs"));
    let base_tmp = bases.join(format!("{id}.{stamp}.tmp"));
    flatten::flatten(&layers, &rootfs)?;
    pack::pack_squashfs(&rootfs, &base_tmp)?;
    std::fs::rename(&base_tmp, &base)?;
    let _ = std::fs::remove_dir_all(&rootfs);

    let json = serde_json::to_string(&config).map_err(|e| Error::Json(e.to_string()))?;
    std::fs::write(bases.join(format!("{id}.json")), json)?;
    ref_store(&refs_path, tar_path, &id);
    Ok(Built { base, config })
}

/// Read `manifest.json` from a docker-save tar: `(config_member, layer_members)`.
fn read_save_manifest(tar_path: &str) -> Result<(String, Vec<String>)> {
    let mut ar = tar::Archive::new(std::fs::File::open(tar_path)?);
    for entry in ar.entries()? {
        let mut e = entry?;
        if e.path()?.to_string_lossy() == "manifest.json" {
            let mut buf = Vec::new();
            io::copy(&mut e, &mut buf)?;
            return parse_save_manifest(&buf);
        }
    }
    Err(Error::Registry("no manifest.json in image tar".into()))
}

/// The pure JSON half of `read_save_manifest`: the first entry's `Config` blob
/// and its ordered `Layers`.
fn parse_save_manifest(json: &[u8]) -> Result<(String, Vec<String>)> {
    let v: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| Error::Json(e.to_string()))?;
    let m = v.get(0).ok_or_else(|| Error::Registry("empty manifest.json".into()))?;
    let config = m["Config"]
        .as_str()
        .ok_or_else(|| Error::Registry("manifest.json: no Config".into()))?
        .to_string();
    let layers = registry::str_list(&m["Layers"]);
    if layers.is_empty() {
        return Err(Error::Registry("manifest.json: no Layers".into()));
    }
    Ok((config, layers))
}

/// Extract the named members of a tar into `dest`, keyed by member path. Each
/// lands at `dest/<basename>` (the blob's sha256), so repeats are deduped.
fn extract_members(
    tar_path: &str,
    want: &std::collections::HashSet<&str>,
    dest: &Path,
) -> Result<std::collections::HashMap<String, PathBuf>> {
    let mut ar = tar::Archive::new(std::fs::File::open(tar_path)?);
    let mut out = std::collections::HashMap::new();
    for entry in ar.entries()? {
        let mut e = entry?;
        let member = e.path()?.to_string_lossy().into_owned();
        if want.contains(member.as_str()) {
            let name = member.rsplit('/').next().unwrap_or(&member);
            let path = dest.join(name);
            let mut w = std::fs::File::create(&path)?;
            io::copy(&mut e, &mut w)?;
            out.insert(member, path);
        }
    }
    Ok(out)
}

fn short(id: &str) -> &str {
    // `get` returns None if 12 lands mid-UTF-8-char (a crafted local-tar id can be
    // multibyte), so fall back to the whole id rather than panicking on a slice.
    id.get(..12).unwrap_or(id)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_does_not_panic_on_a_multibyte_id() {
        let id = "aaaaaaaaaaa\u{1F980}"; // byte 12 falls inside the multibyte char
        let _ = short(id); // would panic slicing &id[..12] before the fix
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
    }

    #[test]
    fn parses_docker_save_manifest() {
        let json = br#"[{"Config":"blobs/sha256/abc","RepoTags":["x:latest"],
            "Layers":["blobs/sha256/l1","blobs/sha256/l2"]}]"#;
        let (config, layers) = parse_save_manifest(json).unwrap();
        assert_eq!(config, "blobs/sha256/abc");
        assert_eq!(layers, ["blobs/sha256/l1", "blobs/sha256/l2"]);
    }

    #[test]
    fn rejects_manifest_with_no_layers() {
        let json = br#"[{"Config":"blobs/sha256/abc","Layers":[]}]"#;
        assert!(parse_save_manifest(json).is_err());
    }

    #[test]
    fn rejects_empty_manifest() {
        assert!(parse_save_manifest(b"[]").is_err());
    }
}
