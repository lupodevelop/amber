//! `amberd`: the control-plane supervisor and its clients.
//!
//! HVF allows one VM per process, so amberd does not host VMs itself — it owns
//! the control socket and a registry, and spawns one child `amber __vm` process
//! per VM (which also gives each VM its own restricted host process, the security
//! model amber wants). `RunOneShot` spawns a child and relays its stdin/stdout to
//! the client; `List`/`Kill`/`Shutdown` manage the fleet.

use crate::proto::{
    self, read_frame, write_frame, write_reply, Reply, Request, VmInfo, TAG_REPLY, TAG_REQUEST,
    TAG_STDERR, TAG_STDIN, TAG_STDOUT,
};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

/// A live VM's registry entry: its public info plus a flag the owning supervisor
/// thread watches so `Kill` never has to touch a raw pid (no PID-reuse race).
struct VmEntry {
    info: VmInfo,
    kill: Arc<AtomicBool>,
    /// amberd's end of the control channel to the VM (balloon targets, etc.).
    control: Option<UnixStream>,
    /// For a pooled (paused) worker: its guest-console pipes, held until the fork
    /// that releases it either relays them to a client (interactive) or drains
    /// stdout to the log (detached). Paused workers emit nothing, so the pipe is
    /// idle until the go byte.
    stdout: Option<std::process::ChildStdout>,
    stdin: Option<std::process::ChildStdin>,
    /// Host-side vsock UDS base for this worker, if it has one (all restored
    /// forks do). A host peer connects it and sends `CONNECT <port>\n`.
    vsock: Option<String>,
}

/// Lock a Mutex, recovering from poison instead of propagating the panic. A
/// connection thread panicking under the registry lock must not brick the whole
/// daemon (every later List/Kill/Fork would `unwrap()`-panic on the poison); the
/// map's own operations are self-contained, so the recovered state is usable.
trait LockRecover<T> {
    fn locked(&self) -> MutexGuard<'_, T>;
}
impl<T> LockRecover<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

type Registry = Arc<Mutex<HashMap<String, VmEntry>>>;

/// Warm pool: template directory -> ids of pre-staged paused workers ready to be
/// released. A fork pops one (a ~ms handoff) instead of spawning (~tens of ms).
type Pool = Arc<Mutex<HashMap<String, Vec<String>>>>;

/// Default per-VM RAM when no template `ram_cap` applies (matches `VmConfig`).
const DEFAULT_MEM_BYTES: u64 = 512 * 1024 * 1024;

// ---- budget (M5 admission control) ----

/// RAM a VM is accounted at: its template's `ram_cap`, else the default.
fn vm_ram_bytes(target: &str) -> u64 {
    crate::manifest::Manifest::load()
        .as_ref()
        .and_then(|m| m.template(target))
        .and_then(|t| t.ram_cap.as_deref())
        .and_then(crate::manifest::parse_size)
        .map(|b| b as u64)
        .unwrap_or(DEFAULT_MEM_BYTES)
}

/// The fleet RAM ceiling from `[fleet].ram_budget`, if set.
fn ram_budget() -> Option<u64> {
    crate::manifest::Manifest::load()
        .and_then(|m| m.fleet.ram_budget)
        .as_deref()
        .and_then(crate::manifest::parse_size)
        .map(|b| b as u64)
}

/// Total host RAM in bytes (0 if unavailable).
#[cfg(target_os = "macos")]
fn machine_ram() -> u64 {
    Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn machine_ram() -> u64 {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let psize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages > 0 && psize > 0 {
        pages as u64 * psize as u64
    } else {
        0
    }
}

/// Real resident memory of a process, in bytes (0 if unavailable).
#[cfg(target_os = "macos")]
fn rss_bytes(pid: u32) -> u64 {
    if pid == 0 {
        return 0;
    }
    Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

// On Linux, read `/proc/<pid>/statm` directly instead of forking `ps` per VM —
// listing a 1000-VM fleet otherwise spawns 1000 processes per `amber budget`/`ps`.
#[cfg(target_os = "linux")]
fn rss_bytes(pid: u32) -> u64 {
    if pid == 0 {
        return 0;
    }
    // statm fields are in pages; the second is the resident set size.
    let resident_pages: u64 = std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|f| f.parse().ok()))
        .unwrap_or(0);
    let psize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if psize > 0 {
        resident_pages * psize as u64
    } else {
        0
    }
}

/// Atomically admit and reserve a slot for a new VM, or refuse to protect the
/// budget. The check and the registry insert happen under one lock so concurrent
/// admissions can't both pass and overcommit. Returns the reserved `(id, kill)`
/// (the entry's pid is 0 until the child is spawned), or `(budget, used,
/// requested)` on refusal.
fn reserve(
    reg: &Registry,
    counter: &AtomicU64,
    reference: &str,
) -> std::result::Result<(String, Arc<AtomicBool>), (u64, u64, u64)> {
    // File I/O for the manifest stays outside the lock.
    let requested = vm_ram_bytes(reference);
    let budget = ram_budget();

    let mut g = reg.locked();
    if let Some(budget) = budget {
        let used: u64 = g.values().map(|e| e.info.ram_bytes).sum();
        if used + requested > budget {
            return Err((budget, used, requested));
        }
    }
    let id = format!("vm{}", counter.fetch_add(1, Ordering::Relaxed) + 1);
    let kill = Arc::new(AtomicBool::new(false));
    g.insert(
        id.clone(),
        VmEntry {
            info: VmInfo {
                id: id.clone(),
                reference: reference.to_string(),
                pid: 0,
                started: proto::now_secs(),
                ram_bytes: requested,
                rss_bytes: 0,
                paused: false,
            },
            kill: kill.clone(),
            control: None,
            stdout: None,
            stdin: None,
            vsock: None,
        },
    );
    Ok((id, kill))
}

/// Evict one warm-pool worker to free its budget reservation: pull an id from the
/// pool, kill its process, and drop it from the registry (freeing the RAM it was
/// accounted at). Returns true if one was evicted. Pooled VMs are idle and
/// reconstructible, so they yield to a real admission under budget pressure.
fn evict_one_pooled(reg: &Registry, pool: &Pool) -> bool {
    let id = {
        let mut p = pool.locked();
        p.values_mut().find_map(|v| v.pop())
    };
    let Some(id) = id else { return false };
    let mut g = reg.locked();
    if let Some(e) = g.remove(&id) {
        e.kill.store(true, Ordering::Relaxed); // the supervisor reaps the process
    }
    log::info!("evicted pooled VM {id} to free budget");
    true
}

/// Reserve admission for a real VM, evicting warm-pool workers as needed to fit
/// under the budget. Gives up (returning the budget error) only when nothing
/// poolable is left to reclaim.
fn reserve_with_evict(
    reg: &Registry,
    counter: &AtomicU64,
    pool: &Pool,
    reference: &str,
) -> std::result::Result<(String, Arc<AtomicBool>), (u64, u64, u64)> {
    loop {
        match reserve(reg, counter, reference) {
            Ok(ok) => return Ok(ok),
            Err(budget) => {
                if !evict_one_pooled(reg, pool) {
                    return Err(budget);
                }
            }
        }
    }
}

/// Record a reserved VM's pid and control channel once its worker has spawned.
fn set_runtime(reg: &Registry, id: &str, pid: u32, control: UnixStream) {
    if let Some(e) = reg.locked().get_mut(id) {
        e.info.pid = pid;
        e.control = Some(control);
    }
}

/// Wire a control channel into a child command: a socketpair whose child end is
/// dup'd to fd 3 (named via `AMBER_CONTROL_FD`). Returns amberd's end and the
/// child's end (drop the child's end after spawning).
fn attach_control(cmd: &mut Command) -> io::Result<(UnixStream, UnixStream)> {
    let (host, child) = UnixStream::pair()?;
    let child_fd = child.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(child_fd, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.env("AMBER_CONTROL_FD", "3");
    Ok((host, child))
}

// ---- warm pool (M4 fork) ----

/// Spawn a paused fork of `template`: a detached `restore` worker that does all
/// the costly work (CoW map + GIC + register restore), signals ready on the
/// control channel, and blocks for a go byte. Returns once it is warmed and
/// registered. The caller decides whether to pool the id or release it now.
/// Host-side vsock UDS base path for a minted fork. Unique per (daemon, vm id).
fn fork_vsock_base(pid: u32, id: &str) -> String {
    std::env::temp_dir()
        .join(format!("amber-fork-{pid}-{id}.sock"))
        .to_string_lossy()
        .into_owned()
}

fn spawn_paused(
    reg: &Registry,
    counter: &AtomicU64,
    pool: &Pool,
    template: &str,
    vsock: Option<&str>,
) -> io::Result<String> {
    let (id, kill) = reserve(reg, counter, template)
        .map_err(|_| io::Error::other("RAM budget exceeded"))?;

    let dir = logs_dir();
    std::fs::create_dir_all(&dir)?;
    // Worker logs (stderr) go to the file; the guest console (stdout) is a pipe we
    // hold, so the fork can relay it interactively or drain it to the log.
    let err = std::fs::File::create(dir.join(format!("{id}.log")))?;

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    // The template only restores correctly on the software GIC (the timer survives
    // there), so the worker runs in swgic mode regardless of the daemon's env.
    cmd.arg("restore").arg(template);
    cmd.env("AMBER_PAUSED", "1").env("AMBER_GIC", "sw");
    // Every restored fork gets a vsock base: exec passes its own (the in-guest
    // agent dials `<base>_1`); a plain fork gets a minted one so a host peer can
    // reach the guest with `CONNECT <port>`. Always set — templates carry a vsock
    // device, so a restore without a base would mismatch the device set.
    let base = match vsock {
        Some(v) => v.to_string(),
        None => fork_vsock_base(std::process::id(), &id),
    };
    cmd.env("AMBER_VSOCK", &base);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(err);
    let (mut ctl_host, ctl_child) = match attach_control(&mut cmd) {
        Ok(p) => p,
        Err(e) => {
            reg.locked().remove(&id);
            return Err(e);
        }
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            reg.locked().remove(&id);
            return Err(e);
        }
    };
    drop(ctl_child);
    let pid = child.id();
    let cout = child.stdout.take();
    let cin = child.stdin.take();

    // Block until the worker finishes the restore and signals ready (one byte).
    // This is the warming cost, paid ahead of any fork request.
    let mut ready = [0u8; 1];
    if io::Read::read_exact(&mut ctl_host, &mut ready).is_err() {
        reg.locked().remove(&id);
        let _ = child.kill();
        return Err(io::Error::other("worker failed to warm"));
    }
    if let Some(e) = reg.locked().get_mut(&id) {
        e.info.pid = pid;
        e.control = Some(ctl_host);
        e.stdout = cout;
        e.stdin = cin;
        e.vsock = Some(base.clone());
    }

    let reg2 = reg.clone();
    let pool2 = pool.clone();
    let id2 = id.clone();
    let tmpl2 = template.to_string();
    thread::spawn(move || {
        let never = AtomicBool::new(false);
        let _ = supervise(&mut child, &kill, &never);
        reg2.locked().remove(&id2);
        if let Some(v) = pool2.locked().get_mut(&tmpl2) {
            v.retain(|x| x != &id2);
        }
        // SIGKILL skips the guest's Drop, so drop the host-side socket ourselves.
        let _ = std::fs::remove_file(&base);
    });
    Ok(id)
}

/// Release a paused worker by writing the go byte to its control channel.
fn release(reg: &Registry, id: &str) -> bool {
    let g = reg.locked();
    match g.get(id).and_then(|e| e.control.as_ref()) {
        Some(ctl) => {
            let mut w: &UnixStream = ctl;
            io::Write::write_all(&mut w, &[1u8]).is_ok()
        }
        None => false,
    }
}

/// Send one control frame to a VM by id and optionally record its paused state.
/// The reply is an `Ok`, or an error if the VM is unknown / has no channel.
fn control_op(reg: &Registry, id: &str, frame: &[u8], set_paused: Option<bool>) -> Reply {
    let mut g = reg.locked();
    let Some(e) = g.get_mut(id) else {
        return Reply::Error { message: format!("no such vm: {id}") };
    };
    let wrote = match e.control.as_ref() {
        Some(ctl) => {
            let mut w: &UnixStream = ctl;
            w.write_all(frame).and_then(|_| w.flush())
        }
        None => return Reply::Error { message: format!("vm {id} has no control channel") },
    };
    match wrote {
        Ok(()) => {
            if let Some(p) = set_paused {
                e.info.paused = p;
            }
            Reply::Ok
        }
        Err(err) => Reply::Error { message: format!("control write: {err}") },
    }
}

fn handle_fork(
    mut stream: UnixStream,
    reg: Registry,
    counter: Arc<AtomicU64>,
    pool: Pool,
    template: String,
    interactive: bool,
) -> io::Result<()> {
    // Take a pre-warmed worker if the pool has one; otherwise stage one now.
    let pooled = pool.locked().get_mut(&template).and_then(|v| v.pop());
    let id = match pooled {
        Some(id) => id,
        None => match spawn_paused(&reg, &counter, &pool, &template, None) {
            Ok(id) => id,
            Err(e) => {
                write_reply(&mut stream, &Reply::Error { message: e.to_string() })?;
                return Ok(());
            }
        },
    };

    // Grab the worker's console pipes before releasing it (paused, so silent until
    // the go byte — nothing is lost) and start the refill so the next fork is warm.
    let (cout, cin, kill) = {
        let mut g = reg.locked();
        match g.get_mut(&id) {
            Some(e) => (e.stdout.take(), e.stdin.take(), Some(e.kill.clone())),
            None => (None, None, None),
        }
    };
    if !release(&reg, &id) {
        write_reply(&mut stream, &Reply::Error { message: format!("failed to release {id}") })?;
        return Ok(());
    }
    refill_pool(reg.clone(), counter, pool, template);

    if interactive {
        relay_interactive(stream, cout, cin, kill)
    } else {
        // Detached: drain the guest console to the log so `amber logs` shows it.
        if let Some(mut out) = cout {
            if let Ok(mut log) =
                std::fs::OpenOptions::new().append(true).open(logs_dir().join(format!("{id}.log")))
            {
                thread::spawn(move || {
                    let _ = io::copy(&mut out, &mut log);
                });
            }
        }
        let vsock = reg.locked().get(&id).and_then(|e| e.vsock.clone());
        write_reply(&mut stream, &Reply::Started { id, vsock })
    }
}

/// Run one command in a fork of `template` over the vsock exec channel. Stage a
/// dedicated worker with a private vsock base, listen for the in-guest agent's
/// dial, release the fork, then relay the agent's stdout/stderr/exit frames to the
/// client. One-shot and non-interactive; the worker is torn down at the end.
fn handle_exec(
    mut stream: UnixStream,
    reg: Registry,
    counter: Arc<AtomicU64>,
    pool: Pool,
    template: String,
    cmd: String,
) -> io::Result<()> {
    // The client follows its request with one TAG_STDIN frame carrying the
    // command's stdin (e.g. a tarball to copy files in); empty if none.
    let input = match read_frame(&mut stream) {
        Ok(Some((TAG_STDIN, b))) => b,
        _ => Vec::new(),
    };
    // A private vsock base for this exec; the agent dials `<base>_1`, where we
    // listen. spawn_paused passes the base to the worker as AMBER_VSOCK.
    let n = counter.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("amber-exec-{}-{n}.sock", std::process::id()));
    let base_str = base.to_string_lossy().into_owned();
    let listen_path = format!("{base_str}_1");
    let _ = std::fs::remove_file(&listen_path);
    let listener = UnixListener::bind(&listen_path)?;
    listener.set_nonblocking(true)?;

    let id = match spawn_paused(&reg, &counter, &pool, &template, Some(&base_str)) {
        Ok(id) => id,
        Err(e) => {
            let _ = std::fs::remove_file(&listen_path);
            return write_reply(&mut stream, &Reply::Error { message: e.to_string() });
        }
    };
    // Resume the fork; its agent now dials our listener.
    let released = release(&reg, &id);
    log::debug!("exec: worker {id} released={released}, waiting for agent on {listen_path}");

    let result = match accept_timeout(&listener, Duration::from_secs(15)) {
        Some(mut conn) => {
            log::debug!("exec: agent connected");
            exec_relay(&mut conn, &mut stream, &cmd, &input)
        }
        None => {
            log::warn!("exec: agent did not connect within timeout");
            Err(io::Error::other("exec agent did not connect"))
        }
    };

    // Tear the worker down and clean up the sockets.
    if let Some(flag) = reg.locked().get(&id).map(|e| e.kill.clone()) {
        flag.store(true, Ordering::Relaxed);
    }
    let _ = std::fs::remove_file(&listen_path);
    let _ = std::fs::remove_file(&base);

    match result {
        Ok(code) => write_reply(&mut stream, &Reply::Exit { code }),
        Err(e) => write_reply(&mut stream, &Reply::Error { message: e.to_string() }),
    }
}

/// Accept one connection within `timeout`, polling the nonblocking listener.
fn accept_timeout(listener: &UnixListener, timeout: Duration) -> Option<UnixStream> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((s, _)) => {
                // The connection inherits the listener's nonblocking flag; the
                // relay does blocking reads, so clear it.
                let _ = s.set_nonblocking(false);
                return Some(s);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
    None
}

/// Send the command to the agent, relay its framed output to the client, and
/// return the exit code. Agent frames: `[tag u8][len u32-le][payload]` with tag
/// 1 = stdout, 2 = stderr, 3 = exit (payload = rc i32-le).
fn exec_relay(
    conn: &mut UnixStream,
    client: &mut UnixStream,
    cmd: &str,
    input: &[u8],
) -> io::Result<i32> {
    // Send the command then the stdin blob: each is [u32-le len][bytes].
    let bytes = cmd.as_bytes();
    conn.write_all(&(bytes.len() as u32).to_le_bytes())?;
    conn.write_all(bytes)?;
    conn.write_all(&(input.len() as u32).to_le_bytes())?;
    conn.write_all(input)?;
    conn.flush()?;
    log::debug!("exec_relay: sent cmd ({} bytes) + input ({} bytes)", bytes.len(), input.len());

    loop {
        let mut hdr = [0u8; 5];
        if let Err(e) = conn.read_exact(&mut hdr) {
            log::warn!("exec_relay: header read ended: {e}");
            return Ok(-1); // agent closed without an exit frame
        }
        let len = u32::from_le_bytes(hdr[1..5].try_into().unwrap()) as usize;
        log::debug!("exec_relay: frame tag={} len={len}", hdr[0]);
        if len > proto::MAX_FRAME {
            return Err(io::Error::other("agent frame too large"));
        }
        let mut payload = vec![0u8; len];
        conn.read_exact(&mut payload)?;
        match hdr[0] {
            1 => write_frame(client, TAG_STDOUT, &payload)?,
            2 => write_frame(client, TAG_STDERR, &payload)?,
            3 => {
                let rc = payload.get(..4).and_then(|s| s.try_into().ok()).unwrap_or([0; 4]);
                return Ok(i32::from_le_bytes(rc));
            }
            _ => {}
        }
    }
}

/// Full-duplex relay between the client and a just-released fork's console: the
/// guest's stdout streams to the client, the client's `TAG_STDIN` frames go to the
/// guest. Returns when the guest's stdout closes (it exited) or the client leaves
/// (then the worker is killed so it does not orphan).
fn relay_interactive(
    mut stream: UnixStream,
    cout: Option<std::process::ChildStdout>,
    cin: Option<std::process::ChildStdin>,
    kill: Option<Arc<AtomicBool>>,
) -> io::Result<()> {
    let client_gone = Arc::new(AtomicBool::new(false));

    let mut wout = stream.try_clone()?;
    let gone1 = client_gone.clone();
    let h_out = thread::spawn(move || {
        let Some(mut out) = cout else { return };
        let mut buf = [0u8; 8192];
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if write_frame(&mut wout, TAG_STDOUT, &buf[..n]).is_err() {
                        gone1.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
    });

    let mut rin = stream.try_clone()?;
    let gone2 = client_gone.clone();
    let h_in = thread::spawn(move || {
        let Some(mut cin) = cin else { return };
        loop {
            match read_frame(&mut rin) {
                Ok(Some((TAG_STDIN, payload))) => {
                    if cin.write_all(&payload).and_then(|_| cin.flush()).is_err() {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    gone2.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });

    // The guest closing stdout means it exited; a write failure means the client
    // left. Either way, stop.
    let _ = h_out.join();
    if client_gone.load(Ordering::Relaxed) {
        if let Some(k) = &kill {
            k.store(true, Ordering::Relaxed); // the supervisor reaps the orphan
        }
    }
    let _ = write_reply(&mut stream, &Reply::Exit { code: 0 });
    let _ = stream.shutdown(std::net::Shutdown::Both); // unblock the stdin reader
    let _ = h_in.join();
    Ok(())
}

/// Refill a template's warm pool to its target in the background. Stops early if
/// staging fails (e.g. the RAM budget is full), so the pool self-sizes to budget.
fn refill_pool(reg: Registry, counter: Arc<AtomicU64>, pool: Pool, template: String) {
    let target = pool_target();
    thread::spawn(move || {
        while pool.locked().get(&template).map_or(0, |v| v.len()) < target {
            match spawn_paused(&reg, &counter, &pool, &template, None) {
                Ok(rid) => pool.locked().entry(template.clone()).or_default().push(rid),
                Err(_) => break,
            }
        }
    });
}

/// How many warm forks to keep per template: `[fleet].pool_size`, default 1.
fn pool_target() -> usize {
    crate::manifest::Manifest::load()
        .and_then(|m| m.fleet.pool_size)
        .unwrap_or(1)
        .max(1)
}

// ---- daemon ----

/// Raise the open-file soft limit toward the hard cap. The daemon holds a control
/// socket (and transiently pipes) per live VM, so the default 1024 soft limit caps
/// the fleet around a few hundred VMs — well below where RAM binds. Best-effort:
/// failure just leaves the inherited limit.
fn raise_fd_limit() {
    // When the hard cap is "unlimited", pick a high concrete target instead — the
    // kernel rejects an infinite soft limit (and macOS caps it at maxfilesperproc).
    #[cfg(target_os = "macos")]
    const UNCAPPED_TARGET: libc::rlim_t = 24_576;
    #[cfg(target_os = "linux")]
    const UNCAPPED_TARGET: libc::rlim_t = 1 << 20;

    unsafe {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let target = if lim.rlim_max == libc::RLIM_INFINITY {
            UNCAPPED_TARGET
        } else {
            lim.rlim_max
        };
        if target <= lim.rlim_cur {
            return;
        }
        let newlim = libc::rlimit { rlim_cur: target, rlim_max: lim.rlim_max };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &newlim) == 0 {
            log::info!("amberd: RLIMIT_NOFILE soft limit raised to {target}");
        }
    }
}

pub fn serve() -> io::Result<()> {
    raise_fd_limit();
    let sock = proto::socket_path();
    // Restrict the socket to the owner: a 0700 parent dir and a 0600 socket.
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    log::info!("amberd listening on {}", sock.display());

    let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(AtomicU64::new(0));
    let pool: Pool = Arc::new(Mutex::new(HashMap::new()));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let reg = reg.clone();
        let counter = counter.clone();
        let pool = pool.clone();
        let sock = sock.clone();
        thread::spawn(move || {
            if let Err(e) = handle(stream, reg, counter, pool, &sock) {
                log::warn!("connection error: {e}");
            }
        });
    }
    Ok(())
}

/// Reject any peer whose effective uid is not ours: the socket runs arbitrary
/// images, so only the owner may drive it. The peer-uid syscall differs per OS
/// (macOS `getpeereid`, Linux `SO_PEERCRED`).
#[cfg(target_os = "macos")]
fn authorized(stream: &UnixStream) -> bool {
    let (mut uid, mut gid) = (0u32, 0u32);
    let ok = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
    ok && uid == unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
fn authorized(stream: &UnixStream) -> bool {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    } == 0;
    ok && cred.uid == unsafe { libc::geteuid() }
}

fn handle(
    mut stream: UnixStream,
    reg: Registry,
    counter: Arc<AtomicU64>,
    pool: Pool,
    sock: &std::path::Path,
) -> io::Result<()> {
    if !authorized(&stream) {
        let _ = write_reply(&mut stream, &Reply::Error { message: "unauthorized".into() });
        return Ok(());
    }
    let Some((tag, payload)) = read_frame(&mut stream)? else {
        return Ok(());
    };
    if tag != TAG_REQUEST {
        let _ = write_reply(&mut stream, &Reply::Error { message: "expected a request".into() });
        return Ok(());
    }
    let req: Request = match serde_json::from_slice(&payload) {
        Ok(r) => r,
        Err(e) => {
            let _ = write_reply(&mut stream, &Reply::Error { message: format!("bad request: {e}") });
            return Ok(());
        }
    };

    match req {
        Request::List => {
            // Snapshot under the lock, then sample RSS (a subprocess) unlocked.
            let mut vms: Vec<VmInfo> = reg.locked().values().map(|e| e.info.clone()).collect();
            for v in &mut vms {
                v.rss_bytes = rss_bytes(v.pid);
            }
            write_reply(&mut stream, &Reply::Vms { vms })?;
        }
        Request::Budget => {
            let pids: Vec<(u64, u32)> = {
                let g = reg.locked();
                g.values().map(|e| (e.info.ram_bytes, e.info.pid)).collect()
            };
            let used: u64 = pids.iter().map(|(ram, _)| ram).sum();
            let rss: u64 = pids.iter().map(|(_, pid)| rss_bytes(*pid)).sum();
            write_reply(
                &mut stream,
                &Reply::Budget { budget: ram_budget().unwrap_or(0), used, rss, machine: machine_ram() },
            )?;
        }
        Request::Balloon { id, mib } => {
            let mut frame = vec![amber_core::control::BALLOON];
            frame.extend_from_slice(&mib.to_le_bytes());
            let reply = control_op(&reg, &id, &frame, None);
            write_reply(&mut stream, &reply)?;
        }
        Request::Pause { id } => {
            let reply = control_op(&reg, &id, &[amber_core::control::PAUSE], Some(true));
            write_reply(&mut stream, &reply)?;
        }
        Request::Resume { id } => {
            let reply = control_op(&reg, &id, &[amber_core::control::RESUME], Some(false));
            write_reply(&mut stream, &reply)?;
        }
        Request::Fork { template, interactive } => {
            return handle_fork(stream, reg, counter, pool, template, interactive);
        }
        Request::Exec { template, cmd } => {
            return handle_exec(stream, reg, counter, pool, template, cmd);
        }
        Request::Kill { id } => {
            // Signal the owner thread; it kills and reaps the child it owns.
            let found = reg.locked().get(&id).map(|e| e.kill.clone());
            match found {
                Some(flag) => {
                    flag.store(true, Ordering::Relaxed);
                    write_reply(&mut stream, &Reply::Ok)?;
                }
                None => write_reply(&mut stream, &Reply::Error { message: format!("no such vm: {id}") })?,
            }
        }
        Request::Shutdown => {
            write_reply(&mut stream, &Reply::Ok)?;
            // Kill every VM before exiting so detached ones aren't orphaned. Skip
            // pid 0 (reserved but not yet spawned): kill(0, …) would signal amberd's
            // whole process group, including amberd itself.
            for e in reg.locked().values() {
                if e.info.pid != 0 {
                    unsafe { libc::kill(e.info.pid as i32, libc::SIGKILL) };
                }
            }
            let _ = std::fs::remove_file(sock);
            log::info!("amberd shutting down");
            std::process::exit(0);
        }
        Request::RunOneShot { reference, argv } => {
            run_one_shot(stream, reg, counter, pool, reference, argv)?;
        }
        Request::RunDetached { reference, argv } => {
            start_detached(stream, reg, counter, pool, reference, argv)?;
        }
        Request::Logs { id } => {
            stream_logs(stream, &id)?;
        }
    }
    Ok(())
}

fn logs_dir() -> std::path::PathBuf {
    crate::cache_dir().join("logs")
}

/// Start a VM that keeps running after the client leaves; its stdout/stderr go to
/// `amber-cache/logs/<id>.log`. Replies with the id immediately.
fn start_detached(
    mut stream: UnixStream,
    reg: Registry,
    counter: Arc<AtomicU64>,
    pool: Pool,
    reference: String,
    argv: Vec<String>,
) -> io::Result<()> {
    let (id, kill) = match reserve_with_evict(&reg, &counter, &pool, &reference) {
        Ok(x) => x,
        Err((budget, used, requested)) => {
            write_reply(&mut stream, &Reply::BudgetExceeded { budget, used, requested })?;
            return Ok(());
        }
    };

    let dir = logs_dir();
    std::fs::create_dir_all(&dir)?;
    let out = std::fs::File::create(dir.join(format!("{id}.log")))?;
    let err = out.try_clone()?;

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("__vm").arg(&reference);
    if !argv.is_empty() {
        cmd.arg("--");
        cmd.args(&argv);
    }
    cmd.stdin(Stdio::null()).stdout(out).stderr(err);
    let (ctl_host, ctl_child) = match attach_control(&mut cmd) {
        Ok(p) => p,
        Err(e) => {
            reg.locked().remove(&id);
            return Err(e);
        }
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            reg.locked().remove(&id); // release the reservation
            return Err(e);
        }
    };
    drop(ctl_child); // only the child holds its end now
    set_runtime(&reg, &id, child.id(), ctl_host);

    // Background supervisor: reap on exit or kill, then deregister.
    let reg2 = reg.clone();
    let id2 = id.clone();
    thread::spawn(move || {
        let never = AtomicBool::new(false);
        let _ = supervise(&mut child, &kill, &never);
        reg2.locked().remove(&id2);
    });

    write_reply(&mut stream, &Reply::Started { id, vsock: None })?;
    Ok(())
}

fn stream_logs(mut stream: UnixStream, id: &str) -> io::Result<()> {
    let path = logs_dir().join(format!("{id}.log"));
    match std::fs::read(&path) {
        Ok(bytes) => {
            write_frame(&mut stream, TAG_STDOUT, &bytes)?;
            write_reply(&mut stream, &Reply::Ok)?;
        }
        Err(_) => write_reply(&mut stream, &Reply::Error { message: format!("no logs for {id}") })?,
    }
    Ok(())
}

fn run_one_shot(
    mut stream: UnixStream,
    reg: Registry,
    counter: Arc<AtomicU64>,
    pool: Pool,
    reference: String,
    argv: Vec<String>,
) -> io::Result<()> {
    let (id, kill) = match reserve_with_evict(&reg, &counter, &pool, &reference) {
        Ok(x) => x,
        Err((budget, used, requested)) => {
            write_reply(&mut stream, &Reply::BudgetExceeded { budget, used, requested })?;
            return Ok(());
        }
    };

    // Spawn the per-VM worker process (same binary, internal subcommand).
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("__vm").arg(&reference);
    if !argv.is_empty() {
        cmd.arg("--");
        cmd.args(&argv);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    let (ctl_host, ctl_child) = match attach_control(&mut cmd) {
        Ok(p) => p,
        Err(e) => {
            reg.locked().remove(&id);
            return Err(e);
        }
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            reg.locked().remove(&id); // release the reservation
            return Err(e);
        }
    };
    drop(ctl_child); // only the child holds its end now
    set_runtime(&reg, &id, child.id(), ctl_host);

    // `client_gone` is set when either relay sees the client disconnect, so the
    // supervisor below kills an otherwise-orphaned VM.
    let client_gone = Arc::new(AtomicBool::new(false));
    let cout = child.stdout.take();
    let cin = child.stdin.take();

    // stdout: child -> client.
    let h_out = {
        let mut w = stream.try_clone()?;
        let gone = client_gone.clone();
        thread::spawn(move || {
            let Some(mut out) = cout else { return };
            let mut buf = [0u8; 8192];
            loop {
                match out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if write_frame(&mut w, TAG_STDOUT, &buf[..n]).is_err() {
                            gone.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
        })
    };

    // stdin: client -> child. EOF/error here means the client is gone.
    let h_in = {
        let mut r = stream.try_clone()?;
        let gone = client_gone.clone();
        thread::spawn(move || {
            let Some(mut cin) = cin else { return };
            loop {
                match read_frame(&mut r) {
                    Ok(Some((TAG_STDIN, payload))) => {
                        if cin.write_all(&payload).and_then(|_| cin.flush()).is_err() {
                            break;
                        }
                    }
                    Ok(Some(_)) => {}
                    _ => {
                        gone.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        })
    };

    // Supervise: exit on guest shutdown, or kill the child if asked (rm) or if the
    // client disconnected.
    let code = supervise(&mut child, &kill, &client_gone);
    reg.locked().remove(&id);

    // Tell the client, then close the socket so the relay threads unblock and join.
    let _ = write_reply(&mut stream, &Reply::Exit { code });
    let _ = stream.shutdown(Shutdown::Both);
    let _ = h_out.join();
    let _ = h_in.join();
    Ok(())
}

fn supervise(child: &mut Child, kill: &AtomicBool, client_gone: &AtomicBool) -> i32 {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(-1),
            Ok(None) => {}
            Err(_) => return -1,
        }
        if kill.load(Ordering::Relaxed) || client_gone.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return -1;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

// ---- clients ----

fn connect() -> Option<UnixStream> {
    UnixStream::connect(proto::socket_path()).ok()
}

/// Is a daemon reachable?
pub fn running() -> bool {
    connect().is_some()
}

/// Send a request and read one terminal reply (no streaming).
fn request(req: &Request) -> io::Result<Reply> {
    let mut s = connect().ok_or_else(|| io::Error::other("no amberd (run 'amber up')"))?;
    proto::write_request(&mut s, req)?;
    match read_frame(&mut s)? {
        Some((TAG_REPLY, payload)) => Ok(serde_json::from_slice(&payload)?),
        _ => Err(io::Error::other("unexpected reply")),
    }
}

/// Run via the daemon: forward our stdin, stream the guest's stdout to ours,
/// return the exit code.
pub fn run_client(reference: &str, argv: &[String]) -> io::Result<i32> {
    stream_client(&Request::RunOneShot { reference: reference.to_string(), argv: argv.to_vec() })
}

/// Fork interactively from a template: stream the resumed guest's console.
pub fn fork_interactive(template: &str) -> io::Result<i32> {
    stream_client(&Request::Fork { template: template.to_string(), interactive: true })
}

/// Run a fresh command in a fork of a ready-to-exec template: send `Exec`, then
/// stream the in-guest agent's stdout and stderr (kept distinct) to ours and
/// return the structured exit code. No in-band marker — the code arrives in the
/// terminal `Exit` reply.
/// True if stdin has input ready (or is at EOF) to read without blocking. A regular
/// file, `/dev/null`, and a pipe that already carries data all return immediately;
/// an open-but-idle inherited stdin times out and returns false, so `exec` doesn't
/// hang waiting for input nobody is going to send. Producers (`tar | amber exec`)
/// signal readable within the grace window.
fn stdin_has_input() -> bool {
    let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    let rc = unsafe { libc::poll(&mut pfd, 1, 200) };
    rc > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP)) != 0
}

pub fn exec(template: &str, cmd: &str) -> io::Result<i32> {
    let mut s = connect().ok_or_else(|| io::Error::other("no amberd (run 'amber up')"))?;
    proto::write_request(
        &mut s,
        &Request::Exec { template: template.to_string(), cmd: cmd.to_string() },
    )?;
    // Follow with our stdin as one blob (the command's stdin in the guest, e.g. a
    // tarball to copy files in). We read it only when there's actually input to
    // send: a terminal, or an inherited-but-idle stdin (the common case when a
    // program spawns `amber exec` without piping anything) must NOT make us block
    // on a read that never reaches EOF. A regular file, `/dev/null`, or a producing
    // pipe (`tar | amber exec`) all signal readable at once, so they still stream.
    let input = if unsafe { libc::isatty(0) } == 1 || !stdin_has_input() {
        Vec::new()
    } else {
        let mut b = Vec::new();
        let _ = io::Read::read_to_end(&mut io::stdin(), &mut b);
        b
    };
    write_frame(&mut s, TAG_STDIN, &input)?;

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    loop {
        match read_frame(&mut s)? {
            Some((TAG_STDOUT, bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Some((TAG_STDERR, bytes)) => {
                stderr.write_all(&bytes)?;
                stderr.flush()?;
            }
            Some((TAG_REPLY, payload)) => {
                return match serde_json::from_slice::<Reply>(&payload)? {
                    Reply::Exit { code } => Ok(code),
                    Reply::Error { message } => Err(io::Error::other(message)),
                    Reply::BudgetExceeded { budget, used, requested } => {
                        Err(io::Error::other(budget_msg(budget, used, requested)))
                    }
                    _ => Ok(0),
                };
            }
            _ => return Ok(0),
        }
    }
}

/// Send `req`, then relay our stdin to the VM and its stdout to ours until the
/// terminal Exit. Shared by interactive `run` and `fork`.
fn stream_client(req: &Request) -> io::Result<i32> {
    let mut s = connect().ok_or_else(|| io::Error::other("no amberd"))?;
    proto::write_request(&mut s, req)?;

    // Forward our stdin to the VM on a side thread; the process exiting on the
    // terminal Exit reply tears this down.
    if let Ok(mut wr) = s.try_clone() {
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    break;
                }
                if write_frame(&mut wr, TAG_STDIN, &buf[..n as usize]).is_err() {
                    break;
                }
            }
        });
    }

    let mut stdout = io::stdout();
    loop {
        match read_frame(&mut s)? {
            Some((TAG_STDOUT, bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Some((TAG_REPLY, payload)) => {
                return match serde_json::from_slice::<Reply>(&payload)? {
                    Reply::Exit { code } => Ok(code),
                    Reply::Error { message } => Err(io::Error::other(message)),
                    Reply::BudgetExceeded { budget, used, requested } => {
                        Err(io::Error::other(budget_msg(budget, used, requested)))
                    }
                    _ => Ok(0),
                };
            }
            _ => return Ok(0),
        }
    }
}

/// Start a detached VM; returns its id.
pub fn run_detached(reference: &str, argv: &[String]) -> io::Result<String> {
    match request(&Request::RunDetached { reference: reference.to_string(), argv: argv.to_vec() })? {
        Reply::Started { id, .. } => Ok(id),
        Reply::BudgetExceeded { budget, used, requested } => {
            Err(io::Error::other(budget_msg(budget, used, requested)))
        }
        Reply::Error { message } => Err(io::Error::other(message)),
        _ => Err(io::Error::other("unexpected reply")),
    }
}

/// Fork a detached VM from a warm template; returns its id and vsock UDS base.
pub fn fork(template: &str) -> io::Result<(String, Option<String>)> {
    match request(&Request::Fork { template: template.to_string(), interactive: false })? {
        Reply::Started { id, vsock } => Ok((id, vsock)),
        Reply::BudgetExceeded { budget, used, requested } => {
            Err(io::Error::other(budget_msg(budget, used, requested)))
        }
        Reply::Error { message } => Err(io::Error::other(message)),
        _ => Err(io::Error::other("unexpected reply")),
    }
}

/// Fleet RAM budget, cap-based usage, real RSS, and host RAM, in bytes.
pub fn budget() -> io::Result<(u64, u64, u64, u64)> {
    match request(&Request::Budget)? {
        Reply::Budget { budget, used, rss, machine } => Ok((budget, used, rss, machine)),
        Reply::Error { message } => Err(io::Error::other(message)),
        _ => Err(io::Error::other("unexpected reply")),
    }
}

fn budget_msg(budget: u64, used: u64, requested: u64) -> String {
    let mib = |b: u64| b / (1024 * 1024);
    format!(
        "BudgetExceeded: budget {} MiB, in use {} MiB, requested {} MiB",
        mib(budget),
        mib(used),
        mib(requested)
    )
}

/// Print a VM's captured log to our stdout.
pub fn logs(id: &str) -> io::Result<()> {
    let mut s = connect().ok_or_else(|| io::Error::other("no amberd"))?;
    proto::write_request(&mut s, &Request::Logs { id: id.to_string() })?;
    let mut stdout = io::stdout();
    loop {
        match read_frame(&mut s)? {
            Some((TAG_STDOUT, bytes)) => stdout.write_all(&bytes)?,
            Some((TAG_REPLY, payload)) => {
                return match serde_json::from_slice::<Reply>(&payload)? {
                    Reply::Error { message } => Err(io::Error::other(message)),
                    _ => Ok(()),
                };
            }
            _ => return Ok(()),
        }
    }
}

pub fn list() -> io::Result<Vec<VmInfo>> {
    match request(&Request::List)? {
        Reply::Vms { vms } => Ok(vms),
        Reply::Error { message } => Err(io::Error::other(message)),
        _ => Ok(Vec::new()),
    }
}

/// Ask a VM's balloon to reclaim toward `mib` MiB.
pub fn balloon(id: &str, mib: u64) -> io::Result<()> {
    match request(&Request::Balloon { id: id.to_string(), mib })? {
        Reply::Ok => Ok(()),
        Reply::Error { message } => Err(io::Error::other(message)),
        _ => Ok(()),
    }
}

pub fn kill(id: &str) -> io::Result<()> {
    match request(&Request::Kill { id: id.to_string() })? {
        Reply::Ok => Ok(()),
        Reply::Error { message } => Err(io::Error::other(message)),
        _ => Ok(()),
    }
}

/// Freeze a running VM in place.
pub fn pause(id: &str) -> io::Result<()> {
    match request(&Request::Pause { id: id.to_string() })? {
        Reply::Ok => Ok(()),
        Reply::Error { message } => Err(io::Error::other(message)),
        _ => Ok(()),
    }
}

/// Resume a paused VM.
pub fn resume(id: &str) -> io::Result<()> {
    match request(&Request::Resume { id: id.to_string() })? {
        Reply::Ok => Ok(()),
        Reply::Error { message } => Err(io::Error::other(message)),
        _ => Ok(()),
    }
}

pub fn shutdown() -> io::Result<()> {
    match request(&Request::Shutdown) {
        Ok(_) | Err(_) => Ok(()), // the daemon exits as it replies; a dropped conn is fine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_entry(id: &str) -> VmEntry {
        VmEntry {
            info: VmInfo {
                id: id.to_string(),
                reference: "test".to_string(),
                pid: 0,
                started: 0,
                ram_bytes: 0,
                rss_bytes: 0,
                paused: false,
            },
            kill: Arc::new(AtomicBool::new(false)),
            control: None,
            stdout: None,
            stdin: None,
            vsock: None,
        }
    }

    #[test]
    fn fork_vsock_base_is_deterministic_and_unique_per_id() {
        assert_eq!(fork_vsock_base(7, "vm1"), fork_vsock_base(7, "vm1"));
        assert_ne!(fork_vsock_base(7, "vm1"), fork_vsock_base(7, "vm2"));
        assert!(fork_vsock_base(7, "vm1").contains("vm1"));
    }

    #[test]
    fn evict_one_pooled_removes_entry_and_signals_kill() {
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let pool: Pool = Arc::new(Mutex::new(HashMap::new()));
        let e = fake_entry("vm1");
        let kill = e.kill.clone();
        reg.locked().insert("vm1".to_string(), e);
        pool.locked().insert("tmpl".to_string(), vec!["vm1".to_string()]);

        assert!(evict_one_pooled(&reg, &pool));
        assert!(!reg.locked().contains_key("vm1"));
        assert!(kill.load(Ordering::Relaxed), "supervisor must be told to reap");
    }

    #[test]
    fn evict_one_pooled_is_false_when_pool_empty() {
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let pool: Pool = Arc::new(Mutex::new(HashMap::new()));
        assert!(!evict_one_pooled(&reg, &pool));
    }

    #[test]
    fn poisoned_registry_still_serves() {
        // M3: a thread panicking under the registry lock poisons the Mutex. The
        // daemon must keep managing VMs instead of every later op panicking.
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        reg.locked().insert("vm1".to_string(), fake_entry("vm1"));

        let r2 = reg.clone();
        let _ = std::thread::spawn(move || {
            let _g = r2.lock().unwrap();
            panic!("boom while holding the registry lock");
        })
        .join();

        assert!(reg.lock().is_err(), "mutex should be poisoned by the panic");
        // The recovering accessor still works and preserves prior state.
        assert!(reg.locked().contains_key("vm1"));
        assert!(!release(&reg, "vm1")); // still callable, no panic
    }

    #[test]
    fn release_is_false_without_a_control_channel() {
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        reg.locked().insert("vm1".to_string(), fake_entry("vm1"));
        assert!(!release(&reg, "vm1")); // control is None
        assert!(!release(&reg, "missing"));
    }
}
