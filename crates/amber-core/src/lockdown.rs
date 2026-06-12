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

#[cfg(target_os = "linux")]
mod imp {
    use super::Policy;
    use std::io::{Error, Result};

    // --- seccomp-bpf -------------------------------------------------------
    // Same philosophy as the macOS seatbelt profile: allow-default with targeted
    // denies, so the filter cannot enumerate-and-miss a syscall the KVM run loop
    // needs. We deny the escape primitives — exec (no arbitrary code), and new
    // sockets when the policy has no network. Fork is left to Landlock + the exec
    // deny: a fork that cannot exec, write, or dial is inert. The filter is
    // installed on the main thread before any other thread spawns, and seccomp
    // filters are inherited across clone(), so every vcpu/reader thread gets it.

    // BPF classes/ops (linux/bpf_common.h).
    const LD: u16 = 0x00;
    const W: u16 = 0x00;
    const ABS: u16 = 0x20;
    const JMP: u16 = 0x05;
    const JEQ: u16 = 0x10;
    const RET: u16 = 0x06;
    const K: u16 = 0x00;

    // seccomp return actions + data offsets (linux/seccomp.h, seccomp_data).
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const ARCH_AARCH64: u32 = 0xc000_00b7; // EM_AARCH64 | 64BIT | LE
    const OFF_NR: u32 = 0;
    const OFF_ARCH: u32 = 4;

    // aarch64 syscall numbers we deny.
    const NR_EXECVE: u32 = 221;
    const NR_EXECVEAT: u32 = 281;
    const NR_SOCKET: u32 = 198;

    fn stmt(code: u16, k: u32) -> libc::sock_filter {
        libc::sock_filter { code, jt: 0, jf: 0, k }
    }
    fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
        libc::sock_filter { code, jt, jf, k }
    }
    /// `if nr == sysno { return ERRNO(EPERM) }` — two instructions.
    fn deny(sysno: u32) -> [libc::sock_filter; 2] {
        [
            jump(JMP | JEQ | K, sysno, 0, 1),
            stmt(RET | K, RET_ERRNO | libc::EPERM as u32),
        ]
    }

    fn install_seccomp(net: bool) -> Result<()> {
        let mut prog = vec![
            // Reject any non-aarch64 personality outright.
            stmt(LD | W | ABS, OFF_ARCH),
            jump(JMP | JEQ | K, ARCH_AARCH64, 1, 0),
            stmt(RET | K, RET_KILL_PROCESS),
            // Load the syscall number for the comparisons below.
            stmt(LD | W | ABS, OFF_NR),
        ];
        prog.extend(deny(NR_EXECVE));
        prog.extend(deny(NR_EXECVEAT));
        if !net {
            prog.extend(deny(NR_SOCKET));
        }
        prog.push(stmt(RET | K, RET_ALLOW));

        let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_mut_ptr() };
        // PR_SET_NO_NEW_PRIVS is required to load a filter unprivileged, and also
        // blocks setuid/fscaps from re-granting privilege on a later exec.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(Error::last_os_error());
        }
        if unsafe {
            libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog as *const _, 0, 0)
        } != 0
        {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    // --- Landlock ----------------------------------------------------------
    // Filesystem confinement (kernel ≥ 5.13). We handle only the write/create
    // access rights, granting them on the policy's write_paths; reads and exec
    // are left unhandled (so unrestricted by Landlock — exec is the seccomp
    // filter's job). With no write_paths, every write is denied.

    const SYS_CREATE_RULESET: libc::c_long = 444;
    const SYS_ADD_RULE: libc::c_long = 445;
    const SYS_RESTRICT_SELF: libc::c_long = 446;
    const RULE_PATH_BENEATH: libc::c_uint = 1;
    const CREATE_RULESET_VERSION: libc::c_uint = 1 << 0;

    // LANDLOCK_ACCESS_FS_* write/create bits (ABI 1 — present on every Landlock
    // kernel). Read/exec bits are deliberately excluded.
    const FS_WRITE: u64 = (1 << 1) // WRITE_FILE
        | (1 << 4)  // REMOVE_DIR
        | (1 << 5)  // REMOVE_FILE
        | (1 << 6)  // MAKE_CHAR
        | (1 << 7)  // MAKE_DIR
        | (1 << 8)  // MAKE_REG
        | (1 << 9)  // MAKE_SOCK
        | (1 << 10) // MAKE_FIFO
        | (1 << 11) // MAKE_BLOCK
        | (1 << 12); // MAKE_SYM

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }
    #[repr(C)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: libc::c_int,
    }

    fn install_landlock(paths: &[std::path::PathBuf]) -> Result<()> {
        // Probe the ABI; a negative result (ENOSYS / disabled) means no Landlock.
        let abi = unsafe {
            libc::syscall(SYS_CREATE_RULESET, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION)
        };
        if abi < 1 {
            log::warn!("lockdown: Landlock unavailable (abi={abi}); filesystem writes not confined");
            return Ok(());
        }
        let attr = RulesetAttr { handled_access_fs: FS_WRITE };
        let rs = unsafe {
            libc::syscall(SYS_CREATE_RULESET, &attr as *const _, std::mem::size_of::<RulesetAttr>(), 0u32)
        };
        if rs < 0 {
            return Err(Error::last_os_error());
        }
        let rs = rs as libc::c_int;

        for p in paths {
            let cpath = std::ffi::CString::new(p.as_os_str().as_encoded_bytes())
                .map_err(|e| Error::new(std::io::ErrorKind::InvalidInput, e))?;
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd < 0 {
                // The dir may not exist yet (snapshot dirs created at capture);
                // grant on its parent so the eventual write still lands.
                let parent = p.parent().unwrap_or(std::path::Path::new("."));
                let cpar = std::ffi::CString::new(parent.as_os_str().as_encoded_bytes())
                    .map_err(|e| Error::new(std::io::ErrorKind::InvalidInput, e))?;
                let pfd = unsafe { libc::open(cpar.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
                if pfd < 0 {
                    log::warn!("lockdown: cannot open {} for Landlock rule", p.display());
                    continue;
                }
                add_path_rule(rs, pfd)?;
            } else {
                add_path_rule(rs, fd)?;
            }
        }

        if unsafe { libc::syscall(SYS_RESTRICT_SELF, rs, 0u32) } != 0 {
            let e = Error::last_os_error();
            unsafe { libc::close(rs) };
            return Err(e);
        }
        unsafe { libc::close(rs) };
        Ok(())
    }

    fn add_path_rule(ruleset: libc::c_int, parent_fd: libc::c_int) -> Result<()> {
        let rule = PathBeneathAttr { allowed_access: FS_WRITE, parent_fd };
        let rc = unsafe { libc::syscall(SYS_ADD_RULE, ruleset, RULE_PATH_BENEATH, &rule as *const _, 0u32) };
        unsafe { libc::close(parent_fd) };
        if rc != 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    pub fn apply(p: &Policy) -> Result<()> {
        // Landlock first (it also needs no_new_privs, which seccomp sets), then
        // seccomp. Both inherit across the threads the run loop spawns afterward.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(Error::last_os_error());
        }
        install_landlock(&p.write_paths)?;
        install_seccomp(p.net)?;
        log::info!(
            "vmm locked down (seccomp+landlock): no exec, fs read-only{}{}",
            if p.write_paths.is_empty() { "".to_string() } else { format!(" except {:?}", p.write_paths) },
            if p.net { ", network allowed" } else { ", new sockets denied" },
        );
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::Policy;

    pub fn apply(_p: &Policy) -> std::io::Result<()> {
        log::warn!("lockdown: no mechanism on this platform");
        Ok(())
    }
}
