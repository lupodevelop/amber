//! amber's in-guest exec agent.
//!
//! Baked into a template's initramfs and run as its command. On boot it prints
//! the readiness marker (so amber snapshots the template right here, *before* any
//! host connection exists — vsock state can't be snapshotted), then blocks. When
//! the template is restored for an `exec`, it dials the host over AF_VSOCK, reads
//! one command, runs it under `sh -c`, and streams the result back framed:
//!
//!   host -> agent:  [len: u32-le][command bytes]
//!   agent -> host:  [tag: u8][len: u32-le][payload]   repeated
//!                   tag 1 = stdout, 2 = stderr, 3 = exit (payload = rc i32-le)
//!
//! stdout and stderr are distinct streams and the exit code is a structured
//! frame — no in-band marker to misparse, and binary output passes through clean.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const AF_VSOCK: libc::c_int = 40;
const HOST_CID: libc::c_uint = 2; // VMADDR_CID_HOST
const EXEC_PORT: libc::c_uint = 1;
const READY: &[u8] = b"__AMBER_READY__\n";

/// Dial the host's exec port over AF_VSOCK. The connection is what restore turns
/// "live", so this only succeeds once the host (daemon) is listening for the fork.
fn dial_host() -> File {
    let fd = unsafe { libc::socket(AF_VSOCK, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0, "vsock socket");
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = AF_VSOCK as libc::sa_family_t;
    addr.svm_port = EXEC_PORT;
    addr.svm_cid = HOST_CID;
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "vsock connect");
    unsafe { File::from_raw_fd(fd) }
}

fn write_frame(sock: &Mutex<File>, tag: u8, payload: &[u8]) {
    let mut hdr = [0u8; 5];
    hdr[0] = tag;
    hdr[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    let mut g = sock.lock().unwrap();
    let _ = g.write_all(&hdr);
    let _ = g.write_all(payload);
    let _ = g.flush();
}

/// Read exactly `n` bytes, or fewer on EOF.
fn read_n(r: &mut impl Read, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    let mut off = 0;
    while off < n {
        match r.read(&mut buf[off..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => off += k,
        }
    }
    buf.truncate(off);
    buf
}

/// Copy `tag`-framed chunks from a child pipe to the host until EOF.
fn pump(mut pipe: impl Read, sock: Arc<Mutex<File>>, tag: u8) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => write_frame(&sock, tag, &buf[..n]),
        }
    }
}

fn main() {
    // Snapshot point: announce readiness on the console, then dial. amber captures
    // the template here, so the restored fork resumes exactly at the dial.
    let _ = std::io::stdout().write_all(READY);
    let _ = std::io::stdout().flush();

    // Normally dial the host over vsock. AMBER_AGENT_FD=<fd> instead adopts an
    // already-connected fd — used by the protocol test, which has no vsock.
    let conn = match std::env::var("AMBER_AGENT_FD").ok().and_then(|s| s.parse::<i32>().ok()) {
        Some(fd) => unsafe { File::from_raw_fd(fd) },
        None => dial_host(),
    };
    let sock = Arc::new(Mutex::new(conn));

    // Read the command: a u32-le length then that many bytes.
    let cmd = {
        let mut g = sock.lock().unwrap();
        let len = u32::from_le_bytes(read_n(&mut *g, 4).try_into().unwrap_or([0; 4])) as usize;
        read_n(&mut *g, len)
    };

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(std::ffi::OsStr::from_bytes(&cmd))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            write_frame(&sock, 3, &127i32.to_le_bytes()); // shell not found
            return;
        }
    };

    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let so = sock.clone();
    let se = sock.clone();
    let h_out = std::thread::spawn(move || pump(out, so, 1));
    let h_err = std::thread::spawn(move || pump(err, se, 2));
    let _ = h_out.join();
    let _ = h_err.join();

    let rc = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
    write_frame(&sock, 3, &(rc as i32).to_le_bytes());
}
