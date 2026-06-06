//! Pack a flattened rootfs into a read-only base image the guest mounts under an
//! overlay. squashfs for now (its module ships with the kernel we use); erofs is
//! a drop-in swap — a different mkfs and guest module — once the kernel bundles
//! erofs built in. `-all-root` because host-side flattening cannot preserve
//! ownership anyway, and the guest runs as root.

use crate::{Error, Result};
use std::path::Path;
use std::process::Command;

pub fn pack_squashfs(rootfs: &Path, out: &Path) -> Result<()> {
    if out.exists() {
        std::fs::remove_file(out)?;
    }
    let status = Command::new("mksquashfs")
        .arg(rootfs)
        .arg(out)
        .args(["-all-root", "-noappend", "-quiet", "-no-progress", "-no-xattrs"])
        .status()
        .map_err(|e| Error::Pack(format!("running mksquashfs: {e}")))?;
    if !status.success() {
        return Err(Error::Pack(format!("mksquashfs exited with {status}")));
    }
    Ok(())
}
