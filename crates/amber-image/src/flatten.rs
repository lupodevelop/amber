//! Flatten OCI layers into a single rootfs tree.
//!
//! Layers are gzipped tarballs applied in order. Most entries are extracted as
//! is; the special cases are OverlayFS-style whiteouts: a file named `.wh.<name>`
//! deletes `<name>` from the accumulated tree, and `.wh..wh..opq` marks its
//! directory opaque (drop everything inherited from lower layers). Entry types
//! that need root (device/fifo nodes) are skipped — a guest gets its `/dev` from
//! devtmpfs, not the image.

use crate::Result;
use flate2::read::GzDecoder;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use tar::EntryType;

const WH_PREFIX: &str = ".wh.";
const WH_OPAQUE: &str = ".wh..wh..opq";

pub fn flatten(layers: &[PathBuf], dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::create_dir_all(dest)?;

    for layer in layers {
        let file = fs::File::open(layer)?;
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let Some(rel) = sanitize(&path) else {
                log::debug!("skip unsafe path {}", path.display());
                continue;
            };

            // Whiteouts act on the accumulated tree rather than extracting.
            if let Some(name) = rel.file_name().and_then(|n| n.to_str()) {
                if name == WH_OPAQUE {
                    let dir = dest.join(rel.parent().unwrap_or(Path::new("")));
                    clear_dir(&dir)?;
                    continue;
                }
                if let Some(target) = name.strip_prefix(WH_PREFIX) {
                    let victim = dest
                        .join(rel.parent().unwrap_or(Path::new("")))
                        .join(target);
                    remove_any(&victim)?;
                    continue;
                }
            }

            extract(&mut entry, &dest.join(&rel))?;
        }
    }
    Ok(())
}

fn extract<R: io::Read>(entry: &mut tar::Entry<R>, out: &Path) -> Result<()> {
    let kind = entry.header().entry_type();
    let mode = entry.header().mode().unwrap_or(0o644);

    match kind {
        EntryType::Directory => {
            fs::create_dir_all(out)?;
            let _ = fs::set_permissions(out, fs::Permissions::from_mode(mode));
        }
        EntryType::Regular | EntryType::Continuous => {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            remove_any(out)?;
            let mut f = fs::File::create(out)?;
            io::copy(entry, &mut f)?;
            let _ = fs::set_permissions(out, fs::Permissions::from_mode(mode));
        }
        EntryType::Symlink => {
            if let Some(target) = entry.link_name()? {
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                remove_any(out)?;
                std::os::unix::fs::symlink(target, out)?;
            }
        }
        EntryType::Link => {
            // Hard link: target is relative to the rootfs root (out's ancestor).
            if let Some(target) = entry.link_name()? {
                if let Some(root) = rootfs_root(out, entry.path()?.as_ref()) {
                    let src = root.join(sanitize(&target).unwrap_or_default());
                    if let Some(parent) = out.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    remove_any(out)?;
                    // Fall back to a copy if the link cannot be made.
                    if fs::hard_link(&src, out).is_err() {
                        let _ = fs::copy(&src, out);
                    }
                }
            }
        }
        // Device/fifo/other: not needed to run an image; the guest's devtmpfs
        // provides /dev. Skip rather than require root.
        _ => log::debug!("skip {:?} entry {}", kind, out.display()),
    }
    Ok(())
}

/// Reject absolute paths and any `..` escape; strip leading `./`. Returns a path
/// guaranteed to stay within the rootfs when joined to it.
fn sanitize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The rootfs root for a given output path, by stripping the entry's relative
/// path back off the output path.
fn rootfs_root(out: &Path, entry_rel: &Path) -> Option<PathBuf> {
    let rel = sanitize(entry_rel)?;
    let mut root = out.to_path_buf();
    for _ in rel.components() {
        root = root.parent()?.to_path_buf();
    }
    Some(root)
}

fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(_) => {}
    }
    Ok(())
}

fn clear_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        remove_any(&entry?.path())?;
    }
    Ok(())
}
