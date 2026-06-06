//! Guest RAM: one anonymous mmap region the backend maps into guest-physical
//! space at `RAM_BASE`. The host pointer is how we write the kernel, the DTB,
//! and (later) restore a snapshot. The backend reads `host_ptr`/`len` to hand
//! the region to hv_vm_map or KVM_SET_USER_MEMORY_REGION.

use crate::{Error, Result};
use std::ptr::NonNull;

pub struct GuestMemory {
    host: NonNull<u8>,
    len: usize,
    base: u64,
}

// The region is owned exclusively by the Vm; sharing across threads is mediated
// above this type. Marking Send lets the Vm move to a vcpu thread.
unsafe impl Send for GuestMemory {}

impl GuestMemory {
    /// Allocate `len` bytes of guest RAM mapped at guest-physical `base`.
    pub fn new(base: u64, len: usize) -> Result<Self> {
        // MAP_ANON | MAP_PRIVATE, readable and writable. The guest's executable
        // mapping is granted separately by the backend's memory flags.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(Error::Mmap(std::io::Error::last_os_error()));
        }
        Ok(Self {
            host: NonNull::new(ptr as *mut u8).unwrap(),
            len,
            base,
        })
    }

    pub fn base(&self) -> u64 {
        self.base
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn host_ptr(&self) -> *mut u8 {
        self.host.as_ptr()
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Copy `bytes` into the guest at guest-physical `gpa`. Used to place the
    /// kernel image and the DTB before boot.
    pub fn write(&self, gpa: u64, bytes: &[u8]) -> Result<()> {
        let off = gpa
            .checked_sub(self.base)
            .filter(|o| (*o as usize).checked_add(bytes.len()).map_or(false, |e| e <= self.len))
            .ok_or_else(|| Error::Loader(format!("write {:#x}+{} out of range", gpa, bytes.len())))?
            as usize;
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.host.as_ptr().add(off), bytes.len());
        }
        Ok(())
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.host.as_ptr() as *mut libc::c_void, self.len);
        }
    }
}
