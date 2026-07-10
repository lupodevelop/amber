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
use std::io::{Read, Seek, SeekFrom};
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
    // Canonical root for the symlink-containment check below.
    let root = dest.canonicalize()?;

    for layer in layers {
        // Registry layers are gzipped; a local `docker save` tar is plain tar.
        // Sniff the gzip magic and pick the reader accordingly.
        let mut file = fs::File::open(layer)?;
        let mut magic = [0u8; 2];
        let gzipped = file.read(&mut magic)? == 2 && magic == [0x1f, 0x8b];
        file.seek(SeekFrom::Start(0))?;
        let reader: Box<dyn Read> = if gzipped {
            Box::new(GzDecoder::new(file))
        } else {
            Box::new(file)
        };
        let mut archive = tar::Archive::new(reader);
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
                    if within_root(&root, &dir.join("_")) {
                        clear_dir(&dir)?;
                    }
                    continue;
                }
                if let Some(target) = name.strip_prefix(WH_PREFIX) {
                    let victim = dest
                        .join(rel.parent().unwrap_or(Path::new("")))
                        .join(target);
                    if within_root(&root, &victim) {
                        remove_any(&victim)?;
                    }
                    continue;
                }
            }

            let out = dest.join(&rel);
            // A previous layer may have made a parent component a symlink that
            // escapes the tree; refuse to write through it (tar-symlink traversal).
            if !within_root(&root, &out) {
                log::debug!("skip {} (parent escapes rootfs)", out.display());
                continue;
            }
            extract(&mut entry, &out)?;
        }
    }
    Ok(())
}

/// True if writing `out` stays inside the rootfs: the deepest existing ancestor of
/// `out`'s parent, with symlinks resolved, must lie under `root` (already canonical).
/// Blocks the tar-symlink traversal where a layer makes a parent a symlink escaping
/// the tree and a later entry writes through it onto the host.
fn within_root(root: &Path, out: &Path) -> bool {
    let mut p = out.parent();
    while let Some(dir) = p {
        match dir.canonicalize() {
            Ok(c) => return c.starts_with(root),
            Err(_) => p = dir.parent(),
        }
    }
    false
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

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use tar::{Builder, EntryType, Header};

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("amber-flatten-{}-{}", std::process::id(), name))
    }

    fn write_layer(path: &Path, build: impl FnOnce(&mut Builder<GzEncoder<Vec<u8>>>)) {
        let mut b = Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        build(&mut b);
        let gz = b.into_inner().unwrap().finish().unwrap();
        fs::write(path, gz).unwrap();
    }

    fn reg(b: &mut Builder<GzEncoder<Vec<u8>>>, path: &str, data: &[u8]) {
        let mut h = Header::new_gnu();
        h.set_entry_type(EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(data.len() as u64);
        b.append_data(&mut h, path, data).unwrap();
    }

    #[test]
    fn symlink_parent_traversal_is_blocked() {
        let outside = tmp("outside-1");
        let dest = tmp("dest-1");
        let layer = tmp("layer-1.tar.gz");
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir_all(&outside).unwrap();

        // Layer: make `esc` a symlink to a dir OUTSIDE the rootfs, then write
        // `esc/pwned` through it. The write must NOT land in `outside`.
        write_layer(&layer, |b| {
            let mut h = Header::new_gnu();
            h.set_entry_type(EntryType::Symlink);
            h.set_mode(0o777);
            h.set_size(0);
            b.append_link(&mut h, "esc", &outside).unwrap();
            reg(b, "esc/pwned", b"PWNED");
        });

        flatten(&[layer], &dest).unwrap();

        assert!(!outside.join("pwned").exists(), "traversal wrote outside the rootfs");
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn normal_files_still_extract() {
        let dest = tmp("dest-2");
        let layer = tmp("layer-2.tar.gz");
        let _ = fs::remove_dir_all(&dest);
        write_layer(&layer, |b| reg(b, "etc/hello", b"hi"));

        flatten(&[layer], &dest).unwrap();

        assert_eq!(fs::read(dest.join("etc/hello")).unwrap(), b"hi");
        let _ = fs::remove_dir_all(&dest);
    }
}
