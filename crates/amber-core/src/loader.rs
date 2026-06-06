//! arm64 Linux `Image` loader. Parses the 64-byte header (booting.rst) to find
//! where the kernel wants to live, then copies it into guest RAM. No bzImage,
//! no PVH: arm64 has exactly one boot protocol and this is it.

use crate::memory::GuestMemory;
use crate::{layout, Error, Result};

/// "ARM\x64" little-endian, at byte offset 0x38 of the Image header.
const ARM64_MAGIC: u32 = 0x644d_5241;

pub struct LoadedKernel {
    /// Guest-physical entry point: where the vcpu PC must be set.
    pub entry: u64,
}

/// Copy an arm64 `Image` into guest memory and return its entry point.
///
/// Modern kernels set `text_offset` to 0 and require the loader to place the
/// image at a 2 MiB-aligned base. We place it at `RAM_BASE + KERNEL_OFFSET`,
/// which is 2 MiB-aligned, and honor a nonzero `text_offset` if an older kernel
/// sets one.
pub fn load_kernel(mem: &GuestMemory, image: &[u8]) -> Result<LoadedKernel> {
    if image.len() < 64 {
        return Err(Error::Loader("image shorter than header".into()));
    }
    let magic = u32::from_le_bytes(image[0x38..0x3c].try_into().unwrap());
    if magic != ARM64_MAGIC {
        return Err(Error::Loader(format!("bad arm64 magic {magic:#x}")));
    }
    let text_offset = u64::from_le_bytes(image[0x08..0x10].try_into().unwrap());

    let load_off = if text_offset == 0 {
        layout::KERNEL_OFFSET
    } else {
        text_offset
    };
    let entry = mem.base() + load_off;
    mem.write(entry, image)?;
    Ok(LoadedKernel { entry })
}

/// Place an optional initramfs high in RAM and return (start, end) guest-physical
/// addresses for the DTB to advertise via /chosen. Returns None if there is none.
pub fn load_initramfs(mem: &GuestMemory, initrd: Option<&[u8]>) -> Result<Option<(u64, u64)>> {
    let Some(data) = initrd else { return Ok(None) };
    // Put it in the top quarter of RAM, page aligned, clear of the kernel.
    let start = mem.base() + (mem.len() as u64 / 4) * 3;
    let start = (start + 0xfff) & !0xfff;
    mem.write(start, data)?;
    Ok(Some((start, start + data.len() as u64)))
}
