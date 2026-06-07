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
}

type Registry = Arc<Mutex<HashMap<String, VmEntry>>>;

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
        },
    );
    Ok((id, kill))
}

/// Record a reserved VM's pid once its worker has spawned.
fn set_pid(reg: &Registry, id: &str, pid: u32) {
    if let Some(e) = reg.lock().unwrap().get_mut(id) {
        e.info.pid = pid;
    }
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

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let reg = reg.clone();
        let counter = counter.clone();
        let sock = sock.clone();
        thread::spawn(move || {
            if let Err(e) = handle(stream, reg, counter, &sock) {
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
            write_reply(&mut stream, &Reply::Budget { budget: ram_budget().unwrap_or(0), used, rss })?;
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
            run_one_shot(stream, reg, counter, reference, argv)?;
        }
        Request::RunDetached { reference, argv } => {
            start_detached(stream, reg, counter, reference, argv)?;
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
    reference: String,
    argv: Vec<String>,
) -> io::Result<()> {
    let (id, kill) = match reserve(&reg, &counter, &reference) {
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
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            reg.lock().unwrap().remove(&id); // release the reservation
            return Err(e);
        }
    };
    set_pid(&reg, &id, child.id());

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
    reference: String,
    argv: Vec<String>,
) -> io::Result<()> {
    let (id, kill) = match reserve(&reg, &counter, &reference) {
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
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            reg.lock().unwrap().remove(&id); // release the reservation
            return Err(e);
        }
    };
    set_pid(&reg, &id, child.id());

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

/// The fleet RAM budget, cap-based usage, and real RSS in bytes (0 = unlimited).
pub fn budget() -> io::Result<(u64, u64, u64)> {
    match request(&Request::Budget)? {
        Reply::Budget { budget, used, rss } => Ok((budget, used, rss)),
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
