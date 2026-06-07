//! The amberd control protocol: length-prefixed frames over a unix socket.
//!
//! A frame is `[tag: u8][len: u32 LE][payload: len bytes]`. Control frames carry
//! JSON ([`Request`]/[`Reply`]); the `Stdout` frame carries raw guest output so
//! binary console data passes through untouched. One request per connection;
//! a `RunOneShot` then streams `Stdout` frames followed by a terminal `Reply`.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::PathBuf;

pub const TAG_REQUEST: u8 = 1;
pub const TAG_REPLY: u8 = 2;
pub const TAG_STDOUT: u8 = 3;
pub const TAG_STDIN: u8 = 4;

/// Reject frames larger than this. Control frames are small and stdout chunks are
/// 8 KiB; the cap stops a malformed length from forcing a huge allocation.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Default control-socket path. macOS rarely sets `XDG_RUNTIME_DIR`, so fall back
/// to a per-uid subdirectory of the temp dir (created 0700 by the daemon) so the
/// socket is not world-accessible.
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("amber.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir().join(format!("amber-{uid}")).join("amber.sock")
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op")]
pub enum Request {
    /// Cast a VM, run argv, stream I/O, discard.
    RunOneShot { reference: String, argv: Vec<String> },
    /// Cast a VM that keeps running in the background; output goes to its log.
    RunDetached { reference: String, argv: Vec<String> },
    /// Stream a VM's captured log.
    Logs { id: String },
    /// List live VMs.
    List,
    /// Kill a VM by id.
    Kill { id: String },
    /// Stop the daemon.
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind")]
pub enum Reply {
    /// Terminal reply for RunOneShot: the guest process exit code.
    Exit { code: i32 },
    /// A detached VM was started, with its id.
    Started { id: String },
    /// The live VM list.
    Vms { vms: Vec<VmInfo> },
    /// Generic acknowledgement.
    Ok,
    /// A structured error.
    Error { message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VmInfo {
    pub id: String,
    pub reference: String,
    pub pid: u32,
    /// Unix epoch seconds when the VM was started, for `ps` age.
    pub started: u64,
}

/// Current Unix time in whole seconds.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&[tag])?;
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one frame. Returns None on a clean EOF at a frame boundary.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut tag = [0u8; 1];
    match r.read_exact(&mut tag) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((tag[0], payload)))
}

pub fn write_request(w: &mut impl Write, req: &Request) -> io::Result<()> {
    let json = serde_json::to_vec(req)?;
    write_frame(w, TAG_REQUEST, &json)
}

pub fn write_reply(w: &mut impl Write, reply: &Reply) -> io::Result<()> {
    let json = serde_json::to_vec(reply)?;
    write_frame(w, TAG_REPLY, &json)
}
