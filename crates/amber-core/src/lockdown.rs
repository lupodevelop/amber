//! VMM lockdown: drop the VM process's privileges before the guest runs.
//!
//! The policy is platform-agnostic and lives here; the mechanism is per-OS behind
//! `cfg` (macOS: a seatbelt profile via `sandbox_init`; Linux: a seccomp-bpf
//! filter when the KVM backend lands). The contract either way: after `apply`,
//! the process can no longer exec, fork, or write the filesystem outside the
//! allowed paths — so a guest escape into the VMM is contained. Everything the
//! VM needs (guest RAM, disk fd, control fd, console pipes, bound listeners) is
//! already open by the time this runs; lockdown only has to govern *new* acquisitions.

/// What the VM process may still do after lockdown.
#[derive(Debug, Default)]
pub struct Policy {
    /// Directories the process may still create/write (the snapshot/template
    /// destination). Everything else becomes read-only.
    pub write_paths: Vec<std::path::PathBuf>,
    /// Whether the process may open new sockets: the userspace netstack dials
    /// host TCP/UDP at runtime (guest flows, DNS) and accepts inbound forwards.
    /// Without a network device nothing new is needed and the network is denied.
    pub net: bool,
}

impl Policy {
    /// Enforce the policy on the current process. Irreversible.
    /// `AMBER_NO_LOCKDOWN=1` skips it (debugging escape hatch).
    pub fn apply(&self) -> std::io::Result<()> {
        if std::env::var_os("AMBER_NO_LOCKDOWN").is_some() {
            log::warn!("lockdown skipped (AMBER_NO_LOCKDOWN)");
            return Ok(());
        }
        imp::apply(self)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Policy;

    extern "C" {
        fn sandbox_init(
            profile: *const libc::c_char,
            flags: u64,
            errorbuf: *mut *mut libc::c_char,
        ) -> libc::c_int;
        fn sandbox_free_error(errorbuf: *mut libc::c_char);
    }

    /// Build the seatbelt (SBPL) profile. Allow-default with targeted denies:
    /// versus deny-default this cannot enumerate-and-miss a syscall HVF needs,
    /// while still removing the abilities that turn a VMM compromise into a
    /// foothold (spawning processes, dropping files, opening sockets).
    fn profile(p: &Policy) -> String {
        let mut s = String::from(
            "(version 1)\n\
             (allow default)\n\
             (deny process-exec*)\n\
             (deny process-fork)\n\
             (deny file-write*)\n",
        );
        for path in &p.write_paths {
            // Canonicalize so the subpath rule survives relative paths; the dir
            // may not exist yet (snapshot dirs are created at capture), so fall
            // back to the parent's canonical form + file name.
            let canon = path.canonicalize().or_else(|_| {
                let parent = path.parent().unwrap_or(std::path::Path::new("."));
                let name = path.file_name().unwrap_or_default();
                parent.canonicalize().map(|c| c.join(name))
            });
            if let Ok(c) = canon {
                s.push_str(&format!("(allow file-write* (subpath \"{}\"))\n", c.display()));
            }
        }
        if !p.net {
            s.push_str("(deny network*)\n");
        }
        s
    }

    pub fn apply(p: &Policy) -> std::io::Result<()> {
        let profile = profile(p);
        let cstr = std::ffi::CString::new(profile)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut err: *mut libc::c_char = std::ptr::null_mut();
        let rc = unsafe { sandbox_init(cstr.as_ptr(), 0, &mut err) };
        if rc != 0 {
            let msg = if err.is_null() {
                "sandbox_init failed".to_string()
            } else {
                let m = unsafe { std::ffi::CStr::from_ptr(err) }.to_string_lossy().into_owned();
                unsafe { sandbox_free_error(err) };
                m
            };
            return Err(std::io::Error::other(msg));
        }
        log::info!(
            "vmm locked down (seatbelt): no exec/fork, fs read-only{}{}",
            if p.write_paths.is_empty() { "".to_string() } else { format!(" except {:?}", p.write_paths) },
            if p.net { ", network allowed" } else { ", network denied" },
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn profile_denies_exec_fork_writes() {
            let s = profile(&Policy::default());
            assert!(s.contains("(deny process-exec*)"));
            assert!(s.contains("(deny process-fork)"));
            assert!(s.contains("(deny file-write*)"));
            assert!(s.contains("(deny network*)")); // no net in the default policy
        }

        #[test]
        fn profile_allows_listed_write_paths_and_net() {
            let p = Policy { write_paths: vec![std::env::temp_dir()], net: true };
            let s = profile(&p);
            assert!(s.contains("(allow file-write* (subpath"));
            assert!(!s.contains("(deny network*)"));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Policy;

    /// Linux mechanism (seccomp-bpf + no_new_privs) lands with the KVM backend;
    /// the policy and call sites are already in place. Warn so a Linux run is
    /// visibly unconfined rather than silently so.
    pub fn apply(_p: &Policy) -> std::io::Result<()> {
        log::warn!("lockdown: no mechanism on this platform yet (seccomp arrives with the KVM backend)");
        Ok(())
    }
}
