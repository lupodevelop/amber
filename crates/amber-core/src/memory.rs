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

    /// Map a snapshot's `mem.bin` as guest RAM **copy-on-write**: reads come from
    /// the file's page cache (shared across every fork of the same template),
    /// writes fault a private anonymous copy. This is what makes a fork cheap — no
    /// up-front copy of the whole image, and forks share all the pages they never
    /// touch. `len` is the guest RAM size (the file is exactly that big).
    pub fn from_snapshot_cow(base: u64, path: &std::path::Path) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path).map_err(Error::Mmap)?;
        let len = file.metadata().map_err(Error::Mmap)?.len() as usize;
        // MAP_PRIVATE on a file fd is copy-on-write; the mapping survives closing
        // the fd (the kernel keeps the reference), so `file` may drop right after.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
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
            .filter(|o| (*o as usize).checked_add(bytes.len()).is_some_and(|e| e <= self.len))
            .ok_or_else(|| Error::Loader(format!("write {:#x}+{} out of range", gpa, bytes.len())))?
            as usize;
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.host.as_ptr().add(off), bytes.len());
        }
        Ok(())
    }

    /// A cheap copyable view of guest RAM for device emulation. Devices read
    /// descriptor rings and data buffers through it. Single-threaded use only
    /// (the vcpu thread), and it must not outlive the `GuestMemory` it views.
    pub fn ram(&self) -> GuestRam {
        GuestRam {
            host: self.host.as_ptr(),
            base: self.base,
            len: self.len,
        }
    }
}

/// A raw, copyable window into guest RAM. Holds no lifetime, so the caller is
/// responsible for keeping the owning `GuestMemory` alive (it does: both live on
/// the vcpu thread for the VM's duration). Out-of-range accesses are rejected.
#[derive(Clone, Copy)]
pub struct GuestRam {
    host: *mut u8,
    base: u64,
    len: usize,
}

impl GuestRam {
    fn offset(&self, gpa: u64, n: usize) -> Option<usize> {
        let off = gpa.checked_sub(self.base)? as usize;
        (off.checked_add(n)? <= self.len).then_some(off)
    }

    /// Host pointer for a guest-physical range, if it lies within RAM. Used by
    /// the balloon to `madvise` guest-reported free pages.
    pub fn host_ptr_at(&self, gpa: u64, len: usize) -> Option<*mut u8> {
        let off = self.offset(gpa, len)?;
        Some(unsafe { self.host.add(off) })
    }

    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        match self.offset(gpa, buf.len()) {
            Some(off) => unsafe {
                std::ptr::copy_nonoverlapping(self.host.add(off), buf.as_mut_ptr(), buf.len());
                true
            },
            None => false,
        }
    }

    pub fn write(&self, gpa: u64, buf: &[u8]) -> bool {
        match self.offset(gpa, buf.len()) {
            Some(off) => unsafe {
                std::ptr::copy_nonoverlapping(buf.as_ptr(), self.host.add(off), buf.len());
                true
            },
            None => false,
        }
    }

    pub fn read_u16(&self, gpa: u64) -> u16 {
        let mut b = [0u8; 2];
        self.read(gpa, &mut b);
        u16::from_le_bytes(b)
    }

    pub fn read_u32(&self, gpa: u64) -> u32 {
        let mut b = [0u8; 4];
        self.read(gpa, &mut b);
        u32::from_le_bytes(b)
    }

    pub fn read_u64(&self, gpa: u64) -> u64 {
        let mut b = [0u8; 8];
        self.read(gpa, &mut b);
        u64::from_le_bytes(b)
    }

    pub fn write_u16(&self, gpa: u64, v: u16) {
        self.write(gpa, &v.to_le_bytes());
    }

    pub fn write_u32(&self, gpa: u64, v: u32) {
        self.write(gpa, &v.to_le_bytes());
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.host.as_ptr() as *mut libc::c_void, self.len);
        }
    }
}
