//! `amberd`: the control-plane supervisor and its clients.
//!
//! HVF allows one VM per process, so amberd does not host VMs itself — it owns
//! the control socket and a registry, and spawns one child `amber __vm` process
//! per VM (which also gives each VM its own restricted host process, the security
//! model amber wants). `RunOneShot` spawns a child and relays its stdout to the
//! client; `List`/`Kill`/`Shutdown` manage the fleet.

use crate::proto::{self, read_frame, write_frame, write_reply, Reply, Request, VmInfo, TAG_REPLY, TAG_REQUEST, TAG_STDOUT};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

type Registry = Arc<Mutex<HashMap<String, VmInfo>>>;

// ---- daemon ----

pub fn serve() -> io::Result<()> {
    let sock = proto::socket_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
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

fn handle(
    mut stream: UnixStream,
    reg: Registry,
    counter: Arc<AtomicU64>,
    sock: &std::path::Path,
) -> io::Result<()> {
    let Some((tag, payload)) = read_frame(&mut stream)? else {
        return Ok(());
    };
    if tag != TAG_REQUEST {
        return Ok(());
    }
    let req: Request = serde_json::from_slice(&payload)?;

    match req {
        Request::List => {
            let vms = reg.lock().unwrap().values().cloned().collect();
            write_reply(&mut stream, &Reply::Vms { vms })?;
        }
        Request::Kill { id } => {
            let removed = reg.lock().unwrap().remove(&id);
            match removed {
                Some(info) => {
                    unsafe { libc::kill(info.pid as i32, libc::SIGKILL) };
                    write_reply(&mut stream, &Reply::Ok)?;
                }
                None => write_reply(&mut stream, &Reply::Error { message: format!("no such vm: {id}") })?,
            }
        }
        Request::Shutdown => {
            write_reply(&mut stream, &Reply::Ok)?;
            let _ = std::fs::remove_file(sock);
            log::info!("amberd shutting down");
            std::process::exit(0);
        }
        Request::RunOneShot { reference, argv } => {
            run_one_shot(stream, reg, counter, reference, argv)?;
        }
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
    // Spawn the per-VM worker process (same binary, internal subcommand).
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("__vm").arg(&reference);
    if !argv.is_empty() {
        cmd.arg("--");
        cmd.args(&argv);
    }
    cmd.stdout(Stdio::piped());
    let mut child = cmd.spawn()?;

    let id = format!("vm{}", counter.fetch_add(1, Ordering::Relaxed) + 1);
    let pid = child.id();
    reg.lock().unwrap().insert(
        id.clone(),
        VmInfo { id: id.clone(), reference, pid },
    );

    // Relay the guest's stdout to the client until the VM exits.
    if let Some(mut out) = child.stdout.take() {
        let mut buf = [0u8; 8192];
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if write_frame(&mut stream, TAG_STDOUT, &buf[..n]).is_err() {
                        let _ = child.kill();
                        break;
                    }
                }
            }
        }
    }
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
    reg.lock().unwrap().remove(&id);
    let _ = write_reply(&mut stream, &Reply::Exit { code });
    Ok(())
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

/// Run via the daemon: stream guest stdout to our stdout, return the exit code.
pub fn run_client(reference: &str, argv: &[String]) -> io::Result<i32> {
    let mut s = connect().ok_or_else(|| io::Error::other("no amberd"))?;
    proto::write_request(
        &mut s,
        &Request::RunOneShot { reference: reference.to_string(), argv: argv.to_vec() },
    )?;
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
                    _ => Ok(0),
                };
            }
            _ => return Ok(0),
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
