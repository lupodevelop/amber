//! virtio-vsock: a guest↔host stream channel, bridged to host Unix sockets.
//!
//! macOS has no AF_VSOCK, so this is the userspace "hybrid" model Firecracker
//! uses: a guest AF_VSOCK stream maps to a host Unix-domain socket. Guest dials
//! port `p` → amber connects `<uds>_<p>`; a host peer reaches the guest by
//! connecting `<uds>` and sending `CONNECT <p>\n`. The device speaks the
//! virtio-vsock packet protocol over the tx/rx virtqueues with proper credit
//! flow control; everything here is single-threaded on the vcpu/run-loop side
//! (the listener runs on its own thread and hands sockets over a channel).

use crate::memory::GuestRam;
use crate::virtio::{Buf, VirtioDevice};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::Receiver;

const HDR_LEN: usize = 44;
const TYPE_STREAM: u16 = 1;
const CID_HOST: u64 = 2;
/// Bottom of the host ephemeral port range for host→guest connections.
const EPHEMERAL_BASE: u32 = 1024;

// Operations.
const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

/// Our advertised receive window per connection. The guest may have this many
/// bytes in flight to us before it must wait for a credit update.
const BUF_ALLOC: u32 = 256 * 1024;
/// Largest guest→host packet we reassemble. A well-behaved packet carries at most
/// our advertised window of payload; a chain claiming more (e.g. 256 × 16 MiB
/// descriptors) is a hostile attempt to make the host allocate gigabytes, so we
/// drop it rather than reassemble.
const MAX_PACKET: usize = HDR_LEN + BUF_ALLOC as usize;
/// A nominal RW payload cap used by tests. At run time the payload is instead
/// sized to the guest's actual posted rx buffer (see `poll_rx`/`pump`), which on
/// Linux is under a page once skb overhead is accounted for.
#[cfg(test)]
const MAX_RW: usize = 4096 - HDR_LEN;

/// The host side of vsock: how a guest port maps to a Unix socket, and how host
/// peers reach the guest. Default impl is the Firecracker hybrid UDS scheme.
pub trait VsockBackend: Send {
    /// Guest dials host `port`: a connected stream, or None to refuse (RST).
    fn dial(&mut self, port: u32) -> Option<UnixStream>;
    /// A host peer that wants the guest port it asked for, non-blocking.
    fn accept(&mut self) -> Option<(u32, UnixStream)>;
}

/// Firecracker-hybrid backend: one base UDS path. `dial` connects `<base>_<port>`;
/// host peers connect `<base>` and send `CONNECT <port>\n`.
pub struct UdsBackend {
    base: std::path::PathBuf,
    incoming: Receiver<(u32, UnixStream)>,
}

impl UdsBackend {
    /// Bind the listener at `base` and spawn its accept thread. Returns None if
    /// the path can't be bound.
    pub fn new(base: impl Into<std::path::PathBuf>) -> Option<Self> {
        let base = base.into();
        let _ = std::fs::remove_file(&base);
        let listener = UnixListener::bind(&base).ok()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                // Read the "CONNECT <port>\n" line the peer sends first.
                let mut s = stream;
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    match s.read(&mut byte) {
                        Ok(1) if byte[0] == b'\n' => break,
                        Ok(1) if line.len() < 32 => line.push(byte[0]),
                        _ => break,
                    }
                }
                let port = std::str::from_utf8(&line)
                    .ok()
                    .and_then(|l| l.strip_prefix("CONNECT "))
                    .and_then(|p| p.trim().parse::<u32>().ok());
                if let Some(p) = port {
                    if tx.send((p, s)).is_err() {
                        break;
                    }
                }
            }
        });
        log::info!("vsock: host socket at {}", base.display());
        Some(Self { base, incoming: rx })
    }
}

impl Drop for UdsBackend {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.base);
    }
}

impl VsockBackend for UdsBackend {
    fn dial(&mut self, port: u32) -> Option<UnixStream> {
        let mut p = self.base.clone().into_os_string();
        p.push(format!("_{port}"));
        UnixStream::connect(p).ok()
    }
    fn accept(&mut self) -> Option<(u32, UnixStream)> {
        self.incoming.try_recv().ok()
    }
}

/// One live stream connection (either direction). Ports are from the guest's
/// view: `guest_port` is the guest local port, `host_port` the peer port.
struct Conn {
    guest_port: u32,
    host_port: u32,
    stream: UnixStream,
    /// Guest's advertised window, from its headers.
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Bytes of RW payload we've sent to the guest.
    tx_cnt: u32,
    /// Bytes of guest payload we've drained to the host (our advertised fwd_cnt).
    fwd_cnt: u32,
    /// Guest payload received but not yet written to the host (backpressure).
    to_host: VecDeque<u8>,
    /// True once the connection is established (RESPONSE exchanged).
    up: bool,
    /// Peer (host) closed its side; flush `to_host` then drop.
    host_done: bool,
}

impl Conn {
    /// Bytes we may still send to the guest under its credit.
    fn guest_window(&self) -> u32 {
        self.peer_buf_alloc.saturating_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt))
    }
}

pub struct VsockDevice {
    backend: Box<dyn VsockBackend>,
    guest_cid: u64,
    conns: Vec<Conn>,
    /// Packets queued for the guest rx queue (each already header+payload).
    rx: VecDeque<Vec<u8>>,
    /// Next ephemeral host port for a host→guest connection. Monotonic so a
    /// just-closed connection's port is not reused while the guest still has it
    /// in teardown — reuse made every other sequential connect to a fork fail.
    next_host_port: u32,
}

impl VsockDevice {
    const DEVICE_ID: u32 = 19;
    const RX: usize = 0;
    const TX: usize = 1;
    const EVENT: usize = 2;

    pub fn new(guest_cid: u64, backend: Box<dyn VsockBackend>) -> Self {
        Self {
            backend,
            guest_cid,
            conns: Vec::new(),
            rx: VecDeque::new(),
            next_host_port: EPHEMERAL_BASE,
        }
    }

    /// A host source port not currently in use for `guest_port`, cycling through
    /// the ephemeral range instead of restarting at the bottom each time.
    fn alloc_host_port(&mut self, guest_port: u32) -> u32 {
        for _ in 0..=(u32::MAX - EPHEMERAL_BASE) {
            let hp = self.next_host_port;
            self.next_host_port = hp.checked_add(1).unwrap_or(EPHEMERAL_BASE);
            if self.find(guest_port, hp).is_none() {
                return hp;
            }
        }
        EPHEMERAL_BASE
    }

    /// Queue a control/data packet for the guest. `fwd_cnt`/`buf_alloc` ride along
    /// so the guest's flow control stays current.
    fn send(&mut self, guest_port: u32, host_port: u32, op: u16, fwd_cnt: u32, payload: &[u8]) {
        let mut p = vec![0u8; HDR_LEN + payload.len()];
        p[0..8].copy_from_slice(&CID_HOST.to_le_bytes());
        p[8..16].copy_from_slice(&self.guest_cid.to_le_bytes());
        p[16..20].copy_from_slice(&host_port.to_le_bytes());
        p[20..24].copy_from_slice(&guest_port.to_le_bytes());
        p[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        p[28..30].copy_from_slice(&TYPE_STREAM.to_le_bytes());
        p[30..32].copy_from_slice(&op.to_le_bytes());
        // flags = 0
        p[36..40].copy_from_slice(&BUF_ALLOC.to_le_bytes());
        p[40..44].copy_from_slice(&fwd_cnt.to_le_bytes());
        p[HDR_LEN..].copy_from_slice(payload);
        self.rx.push_back(p);
    }

    fn find(&mut self, guest_port: u32, host_port: u32) -> Option<usize> {
        self.conns.iter().position(|c| c.guest_port == guest_port && c.host_port == host_port)
    }

    /// Process one guest→host packet (already reassembled from the tx chain).
    fn on_guest_packet(&mut self, pkt: &[u8]) {
        if pkt.len() < HDR_LEN {
            return;
        }
        let dst_cid = u64::from_le_bytes(pkt[8..16].try_into().unwrap());
        let src_port = u32::from_le_bytes(pkt[16..20].try_into().unwrap()); // guest port
        let dst_port = u32::from_le_bytes(pkt[20..24].try_into().unwrap()); // host port
        let len = u32::from_le_bytes(pkt[24..28].try_into().unwrap()) as usize;
        let op = u16::from_le_bytes(pkt[30..32].try_into().unwrap());
        let buf_alloc = u32::from_le_bytes(pkt[36..40].try_into().unwrap());
        let fwd_cnt = u32::from_le_bytes(pkt[40..44].try_into().unwrap());
        let payload = &pkt[HDR_LEN..HDR_LEN + len.min(pkt.len() - HDR_LEN)];

        if dst_cid != CID_HOST {
            return; // not addressed to us
        }
        let (gp, hp) = (src_port, dst_port);

        // Refresh the peer's window on any packet.
        if let Some(i) = self.find(gp, hp) {
            self.conns[i].peer_buf_alloc = buf_alloc;
            self.conns[i].peer_fwd_cnt = fwd_cnt;
        }

        match op {
            OP_REQUEST => {
                // Guest dials host port `hp`.
                log::debug!("vsock: guest dials host port {hp} (guest port {gp})");
                match self.backend.dial(hp) {
                    Some(stream) => {
                        let _ = stream.set_nonblocking(true);
                        self.conns.push(Conn {
                            guest_port: gp,
                            host_port: hp,
                            stream,
                            peer_buf_alloc: buf_alloc,
                            peer_fwd_cnt: fwd_cnt,
                            tx_cnt: 0,
                            fwd_cnt: 0,
                            to_host: VecDeque::new(),
                            up: true,
                            host_done: false,
                        });
                        self.send(gp, hp, OP_RESPONSE, 0, &[]);
                    }
                    None => self.send(gp, hp, OP_RST, 0, &[]),
                }
            }
            OP_RESPONSE => {
                if let Some(i) = self.find(gp, hp) {
                    self.conns[i].up = true; // guest accepted a host-initiated conn
                }
            }
            OP_RW => {
                if let Some(i) = self.find(gp, hp) {
                    // The guest agreed to keep at most BUF_ALLOC bytes in flight to
                    // us (our advertised window). If it floods past that, the host
                    // UDS can't drain fast enough and `to_host` grows unbounded, so
                    // reset the connection rather than buffer without limit.
                    if self.conns[i].to_host.len() + payload.len() > BUF_ALLOC as usize {
                        self.conns[i].up = false;
                        self.conns[i].host_done = true;
                        self.send(gp, hp, OP_RST, 0, &[]);
                    } else {
                        self.conns[i].to_host.extend(payload);
                    }
                }
            }
            OP_CREDIT_REQUEST => {
                let fwd = self.find(gp, hp).map(|i| self.conns[i].fwd_cnt).unwrap_or(0);
                self.send(gp, hp, OP_CREDIT_UPDATE, fwd, &[]);
            }
            OP_CREDIT_UPDATE => {} // window already refreshed above
            OP_SHUTDOWN | OP_RST => {
                if let Some(i) = self.find(gp, hp) {
                    // Drop once any queued host writes are flushed.
                    self.conns[i].host_done = true;
                    self.conns[i].up = false;
                }
            }
            _ => {}
        }
    }

    /// Host-side work: accept new host→guest connections, move bytes both ways
    /// under credit, and reap dead connections. Fills `self.rx`. `max_payload` caps
    /// each RW packet's payload so a packet fits the guest's posted rx buffer.
    fn pump(&mut self, max_payload: usize) {
        // New host→guest connections: tell the guest with a REQUEST.
        while let Some((guest_port, stream)) = self.backend.accept() {
            let _ = stream.set_nonblocking(true);
            // A fresh ephemeral host port (never an immediate reuse).
            let host_port = self.alloc_host_port(guest_port);
            self.conns.push(Conn {
                guest_port,
                host_port,
                stream,
                peer_buf_alloc: 0,
                peer_fwd_cnt: 0,
                tx_cnt: 0,
                fwd_cnt: 0,
                to_host: VecDeque::new(),
                up: false,
                host_done: false,
            });
            self.send(guest_port, host_port, OP_REQUEST, 0, &[]);
        }

        for idx in 0..self.conns.len() {
            // Drain guest→host bytes to the stream; advance fwd_cnt by what lands.
            let mut credit_changed = false;
            loop {
                let front = {
                    let c = &self.conns[idx];
                    if c.to_host.is_empty() {
                        break;
                    }
                    let (a, _) = c.to_host.as_slices();
                    a.to_vec()
                };
                match self.conns[idx].stream.write(&front) {
                    Ok(0) => break,
                    Ok(n) => {
                        let c = &mut self.conns[idx];
                        c.to_host.drain(..n);
                        c.fwd_cnt = c.fwd_cnt.wrapping_add(n as u32);
                        credit_changed = true;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        self.conns[idx].host_done = true;
                        break;
                    }
                }
            }
            if credit_changed && self.conns[idx].up {
                let (gp, hp, fwd) =
                    (self.conns[idx].guest_port, self.conns[idx].host_port, self.conns[idx].fwd_cnt);
                self.send(gp, hp, OP_CREDIT_UPDATE, fwd, &[]);
            }

            // Host→guest bytes, bounded by the guest's credit window and the rx
            // buffer it posted (max_payload).
            if self.conns[idx].up {
                loop {
                    let window = self.conns[idx].guest_window() as usize;
                    let take = window.min(max_payload);
                    if take == 0 {
                        break;
                    }
                    let mut buf = vec![0u8; take];
                    match self.conns[idx].stream.read(&mut buf) {
                        Ok(0) => {
                            self.conns[idx].host_done = true;
                            break;
                        }
                        Ok(n) => {
                            let (gp, hp, fwd) = (
                                self.conns[idx].guest_port,
                                self.conns[idx].host_port,
                                self.conns[idx].fwd_cnt,
                            );
                            self.send(gp, hp, OP_RW, fwd, &buf[..n]);
                            self.conns[idx].tx_cnt = self.conns[idx].tx_cnt.wrapping_add(n as u32);
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            self.conns[idx].host_done = true;
                            break;
                        }
                    }
                }
            }
        }

        // Reap connections whose host side is done and have nothing left to flush:
        // tell the guest, then drop.
        let mut shutdowns = Vec::new();
        self.conns.retain(|c| {
            let dead = c.host_done && c.to_host.is_empty();
            if dead {
                shutdowns.push((c.guest_port, c.host_port, c.fwd_cnt));
            }
            !dead
        });
        for (gp, hp, fwd) in shutdowns {
            self.send(gp, hp, OP_SHUTDOWN, fwd, &[]);
        }
    }
}

/// Drive the guest→host vsock packet parser with arbitrary bytes (fuzz entry).
/// The guest controls this byte string in production, so it must never panic,
/// over-read, or loop on any input. The backend refuses all dials so no real
/// sockets are touched.
#[cfg(feature = "fuzzing")]
pub fn fuzz_on_guest_packet(data: &[u8]) {
    struct NullBackend;
    impl VsockBackend for NullBackend {
        fn dial(&mut self, _: u32) -> Option<UnixStream> {
            None
        }
        fn accept(&mut self) -> Option<(u32, UnixStream)> {
            None
        }
    }
    let mut dev = VsockDevice::new(3, Box::new(NullBackend));
    dev.on_guest_packet(data);
    // Drive the downstream state the parsed packet may have mutated.
    let _ = dev.poll_rx(4096);
}

impl VirtioDevice for VsockDevice {
    fn device_id(&self) -> u32 {
        Self::DEVICE_ID
    }
    fn num_queues(&self) -> usize {
        3 // rx, tx, event
    }
    fn config(&self, off: u64) -> u32 {
        // Config space: guest_cid (u64) at offset 0.
        match off {
            0 => self.guest_cid as u32,
            4 => (self.guest_cid >> 32) as u32,
            _ => 0,
        }
    }
    fn rx_queue(&self) -> Option<usize> {
        Some(Self::RX)
    }
    fn event_queue(&self) -> Option<usize> {
        Some(Self::EVENT)
    }
    fn wants_poll(&self) -> bool {
        // Always: the run loop must keep pumping so an inbound host connection
        // (accepted on the listener thread) reaches the guest within a poll tick,
        // not only after the guest happens to exit for something else.
        true
    }

    /// Guest→host: queue 1 carries a virtio-vsock packet across its readable
    /// descriptors. Reassemble and process it. The event queue (2) is ignored.
    fn handle(&mut self, queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32 {
        if queue == Self::TX {
            // Bound the host allocation: sum the readable lengths first and drop a
            // packet that claims more than our window (guest is misbehaving).
            let total: usize = bufs.iter().filter(|b| !b.writable).map(|b| b.len as usize).sum();
            if total > MAX_PACKET {
                log::debug!("vsock: dropping oversized tx packet ({total} bytes)");
                return 0;
            }
            let mut pkt = Vec::with_capacity(total);
            for b in bufs.iter().filter(|b| !b.writable) {
                let mut d = vec![0u8; b.len as usize];
                ram.read(b.addr, &mut d);
                pkt.extend_from_slice(&d);
            }
            self.on_guest_packet(&pkt);
        }
        0
    }

    /// Next packet for the guest rx queue: pump the host side, then dequeue. The
    /// guest's posted rx buffer is `max_frame` bytes, so a data packet's payload is
    /// capped to that minus the header — an oversized packet would be truncated by
    /// the virtio layer and dropped by the guest.
    fn poll_rx(&mut self, max_frame: usize) -> Option<Vec<u8>> {
        if self.rx.is_empty() {
            self.pump(max_frame.saturating_sub(HDR_LEN));
        }
        self.rx.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend whose `dial` returns one end of a socketpair and hands the other
    /// to the test, and whose `accept` is fed by the test.
    struct TestBackend {
        dialed: Option<UnixStream>,
        incoming: VecDeque<(u32, UnixStream)>,
    }
    impl VsockBackend for TestBackend {
        fn dial(&mut self, _port: u32) -> Option<UnixStream> {
            self.dialed.take()
        }
        fn accept(&mut self) -> Option<(u32, UnixStream)> {
            self.incoming.pop_front()
        }
    }

    fn hdr(pkt: &[u8]) -> (u32, u32, u16, usize) {
        let gp = u32::from_le_bytes(pkt[20..24].try_into().unwrap());
        let hp = u32::from_le_bytes(pkt[16..20].try_into().unwrap());
        let op = u16::from_le_bytes(pkt[30..32].try_into().unwrap());
        let len = u32::from_le_bytes(pkt[24..28].try_into().unwrap()) as usize;
        (gp, hp, op, len)
    }

    fn guest_pkt(gp: u32, hp: u32, op: u16, fwd: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; HDR_LEN + payload.len()];
        p[8..16].copy_from_slice(&CID_HOST.to_le_bytes()); // dst = host
        p[16..20].copy_from_slice(&gp.to_le_bytes());      // src_port = guest
        p[20..24].copy_from_slice(&hp.to_le_bytes());      // dst_port = host
        p[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        p[28..30].copy_from_slice(&TYPE_STREAM.to_le_bytes());
        p[30..32].copy_from_slice(&op.to_le_bytes());
        p[36..40].copy_from_slice(&BUF_ALLOC.to_le_bytes());
        p[40..44].copy_from_slice(&fwd.to_le_bytes());
        p[HDR_LEN..].copy_from_slice(payload);
        p
    }

    #[test]
    fn guest_to_host_connect_and_stream() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let _ = a.set_nonblocking(true);
        let mut dev =
            VsockDevice::new(3, Box::new(TestBackend { dialed: Some(a), incoming: VecDeque::new() }));

        // Guest dials host port 1024.
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_REQUEST, 0, b"")); // REQUEST
        let resp = dev.rx.pop_front().expect("a RESPONSE");
        assert_eq!(hdr(&resp).2, OP_RESPONSE);

        // Guest sends data; it must reach the host stream.
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_RW, 0, b"ping"));
        dev.pump(MAX_RW);
        let mut got = [0u8; 4];
        b.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");

        // Host replies; it must come back as an RW packet to the guest.
        b.write_all(b"pong").unwrap();
        dev.pump(MAX_RW);
        let rw = dev.rx.iter().find(|p| hdr(p).2 == OP_RW).expect("an RW back");
        assert_eq!(&rw[HDR_LEN..], b"pong");
    }

    #[test]
    fn guest_exceeding_window_is_reset() {
        // M4: a guest that floods past its advertised window (host never drains)
        // must be RST, and buffered bytes must never exceed the window.
        let (a, _b) = UnixStream::pair().unwrap();
        let _ = a.set_nonblocking(true);
        let mut dev =
            VsockDevice::new(3, Box::new(TestBackend { dialed: Some(a), incoming: VecDeque::new() }));
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_REQUEST, 0, b""));
        dev.rx.clear();

        // No pump between sends: the host side never advances fwd_cnt.
        let full = vec![0u8; BUF_ALLOC as usize];
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_RW, 0, &full)); // fills window
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_RW, 0, b"x")); // one over -> RST

        assert!(dev.rx.iter().any(|p| hdr(p).2 == OP_RST), "flooding past the window must RST");
        let i = dev.find(40000, 1024).unwrap();
        assert!(
            dev.conns[i].to_host.len() <= BUF_ALLOC as usize,
            "buffered bytes exceeded the advertised window",
        );
    }

    #[test]
    fn oversized_tx_packet_is_dropped() {
        // M4: a tx descriptor chain claiming more than MAX_PACKET bytes must be
        // dropped without reassembly, so a hostile chain can't OOM the host.
        let mem = crate::memory::GuestMemory::new(0, MAX_PACKET + 0x2000).unwrap();
        let ram = mem.ram();
        let (a, _b) = UnixStream::pair().unwrap();
        let _ = a.set_nonblocking(true);
        let mut dev =
            VsockDevice::new(3, Box::new(TestBackend { dialed: Some(a), incoming: VecDeque::new() }));
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_REQUEST, 0, b""));
        dev.rx.clear();

        // A small valid RW header in buf0, plus a huge in-range filler buf that
        // pushes the chain total past MAX_PACKET.
        let rw = guest_pkt(40000, 1024, OP_RW, 0, b"ping");
        ram.write(0, &rw);
        let bufs = vec![
            Buf { addr: 0, len: rw.len() as u32, writable: false },
            Buf { addr: 0x1000, len: MAX_PACKET as u32, writable: false },
        ];
        dev.handle(VsockDevice::TX, &ram, &bufs);

        let i = dev.find(40000, 1024).unwrap();
        assert!(dev.conns[i].to_host.is_empty(), "oversized packet was not dropped");

        // A normal-sized packet still delivers.
        let bufs = vec![Buf { addr: 0, len: rw.len() as u32, writable: false }];
        dev.handle(VsockDevice::TX, &ram, &bufs);
        assert_eq!(dev.conns[i].to_host.len(), 4, "normal packet should deliver");
    }

    #[test]
    fn refused_dial_resets() {
        let mut dev =
            VsockDevice::new(3, Box::new(TestBackend { dialed: None, incoming: VecDeque::new() }));
        dev.on_guest_packet(&guest_pkt(40000, 9999, OP_REQUEST, 0, b""));
        assert_eq!(hdr(&dev.rx.pop_front().unwrap()).2, OP_RST);
    }

    #[test]
    fn host_ports_are_not_immediately_reused() {
        let mut dev =
            VsockDevice::new(3, Box::new(TestBackend { dialed: None, incoming: VecDeque::new() }));
        // Sequential connections to the same guest port get distinct, advancing
        // host ports — never a fresh 1024 that collides with one still tearing
        // down (that was the every-other-connect failure to a fork).
        let a = dev.alloc_host_port(40000);
        let b = dev.alloc_host_port(40000);
        let c = dev.alloc_host_port(40000);
        assert_eq!(a, EPHEMERAL_BASE);
        assert_eq!(b, EPHEMERAL_BASE + 1);
        assert_eq!(c, EPHEMERAL_BASE + 2);
    }

    #[test]
    fn host_to_guest_requests_then_streams() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let _ = a.set_nonblocking(true);
        let mut inc = VecDeque::new();
        inc.push_back((5000u32, a)); // host peer wants guest port 5000
        let mut dev = VsockDevice::new(3, Box::new(TestBackend { dialed: None, incoming: inc }));

        dev.pump(MAX_RW); // emits a REQUEST to the guest
        let (gp, hp, op, _) = hdr(&dev.rx.pop_front().unwrap());
        assert_eq!((gp, op), (5000, OP_REQUEST));

        // Guest accepts; now host data flows to the guest.
        dev.on_guest_packet(&guest_pkt(gp, hp, OP_RESPONSE, 0, b""));
        b.write_all(b"hello").unwrap();
        dev.pump(MAX_RW);
        let rw = dev.rx.iter().find(|p| hdr(p).2 == OP_RW).expect("RW to guest");
        assert_eq!(&rw[HDR_LEN..], b"hello");
    }

    #[test]
    fn credit_blocks_host_to_guest_until_window() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let _ = a.set_nonblocking(true);
        let mut dev =
            VsockDevice::new(3, Box::new(TestBackend { dialed: Some(a), incoming: VecDeque::new() }));
        // Guest connects but advertises a tiny 3-byte window.
        let mut req = guest_pkt(40000, 1024, OP_REQUEST, 0, b"");
        req[36..40].copy_from_slice(&3u32.to_le_bytes()); // buf_alloc = 3
        dev.on_guest_packet(&req);
        dev.rx.clear();

        b.write_all(b"abcdef").unwrap();
        dev.pump(MAX_RW);
        // Only 3 bytes may go (window=3); no more until the guest raises fwd_cnt.
        let sent: usize = dev.rx.iter().filter(|p| hdr(p).2 == OP_RW).map(|p| p.len() - HDR_LEN).sum();
        assert_eq!(sent, 3, "host→guest is capped at the guest's credit");

        // Guest consumed them (fwd_cnt=3): the window reopens.
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_CREDIT_UPDATE, 3, b""));
        dev.pump(MAX_RW);
        let sent: usize = dev.rx.iter().filter(|p| hdr(p).2 == OP_RW).map(|p| p.len() - HDR_LEN).sum();
        assert_eq!(sent, 6, "the rest flows after credit");
    }

    #[test]
    fn rw_packets_fit_the_posted_rx_buffer() {
        // poll_rx must size each data packet to the guest's posted rx buffer: a
        // packet larger than the buffer is truncated by the virtio layer and the
        // guest drops it as malformed (real guests post sub-page, ~3776-byte, bufs).
        let (a, mut b) = UnixStream::pair().unwrap();
        let _ = a.set_nonblocking(true);
        let mut dev =
            VsockDevice::new(3, Box::new(TestBackend { dialed: Some(a), incoming: VecDeque::new() }));
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_REQUEST, 0, b"")); // 256 KiB window
        dev.rx.clear();

        // Write on a thread so it can block on the socket buffer while we drain it.
        let total = 20_000usize;
        let writer = std::thread::spawn(move || {
            b.write_all(&vec![0xab; total]).unwrap();
        });

        // The guest posts buffers of 1000 bytes; no packet may exceed that.
        let frame = 1000usize;
        let mut data = 0usize;
        // Poll until we've drained the whole blob (poll_rx pumps the socket each
        // time rx empties); a brief spin lets the writer make progress.
        for _ in 0..100_000 {
            match dev.poll_rx(frame) {
                Some(p) => {
                    assert!(p.len() <= frame, "packet {} exceeds posted buffer {frame}", p.len());
                    if hdr(&p).2 == OP_RW {
                        data += p.len() - HDR_LEN;
                    }
                }
                None if data >= total => break,
                None => std::thread::yield_now(),
            }
        }
        writer.join().unwrap();
        assert_eq!(data, total, "every byte must arrive, split across capped packets");
    }

    // ---- malformed guest input (the guest→host attack surface) ----
    // These pin the invariants the fuzzer explores: no panic, no state corruption,
    // junk packets are dropped. `on_guest_packet` parses fully guest-controlled bytes.

    fn null_dev() -> VsockDevice {
        VsockDevice::new(3, Box::new(TestBackend { dialed: None, incoming: VecDeque::new() }))
    }

    #[test]
    fn short_packets_are_ignored_not_panicked() {
        let mut dev = null_dev();
        for n in 0..HDR_LEN {
            dev.on_guest_packet(&vec![0xff; n]); // shorter than a header
        }
        assert!(dev.conns.is_empty());
        assert!(dev.rx.is_empty());
    }

    #[test]
    fn unknown_opcode_is_dropped() {
        let mut dev = null_dev();
        dev.on_guest_packet(&guest_pkt(40000, 1024, 0xbeef, 0, b"junk"));
        dev.on_guest_packet(&guest_pkt(40000, 1024, 0, 0, b""));
        assert!(dev.conns.is_empty());
        assert!(dev.rx.is_empty());
    }

    #[test]
    fn data_for_unknown_connection_is_dropped() {
        let mut dev = null_dev();
        // RW / SHUTDOWN / CREDIT for a connection that was never opened.
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_RW, 0, b"payload"));
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_SHUTDOWN, 0, b""));
        dev.on_guest_packet(&guest_pkt(40000, 1024, OP_CREDIT_UPDATE, 999, b""));
        assert!(dev.conns.is_empty(), "no phantom connection from stray data");
    }

    #[test]
    fn oversized_len_field_does_not_over_read() {
        // Header claims a 4 GiB payload but carries 4 bytes: the parser must clamp
        // to what's present, never index past the packet.
        let mut p = guest_pkt(40000, 1024, OP_RW, 0, b"abcd");
        p[24..28].copy_from_slice(&u32::MAX.to_le_bytes()); // len = 0xffff_ffff
        let mut dev = null_dev();
        dev.on_guest_packet(&p); // must not panic / OOB
        assert!(dev.conns.is_empty());
    }

    #[test]
    fn packet_for_other_cid_is_ignored() {
        // dst_cid must be the host (2); anything else is not ours.
        let mut p = guest_pkt(40000, 1024, OP_REQUEST, 0, b"");
        p[8..16].copy_from_slice(&999u64.to_le_bytes()); // dst_cid = 999, not host
        let mut dev =
            VsockDevice::new(3, Box::new(TestBackend { dialed: None, incoming: VecDeque::new() }));
        dev.on_guest_packet(&p);
        assert!(dev.conns.is_empty());
        assert!(dev.rx.is_empty(), "no RST/RESPONSE for a packet not addressed to us");
    }

    #[test]
    fn refused_request_then_stray_data_stays_clean() {
        // A dial we refuse (RST), followed by data on the never-established conn.
        let mut dev = null_dev();
        dev.on_guest_packet(&guest_pkt(40000, 9999, OP_REQUEST, 0, b"")); // -> RST
        assert_eq!(hdr(&dev.rx.pop_front().unwrap()).2, OP_RST);
        dev.on_guest_packet(&guest_pkt(40000, 9999, OP_RW, 0, b"after-rst"));
        assert!(dev.conns.is_empty());
        assert!(dev.rx.is_empty());
    }
}
