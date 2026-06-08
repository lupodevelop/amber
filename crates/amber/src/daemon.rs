//! `amberd`: the control-plane supervisor and its clients.
//!
//! HVF allows one VM per process, so amberd does not host VMs itself — it owns
//! the control socket and a registry, and spawns one child `amber __vm` process
//! per VM (which also gives each VM its own restricted host process, the security
//! model amber wants). `RunOneShot` spawns a child and relays its stdin/stdout to
//! the client; `List`/`Kill`/`Shutdown` manage the fleet.

use crate::proto::{
    self, read_frame, write_frame, write_reply, Reply, Request, VmInfo, TAG_REPLY, TAG_REQUEST,
    TAG_STDIN, TAG_STDOUT,
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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A live VM's registry entry: its public info plus a flag the owning supervisor
/// thread watches so `Kill` never has to touch a raw pid (no PID-reuse race).
struct VmEntry {
    info: VmInfo,
    kill: Arc<AtomicBool>,
    /// amberd's end of the control channel to the VM (balloon targets, etc.).
    control: Option<UnixStream>,
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

/// Total host RAM in bytes (via `sysctl hw.memsize`; 0 if unavailable).
fn machine_ram() -> u64 {
    Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Real resident memory of a process, in bytes (via `ps`; 0 if unavailable).
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

    let mut g = reg.lock().unwrap();
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
            },
            kill: kill.clone(),
            control: None,
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
        let mut p = pool.lock().unwrap();
        p.values_mut().find_map(|v| v.pop())
    };
    let Some(id) = id else { return false };
    let mut g = reg.lock().unwrap();
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
    if let Some(e) = reg.lock().unwrap().get_mut(id) {
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
fn spawn_paused(reg: &Registry, counter: &AtomicU64, pool: &Pool, template: &str) -> io::Result<String> {
    let (id, kill) = reserve(reg, counter, template)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "RAM budget exceeded"))?;

    let dir = logs_dir();
    std::fs::create_dir_all(&dir)?;
    let out = std::fs::File::create(dir.join(format!("{id}.log")))?;
    let err = out.try_clone()?;

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    // The template only restores correctly on the software GIC (the timer survives
    // there), so the worker runs in swgic mode regardless of the daemon's env.
    cmd.arg("restore").arg(template);
    cmd.env("AMBER_PAUSED", "1").env("AMBER_GIC", "sw");
    cmd.stdin(Stdio::null()).stdout(out).stderr(err);
    let (mut ctl_host, ctl_child) = match attach_control(&mut cmd) {
        Ok(p) => p,
        Err(e) => {
            reg.lock().unwrap().remove(&id);
            return Err(e);
        }
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            reg.lock().unwrap().remove(&id);
            return Err(e);
        }
    };
    drop(ctl_child);
    let pid = child.id();

    // Block until the worker finishes the restore and signals ready (one byte).
    // This is the warming cost, paid ahead of any fork request.
    let mut ready = [0u8; 1];
    if io::Read::read_exact(&mut ctl_host, &mut ready).is_err() {
        reg.lock().unwrap().remove(&id);
        let _ = child.kill();
        return Err(io::Error::new(io::ErrorKind::Other, "worker failed to warm"));
    }
    set_runtime(reg, &id, pid, ctl_host);

    let reg2 = reg.clone();
    let pool2 = pool.clone();
    let id2 = id.clone();
    let tmpl2 = template.to_string();
    thread::spawn(move || {
        let never = AtomicBool::new(false);
        let _ = supervise(&mut child, &kill, &never);
        reg2.lock().unwrap().remove(&id2);
        if let Some(v) = pool2.lock().unwrap().get_mut(&tmpl2) {
            v.retain(|x| x != &id2);
        }
    });
    Ok(id)
}

/// Release a paused worker by writing the go byte to its control channel.
fn release(reg: &Registry, id: &str) -> bool {
    let g = reg.lock().unwrap();
    match g.get(id).and_then(|e| e.control.as_ref()) {
        Some(ctl) => {
            let mut w: &UnixStream = ctl;
            io::Write::write_all(&mut w, &[1u8]).is_ok()
        }
        None => false,
    }
}

fn handle_fork(
    mut stream: UnixStream,
    reg: Registry,
    counter: Arc<AtomicU64>,
    pool: Pool,
    template: String,
) -> io::Result<()> {
    // Take a pre-warmed worker if the pool has one; otherwise stage one now.
    let pooled = pool.lock().unwrap().get_mut(&template).and_then(|v| v.pop());
    let id = match pooled {
        Some(id) => id,
        None => match spawn_paused(&reg, &counter, &pool, &template) {
            Ok(id) => id,
            Err(e) => {
                write_reply(&mut stream, &Reply::Error { message: e.to_string() })?;
                return Ok(());
            }
        },
    };

    if !release(&reg, &id) {
        write_reply(&mut stream, &Reply::Error { message: format!("failed to release {id}") })?;
        return Ok(());
    }
    write_reply(&mut stream, &Reply::Started { id })?;

    // Refill the pool to its target in the background, so the next forks are warm
    // too. Stops early if staging fails (e.g. the RAM budget is full) — the pool
    // is then naturally sized by whatever the budget allows.
    let target = pool_target();
    thread::spawn(move || {
        while pool.lock().unwrap().get(&template).map_or(0, |v| v.len()) < target {
            match spawn_paused(&reg, &counter, &pool, &template) {
                Ok(rid) => pool.lock().unwrap().entry(template.clone()).or_default().push(rid),
                Err(_) => break,
            }
        }
    });
    Ok(())
}

/// How many warm forks to keep per template: `[fleet].pool_size`, default 1.
fn pool_target() -> usize {
    crate::manifest::Manifest::load()
        .and_then(|m| m.fleet.pool_size)
        .unwrap_or(1)
        .max(1)
}

// ---- daemon ----

pub fn serve() -> io::Result<()> {
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
/// images, so only the owner may drive it.
fn authorized(stream: &UnixStream) -> bool {
    let (mut uid, mut gid) = (0u32, 0u32);
    let ok = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
    ok && uid == unsafe { libc::geteuid() }
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
            let mut vms: Vec<VmInfo> = reg.lock().unwrap().values().map(|e| e.info.clone()).collect();
            for v in &mut vms {
                v.rss_bytes = rss_bytes(v.pid);
            }
            write_reply(&mut stream, &Reply::Vms { vms })?;
        }
        Request::Budget => {
            let pids: Vec<(u64, u32)> = {
                let g = reg.lock().unwrap();
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
            let reply = {
                let g = reg.lock().unwrap();
                match g.get(&id).and_then(|e| e.control.as_ref()) {
                    Some(ctl) => {
                        let mut w: &UnixStream = ctl;
                        match w.write_all(&mib.to_le_bytes()).and_then(|_| w.flush()) {
                            Ok(()) => Reply::Ok,
                            Err(e) => Reply::Error { message: format!("control write: {e}") },
                        }
                    }
                    None => Reply::Error { message: format!("no such vm: {id}") },
                }
            };
            write_reply(&mut stream, &reply)?;
        }
        Request::Fork { template } => {
            return handle_fork(stream, reg, counter, pool, template);
        }
        Request::Kill { id } => {
            // Signal the owner thread; it kills and reaps the child it owns.
            let found = reg.lock().unwrap().get(&id).map(|e| e.kill.clone());
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
            // Kill every VM before exiting so detached ones aren't orphaned.
            for e in reg.lock().unwrap().values() {
                unsafe { libc::kill(e.info.pid as i32, libc::SIGKILL) };
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
    std::path::PathBuf::from("amber-cache/logs")
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
            reg.lock().unwrap().remove(&id);
            return Err(e);
        }
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            reg.lock().unwrap().remove(&id); // release the reservation
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
        reg2.lock().unwrap().remove(&id2);
    });

    write_reply(&mut stream, &Reply::Started { id })?;
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
            reg.lock().unwrap().remove(&id);
            return Err(e);
        }
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            reg.lock().unwrap().remove(&id); // release the reservation
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
    reg.lock().unwrap().remove(&id);

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
    let mut s = connect().ok_or_else(|| io::Error::other("no amberd"))?;
    proto::write_request(
        &mut s,
        &Request::RunOneShot { reference: reference.to_string(), argv: argv.to_vec() },
    )?;

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
        Reply::Started { id } => Ok(id),
        Reply::BudgetExceeded { budget, used, requested } => {
            Err(io::Error::other(budget_msg(budget, used, requested)))
        }
        Reply::Error { message } => Err(io::Error::other(message)),
        _ => Err(io::Error::other("unexpected reply")),
    }
}

/// Fork a VM from a warm template; returns the new VM's id.
pub fn fork(template: &str) -> io::Result<String> {
    match request(&Request::Fork { template: template.to_string() })? {
        Reply::Started { id } => Ok(id),
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

pub fn shutdown() -> io::Result<()> {
    match request(&Request::Shutdown) {
        Ok(_) | Err(_) => Ok(()), // the daemon exits as it replies; a dropped conn is fine
    }
}
