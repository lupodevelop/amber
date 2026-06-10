//! Host-side network backends behind amber-core's `NetBackend` seam.
//!
//! Each strategy is feature-gated so its dependencies stay opt-in. The default is
//! a userspace netstack (smoltcp): amber is the gateway (10.0.0.1) and the guest a
//! static 10.0.0.2; smoltcp answers ARP and ICMP for the gateway and proxies the
//! guest's TCP to host sockets via destination NAT. gvproxy/vmnet/tap slot in
//! behind the same seam.

use amber_core::NetBackend;

/// Build the named backend, or None if unknown / its feature is off.
pub fn backend(kind: &str) -> Option<Box<dyn NetBackend>> {
    match kind {
        #[cfg(feature = "smoltcp-backend")]
        "smoltcp" => Some(Box::new(smoltcp_backend::SmoltcpBackend::new())),
        _ => None,
    }
}

#[cfg(feature = "smoltcp-backend")]
mod smoltcp_backend {
    use amber_core::NetBackend;
    use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
    use smoltcp::phy::{Device, DeviceCapabilities, Medium};
    use smoltcp::socket::{tcp, udp};
    use smoltcp::time::Instant;
    use smoltcp::wire::{
        EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpAddress, IpCidr,
        IpEndpoint, IpListenEndpoint, IpProtocol, Ipv4Address, Ipv4Packet, TcpPacket,
    };
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Where the gateway forwards the guest's DNS queries.
    const RESOLVER: &str = "1.1.1.1:53";

    const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 0, 1);
    const GUEST: Ipv4Address = Ipv4Address::new(10, 0, 0, 2);
    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const MTU: usize = 1500;
    /// Ceilings on guest-driven resource growth. Each flow holds 128 KiB of smoltcp
    /// buffers; each in-flight DNS query holds a host UDP socket (a file descriptor).
    /// A hostile guest could otherwise open these without bound, so new ones past the
    /// cap are dropped (the guest sees the connection/query fail, the host stays up).
    const MAX_FLOWS: usize = 256;
    const MAX_PENDING_DNS: usize = 256;

    type Frames = Arc<Mutex<VecDeque<Vec<u8>>>>;

    struct Bridge {
        rx: Frames,
        tx: Frames,
    }
    struct RxTok(Vec<u8>);
    struct TxTok(Frames);

    impl Device for Bridge {
        type RxToken<'a> = RxTok;
        type TxToken<'a> = TxTok;
        fn receive(&mut self, _t: Instant) -> Option<(RxTok, TxTok)> {
            let frame = self.rx.lock().unwrap().pop_front()?;
            Some((RxTok(frame), TxTok(self.tx.clone())))
        }
        fn transmit(&mut self, _t: Instant) -> Option<TxTok> {
            Some(TxTok(self.tx.clone()))
        }
        fn capabilities(&self) -> DeviceCapabilities {
            let mut c = DeviceCapabilities::default();
            c.medium = Medium::Ethernet;
            c.max_transmission_unit = MTU;
            c
        }
    }
    impl smoltcp::phy::RxToken for RxTok {
        fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
            f(&self.0)
        }
    }
    impl smoltcp::phy::TxToken for TxTok {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            let mut buf = vec![0u8; len];
            let r = f(&mut buf);
            self.0.lock().unwrap().push_back(buf);
            r
        }
    }

    /// One proxied TCP connection. The guest connects to `ext_ip:ext_port`; we NAT
    /// the destination to the gateway's own IP and a unique `eph` port so smoltcp
    /// accepts it on a socket it owns, and bridge that socket to the host stream.
    struct Flow {
        guest_port: u16,
        ext_ip: Ipv4Addr,
        ext_port: u16,
        eph: u16,
        handle: SocketHandle,
        /// The host connection, once it completes. `connecting` carries it from the
        /// connect thread so the vcpu never blocks on a slow or dead destination.
        host: Option<TcpStream>,
        connecting: Option<std::sync::mpsc::Receiver<std::io::Result<TcpStream>>>,
        host_done: bool,
    }

    /// A DNS query the gateway forwarded to the host resolver, awaiting its reply
    /// to relay back to the guest endpoint that asked.
    struct DnsQuery {
        host: UdpSocket,
        guest: IpEndpoint,
    }

    pub struct SmoltcpBackend {
        iface: Interface,
        device: Bridge,
        sockets: SocketSet<'static>,
        flows: Vec<Flow>,
        dns: SocketHandle,
        pending_dns: Vec<DnsQuery>,
        /// Inbound (host->guest) port-forwards: host listeners hand accepted
        /// connections here; the netstack dials the guest and we bridge them.
        accepted: Arc<Mutex<VecDeque<(TcpStream, u16)>>>,
        inbound: Vec<(SocketHandle, TcpStream)>,
        has_listeners: bool,
        next_eph: u16,
        start: std::time::Instant,
    }

    /// Bridge bytes between a smoltcp socket and its host stream (shared by
    /// outbound and inbound flows). Returns true once the smoltcp side is Closed.
    fn bridge(sock: &mut tcp::Socket, host: &mut TcpStream, host_done: &mut bool) {
        if sock.can_recv() {
            let _ = sock.recv(|data| {
                let n = host.write(data).unwrap_or(0);
                (n, ())
            });
        }
        if sock.can_send() && !*host_done {
            let mut buf = [0u8; 32 * 1024];
            match host.read(&mut buf) {
                Ok(0) => {
                    *host_done = true;
                    sock.close();
                }
                Ok(n) => {
                    let _ = sock.send_slice(&buf[..n]);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    *host_done = true;
                    sock.close();
                }
            }
        }
    }

    /// (src, dst, src_port, dst_port, syn, ack) of an Ethernet/IPv4/TCP frame.
    fn tcp_tuple(frame: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, u16, u16, bool, bool)> {
        let eth = EthernetFrame::new_checked(frame).ok()?;
        if eth.ethertype() != EthernetProtocol::Ipv4 {
            return None;
        }
        let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
        if ip.next_header() != IpProtocol::Tcp {
            return None;
        }
        let src = Ipv4Addr::from(ip.src_addr().octets());
        let dst = Ipv4Addr::from(ip.dst_addr().octets());
        let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
        Some((src, dst, tcp.src_port(), tcp.dst_port(), tcp.syn(), tcp.ack()))
    }

    /// Rewrite the IPv4 src/dst address and the TCP src/dst port in place, fixing
    /// both checksums. `None` leaves a field unchanged.
    fn rewrite(frame: &mut [u8], new_src: Option<Ipv4Address>, new_dst: Option<Ipv4Address>, new_sport: Option<u16>, new_dport: Option<u16>) {
        if frame.len() < 14 {
            return;
        }
        let (_, l3) = frame.split_at_mut(14);
        let ihl = match Ipv4Packet::new_checked(&l3[..]) {
            Ok(p) => p.header_len() as usize,
            Err(_) => return,
        };
        let (ip_bytes, tcp_bytes) = l3.split_at_mut(ihl);
        let mut ip = Ipv4Packet::new_unchecked(ip_bytes);
        if let Some(s) = new_src {
            ip.set_src_addr(s);
        }
        if let Some(d) = new_dst {
            ip.set_dst_addr(d);
        }
        let src_ip = IpAddress::Ipv4(ip.src_addr());
        let dst_ip = IpAddress::Ipv4(ip.dst_addr());
        ip.fill_checksum();

        let mut tcp = TcpPacket::new_unchecked(tcp_bytes);
        if let Some(p) = new_sport {
            tcp.set_src_port(p);
        }
        if let Some(p) = new_dport {
            tcp.set_dst_port(p);
        }
        tcp.fill_checksum(&src_ip, &dst_ip);
    }

    impl SmoltcpBackend {
        pub fn new() -> Self {
            let rx: Frames = Arc::new(Mutex::new(VecDeque::new()));
            let tx: Frames = Arc::new(Mutex::new(VecDeque::new()));
            let mut device = Bridge { rx, tx };
            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(MAC)));
            let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
            iface.update_ip_addrs(|addrs| {
                let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(GATEWAY), 24));
            });

            // A UDP socket on the gateway's own :53 — the guest's nameserver. No NAT
            // needed: the destination is the gateway's address, so smoltcp accepts.
            let mut sockets = SocketSet::new(Vec::new());
            let mut dns_sock = udp::Socket::new(
                udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 16 * 1024]),
                udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 16 * 1024]),
            );
            let _ = dns_sock.bind(IpListenEndpoint { addr: Some(IpAddress::Ipv4(GATEWAY)), port: 53 });
            let dns = sockets.add(dns_sock);

            // Inbound port-forwards from AMBER_PORTS="hostport:guestport,...": one
            // host listener per mapping, accepted connections queued for the netstack.
            let accepted: Arc<Mutex<VecDeque<(TcpStream, u16)>>> = Arc::new(Mutex::new(VecDeque::new()));
            let mut has_listeners = false;
            for m in std::env::var("AMBER_PORTS").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
                let Some((h, g)) = m.split_once(':') else { continue };
                let (Ok(hp), Ok(gp)) = (h.parse::<u16>(), g.parse::<u16>()) else { continue };
                match TcpListener::bind(("0.0.0.0", hp)) {
                    Ok(listener) => {
                        has_listeners = true;
                        let q = accepted.clone();
                        std::thread::spawn(move || {
                            for stream in listener.incoming().flatten() {
                                q.lock().unwrap().push_back((stream, gp));
                            }
                        });
                        log::info!("net: forwarding host :{hp} -> guest :{gp}");
                    }
                    Err(e) => log::warn!("net: cannot listen on :{hp}: {e}"),
                }
            }

            SmoltcpBackend {
                iface,
                device,
                sockets,
                flows: Vec::new(),
                dns,
                pending_dns: Vec::new(),
                accepted,
                inbound: Vec::new(),
                has_listeners,
                next_eph: 40000,
                start: std::time::Instant::now(),
            }
        }

        /// Dial the guest for each freshly accepted host connection, then bridge.
        fn accept_inbound(&mut self) {
            let pending: Vec<(TcpStream, u16)> = self.accepted.lock().unwrap().drain(..).collect();
            for (stream, gport) in pending {
                let _ = stream.set_nonblocking(true);
                self.next_eph = self.next_eph.wrapping_add(1).max(40000);
                let eph = self.next_eph;
                let mut sock = tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
                    tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
                );
                let r = sock.connect(self.iface.context(), (IpAddress::Ipv4(GUEST), gport), (IpAddress::Ipv4(GATEWAY), eph));
                if r.is_ok() {
                    let handle = self.sockets.add(sock);
                    self.inbound.push((handle, stream));
                } else {
                    log::debug!("net: inbound dial to guest:{gport} failed: {r:?}");
                }
            }
        }

        fn pump_inbound(&mut self) {
            for (handle, host) in &mut self.inbound {
                let sock = self.sockets.get_mut::<tcp::Socket>(*handle);
                let mut done = false;
                bridge(sock, host, &mut done);
            }
            let sockets = &mut self.sockets;
            self.inbound.retain(|(h, _)| {
                let dead = sockets.get::<tcp::Socket>(*h).state() == tcp::State::Closed;
                if dead {
                    sockets.remove(*h);
                }
                !dead
            });
        }

        /// Forward DNS queries the guest sent to the gateway out to the host
        /// resolver, and relay replies back. UDP, stateless, one host socket per
        /// in-flight query (matched back to its guest endpoint).
        fn pump_dns(&mut self) {
            // guest -> resolver
            loop {
                let sock = self.sockets.get_mut::<udp::Socket>(self.dns);
                let (payload, guest) = match sock.recv() {
                    Ok((data, meta)) => (data.to_vec(), meta.endpoint),
                    Err(_) => break,
                };
                if self.pending_dns.len() >= MAX_PENDING_DNS {
                    continue; // too many queries in flight: drop (the guest retries)
                }
                if let Ok(h) = UdpSocket::bind("0.0.0.0:0") {
                    let _ = h.set_nonblocking(true);
                    if h.connect(RESOLVER).is_ok() && h.send(&payload).is_ok() {
                        self.pending_dns.push(DnsQuery { host: h, guest });
                    }
                }
            }
            // resolver -> guest
            let sock = self.sockets.get_mut::<udp::Socket>(self.dns);
            self.pending_dns.retain_mut(|q| {
                let mut buf = [0u8; 2048];
                match q.host.recv(&mut buf) {
                    Ok(n) => {
                        let _ = sock.send_slice(&buf[..n], q.guest);
                        false
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
                    Err(_) => false,
                }
            });
        }

        fn now(&self) -> Instant {
            Instant::from_micros(self.start.elapsed().as_micros() as i64)
        }
        fn poll_iface(&mut self) {
            let now = self.now();
            self.iface.poll(now, &mut self.device, &mut self.sockets);
        }

        /// On a fresh SYN to a new destination, connect the host and open a smoltcp
        /// socket on the gateway IP + a fresh ephemeral port to terminate the
        /// guest's (NAT-rewritten) connection.
        fn maybe_open_flow(&mut self, frame: &[u8]) {
            let Some((src, dst, sport, dport, syn, ack)) = tcp_tuple(frame) else { return };
            if !syn || ack || src != Ipv4Addr::from(GUEST.octets()) {
                return;
            }
            if self.flows.iter().any(|f| f.guest_port == sport && f.ext_ip == dst && f.ext_port == dport) {
                return;
            }
            if self.flows.len() >= MAX_FLOWS {
                log::warn!("net: flow cap ({MAX_FLOWS}) reached, dropping SYN to {dst}:{dport}");
                return;
            }
            // Connect on a thread so the vcpu is never blocked; the stream arrives
            // over the channel and the flow starts forwarding once it lands.
            let (tx, rx) = std::sync::mpsc::channel();
            let addr = SocketAddr::from((dst, dport));
            std::thread::spawn(move || {
                let _ = tx.send(TcpStream::connect_timeout(&addr, Duration::from_secs(5)));
            });
            self.next_eph = self.next_eph.wrapping_add(1).max(40000);
            let eph = self.next_eph;
            let mut sock = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
                tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
            );
            if sock.listen((IpAddress::Ipv4(GATEWAY), eph)).is_err() {
                return;
            }
            let handle = self.sockets.add(sock);
            self.flows.push(Flow {
                guest_port: sport,
                ext_ip: dst,
                ext_port: dport,
                eph,
                handle,
                host: None,
                connecting: Some(rx),
                host_done: false,
            });
        }

        fn pump_flows(&mut self) {
            for flow in &mut self.flows {
                let sock = self.sockets.get_mut::<tcp::Socket>(flow.handle);

                // Resolve a still-connecting host (non-blocking).
                if flow.host.is_none() {
                    match flow.connecting.as_ref().map(|rx| rx.try_recv()) {
                        Some(Ok(Ok(s))) => {
                            let _ = s.set_nonblocking(true);
                            flow.host = Some(s);
                            flow.connecting = None;
                        }
                        Some(Ok(Err(_))) | Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                            // Connect failed: tear the guest side down.
                            flow.connecting = None;
                            flow.host_done = true;
                            sock.abort();
                        }
                        _ => {} // still connecting; leave guest data buffered in smoltcp
                    }
                }
                let Some(host) = flow.host.as_mut() else { continue };
                bridge(sock, host, &mut flow.host_done);
            }
            let sockets = &mut self.sockets;
            self.flows.retain(|flow| {
                let dead = sockets.get::<tcp::Socket>(flow.handle).state() == tcp::State::Closed;
                if dead {
                    sockets.remove(flow.handle);
                }
                !dead
            });
        }

        /// Rewrite a guest->external TCP frame so its destination is the gateway IP
        /// and the flow's ephemeral port (smoltcp then owns the destination).
        fn dnat_in(&self, frame: &mut [u8]) {
            let Some((src, dst, sport, dport, _, _)) = tcp_tuple(frame) else { return };
            if src != Ipv4Addr::from(GUEST.octets()) {
                return;
            }
            if let Some(f) = self.flows.iter().find(|f| f.guest_port == sport && f.ext_ip == dst && f.ext_port == dport) {
                rewrite(frame, None, Some(GATEWAY), None, Some(f.eph));
            }
        }

        /// Rewrite a gateway->guest reply so its source looks like the external
        /// host the guest connected to (undo the DNAT).
        fn dnat_out(&self, frame: &mut [u8]) {
            let Some((src, dst, sport, dport, _, _)) = tcp_tuple(frame) else { return };
            if src != Ipv4Addr::from(GATEWAY.octets()) || dst != Ipv4Addr::from(GUEST.octets()) {
                return;
            }
            if let Some(f) = self.flows.iter().find(|f| f.eph == sport && f.guest_port == dport) {
                let ext = Ipv4Address::from(f.ext_ip.octets());
                rewrite(frame, Some(ext), None, Some(f.ext_port), None);
            }
        }
    }

    impl NetBackend for SmoltcpBackend {
        fn send(&mut self, frame: &[u8]) {
            self.maybe_open_flow(frame);
            let mut f = frame.to_vec();
            self.dnat_in(&mut f);
            self.device.rx.lock().unwrap().push_back(f);
            self.poll_iface();
            self.pump_flows();
            self.pump_dns();
            self.poll_iface();
        }
        fn poll(&mut self) -> Option<Vec<u8>> {
            self.poll_iface();
            self.accept_inbound();
            self.pump_flows();
            self.pump_inbound();
            self.pump_dns();
            self.poll_iface();
            let mut f = self.device.tx.lock().unwrap().pop_front()?;
            self.dnat_out(&mut f);
            Some(f)
        }
        fn mac(&self) -> [u8; 6] {
            MAC
        }
        fn wants_poll(&self) -> bool {
            // Keep polling while there is work or any inbound listener (so accepted
            // connections are dialed promptly even when the guest is otherwise idle).
            self.has_listeners
                || !self.flows.is_empty()
                || !self.inbound.is_empty()
                || !self.pending_dns.is_empty()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use smoltcp::phy::ChecksumCapabilities;
        use smoltcp::wire::{Ipv4Repr, TcpControl, TcpRepr, TcpSeqNumber};

        /// Build an Ethernet/IPv4/TCP frame with correct checksums.
        fn build(src: Ipv4Address, dst: Ipv4Address, sport: u16, dport: u16, ctrl: TcpControl) -> Vec<u8> {
            let tcp_repr = TcpRepr {
                src_port: sport,
                dst_port: dport,
                control: ctrl,
                seq_number: TcpSeqNumber(1),
                ack_number: None,
                window_len: 1024,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None, None, None],
                timestamp: None,
                payload: &[],
            };
            let ip_repr = Ipv4Repr {
                src_addr: src,
                dst_addr: dst,
                next_header: IpProtocol::Tcp,
                payload_len: tcp_repr.buffer_len(),
                hop_limit: 64,
            };
            // buffer_len() is the header only (smoltcp emits no IP options), so the
            // frame is eth(14) + IP header + TCP segment.
            let tcp_off = 14 + ip_repr.buffer_len();
            let mut buf = vec![0u8; tcp_off + tcp_repr.buffer_len()];
            {
                let mut eth = EthernetFrame::new_unchecked(&mut buf);
                eth.set_src_addr(EthernetAddress([2, 2, 2, 2, 2, 2]));
                eth.set_dst_addr(EthernetAddress(MAC));
                eth.set_ethertype(EthernetProtocol::Ipv4);
            }
            let src_ip = IpAddress::Ipv4(src);
            let dst_ip = IpAddress::Ipv4(dst);
            let caps = ChecksumCapabilities::default();
            {
                let mut ip = Ipv4Packet::new_unchecked(&mut buf[14..]);
                ip_repr.emit(&mut ip, &caps);
            }
            {
                let mut tcp = TcpPacket::new_unchecked(&mut buf[tcp_off..]);
                tcp_repr.emit(&mut tcp, &src_ip, &dst_ip, &caps);
            }
            buf
        }

        /// Re-validate both header checksums (new_checked only checks structure).
        fn checksums_ok(frame: &[u8]) -> bool {
            let eth = EthernetFrame::new_checked(frame).unwrap();
            let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
            if !ip.verify_checksum() {
                return false;
            }
            let src = IpAddress::Ipv4(ip.src_addr());
            let dst = IpAddress::Ipv4(ip.dst_addr());
            let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
            tcp.verify_checksum(&src, &dst)
        }

        #[test]
        fn tcp_tuple_parses_a_valid_syn() {
            let f = build(GUEST, Ipv4Address::new(93, 184, 216, 34), 50000, 80, TcpControl::Syn);
            assert!(checksums_ok(&f));
            let (src, dst, sp, dp, syn, ack) = tcp_tuple(&f).unwrap();
            assert_eq!(src, Ipv4Addr::new(10, 0, 0, 2));
            assert_eq!(dst, Ipv4Addr::new(93, 184, 216, 34));
            assert_eq!((sp, dp, syn, ack), (50000, 80, true, false));
        }

        #[test]
        fn tcp_tuple_rejects_non_tcp_and_short() {
            assert!(tcp_tuple(&[0u8; 10]).is_none()); // shorter than an Ethernet header
            let mut arp = vec![0u8; 60];
            arp[12] = 0x08;
            arp[13] = 0x06; // ARP ethertype, not IPv4
            assert!(tcp_tuple(&arp).is_none());
        }

        #[test]
        fn rewrite_dnat_in_changes_dst_and_fixes_checksums() {
            let mut f = build(GUEST, Ipv4Address::new(1, 2, 3, 4), 50000, 80, TcpControl::Syn);
            rewrite(&mut f, None, Some(GATEWAY), None, Some(40001));
            assert!(checksums_ok(&f)); // checksums stay valid after the edit
            let (src, dst, sp, dp, ..) = tcp_tuple(&f).unwrap();
            assert_eq!(dst, Ipv4Addr::new(10, 0, 0, 1)); // dst -> gateway
            assert_eq!(dp, 40001); // dport -> ephemeral
            assert_eq!((src, sp), (Ipv4Addr::new(10, 0, 0, 2), 50000)); // src untouched
        }

        #[test]
        fn rewrite_dnat_out_restores_source() {
            // A reply gateway:40001 -> guest, rewritten to look like the external host.
            let mut f = build(GATEWAY, GUEST, 40001, 50000, TcpControl::Syn);
            let ext = Ipv4Address::new(1, 2, 3, 4);
            rewrite(&mut f, Some(ext), None, Some(80), None);
            assert!(checksums_ok(&f));
            let (src, _, sp, ..) = tcp_tuple(&f).unwrap();
            assert_eq!(src, Ipv4Addr::new(1, 2, 3, 4));
            assert_eq!(sp, 80);
        }

        #[test]
        fn rewrite_on_garbage_does_not_panic() {
            let mut tiny = vec![0u8; 10];
            rewrite(&mut tiny, Some(GATEWAY), None, None, None); // < 14 bytes: early out
            let mut junk = vec![0xffu8; 14]; // Ethernet header, garbage L3
            rewrite(&mut junk, None, Some(GATEWAY), None, None); // new_checked fails: no-op
        }
    }
}
