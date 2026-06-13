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
/// Guest stderr, kept distinct from stdout (the vsock exec path carries both).
pub const TAG_STDERR: u8 = 5;

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
    /// Report the fleet RAM budget and usage.
    Budget,
    /// Ask a VM's balloon to reclaim toward `mib` MiB (active reclaim).
    Balloon { id: String, mib: u64 },
    /// Freeze a running VM in place (vcpu parks; RAM/registers intact).
    Pause { id: String },
    /// Resume a paused VM.
    Resume { id: String },
    /// Fork a VM from a warm template (a snapshot directory): hand off a
    /// pre-staged paused worker if the pool has one, else stage one now.
    /// `interactive` streams the resumed guest's console I/O to the client (like
    /// `RunOneShot`); otherwise the fork is detached and only its id is returned.
    Fork { template: String, interactive: bool },
    /// Run one command in a fork of a template over the vsock exec channel: the
    /// in-guest agent runs it under `sh -c`, and its stdout/stderr/exit-code come
    /// back as distinct frames (no in-band marker). Non-interactive, one-shot.
    Exec { template: String, cmd: String },
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
    /// The fleet RAM budget and usage, in bytes (`budget` 0 = unlimited). `used`
    /// is the cap-based reservation (what admission counts); `rss` is the real
    /// resident footprint of the live VMs; `machine` is total host RAM, so the
    /// host headroom outside the fleet ceiling is visible.
    Budget { budget: u64, used: u64, rss: u64, machine: u64 },
    /// Admission refused to protect the budget. All values in bytes.
    BudgetExceeded { budget: u64, used: u64, requested: u64 },
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
    /// RAM this VM is accounted at against the fleet budget, in bytes.
    pub ram_bytes: u64,
    /// Real resident memory of the VM process, sampled when listed (0 otherwise).
    pub rss_bytes: u64,
    /// Whether the VM is currently paused (frozen via `pause`).
    #[serde(default)]
    pub paused: bool,
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
