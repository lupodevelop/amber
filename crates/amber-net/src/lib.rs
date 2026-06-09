//! Host-side network backends behind amber-core's `NetBackend` seam.
//!
//! Each strategy is feature-gated so its dependencies stay opt-in. The default is
//! a userspace netstack (smoltcp): amber is the gateway (10.0.0.1) and the guest a
//! static 10.0.0.2; smoltcp answers ARP and ICMP for the gateway, and (later
//! modules) proxies the guest's TCP/UDP to host sockets. gvproxy/vmnet/tap slot in
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
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::phy::{Device, DeviceCapabilities, Medium};
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// The gateway amber presents to the guest. The guest is statically configured
    /// at GUEST with GATEWAY as its default route.
    const GATEWAY: [u8; 4] = [10, 0, 0, 1];
    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const MTU: usize = 1500;

    type Frames = Arc<Mutex<VecDeque<Vec<u8>>>>;

    /// A smoltcp `phy::Device` bridged to the virtio-net queues: `rx` holds frames
    /// the guest sent (smoltcp consumes), `tx` holds frames smoltcp produced (the
    /// device hands them to the guest).
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

    pub struct SmoltcpBackend {
        iface: Interface,
        device: Bridge,
        sockets: SocketSet<'static>,
        start: std::time::Instant,
    }

    impl SmoltcpBackend {
        pub fn new() -> Self {
            let rx: Frames = Arc::new(Mutex::new(VecDeque::new()));
            let tx: Frames = Arc::new(Mutex::new(VecDeque::new()));
            let mut device = Bridge { rx, tx };

            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(MAC)));
            let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
            iface.update_ip_addrs(|addrs| {
                let _ = addrs.push(IpCidr::new(
                    IpAddress::v4(GATEWAY[0], GATEWAY[1], GATEWAY[2], GATEWAY[3]),
                    24,
                ));
            });

            SmoltcpBackend {
                iface,
                device,
                sockets: SocketSet::new(Vec::new()),
                start: std::time::Instant::now(),
            }
        }

        fn now(&self) -> Instant {
            Instant::from_micros(self.start.elapsed().as_micros() as i64)
        }

        fn poll_iface(&mut self) {
            let now = self.now();
            self.iface.poll(now, &mut self.device, &mut self.sockets);
        }
    }

    impl NetBackend for SmoltcpBackend {
        fn send(&mut self, frame: &[u8]) {
            self.device.rx.lock().unwrap().push_back(frame.to_vec());
            self.poll_iface();
        }
        fn poll(&mut self) -> Option<Vec<u8>> {
            self.poll_iface();
            self.device.tx.lock().unwrap().pop_front()
        }
        fn mac(&self) -> [u8; 6] {
            MAC
        }
    }
}
