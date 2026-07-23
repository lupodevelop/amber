//! virtio-net plus a pluggable host-network seam.
//!
//! Firecracker-shaped: the device is just a packet pump between the guest's
//! rx/tx virtqueues and a [`NetBackend`]; the backend is what actually moves
//! frames to and from the host. Backends are interchangeable — a userspace
//! netstack (smoltcp), a helper process (gvproxy), the Apple `vmnet` framework,
//! or a Linux TAP fd for the KVM backend — chosen at startup, so the device
//! never changes when the host strategy does.

use crate::memory::GuestRam;
use crate::virtio::{Buf, VirtioDevice};

/// The host-side network seam. One implementation per strategy.
pub trait NetBackend: Send {
    /// A frame the guest transmitted (raw Ethernet, virtio header already stripped).
    fn send(&mut self, frame: &[u8]);
    /// The next frame to hand the guest, if one is ready. Non-blocking; the device
    /// polls it from the run loop.
    fn poll(&mut self) -> Option<Vec<u8>>;
    /// The MAC the device advertises to the guest. A locally-administered address.
    fn mac(&self) -> [u8; 6] {
        [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
    }
    /// True while the backend has in-flight work (open connections, pending
    /// queries) whose host-side responses arrive asynchronously. The run loop polls
    /// more eagerly while this holds so replies reach the guest in ~ms, not the
    /// idle-park cap.
    fn wants_poll(&self) -> bool {
        false
    }
}

/// A backend that drops transmits and never receives, logging a one-line summary
/// of each frame the guest sends — for bring-up and tests (proves the device and
/// the tx path work without any host networking).
pub struct CaptureBackend;

impl NetBackend for CaptureBackend {
    fn send(&mut self, frame: &[u8]) {
        let ethertype = if frame.len() >= 14 {
            u16::from_be_bytes([frame[12], frame[13]])
        } else {
            0
        };
        let kind = match ethertype {
            0x0806 => "ARP",
            0x0800 => "IPv4",
            0x86dd => "IPv6",
            _ => "?",
        };
        log::info!("net tx: {} bytes, ethertype {ethertype:#06x} ({kind})", frame.len());
    }
    fn poll(&mut self) -> Option<Vec<u8>> {
        None
    }
}

/// The modern virtio-net header (VIRTIO_F_VERSION_1) prefixing every frame on the
/// queues: flags, gso, csum fields, and num_buffers — 12 bytes. We use no offloads,
/// so it is zeroed except num_buffers on receive.
const VIRTIO_NET_HDR_LEN: usize = 12;

/// Ceiling on one assembled tx frame. No segmentation offload is negotiated, so a
/// real frame stays near the MTU; this caps what a long hostile chain can build.
const MAX_TX_FRAME: usize = 64 * 1024;

/// virtio-net (device id 1). Queue 0 is receive (filled asynchronously as frames
/// arrive from the backend), queue 1 is transmit.
pub struct NetDevice {
    backend: Box<dyn NetBackend>,
    mac: [u8; 6],
    /// Per-direction rate caps (`AMBER_NET_BPS`); None = unlimited. Transmit
    /// blocks (backpressure in the notify); receive defers via `pending_rx`.
    tx_limit: Option<crate::limiter::TokenBucket>,
    rx_limit: Option<crate::limiter::TokenBucket>,
    /// A received frame the rate cap deferred; retried on the next rx pump.
    pending_rx: Option<Vec<u8>>,
    /// Transmit rate debt the run loop must sleep off — set in `handle`, drained
    /// by `take_throttle` so the vcpu sleeps AFTER releasing the shared virtio
    /// lock, not while holding it (which would freeze every other device).
    tx_debt: Option<std::time::Duration>,
}

impl NetDevice {
    const DEVICE_ID: u32 = 1;
    const RX: usize = 0;
    const TX: usize = 1;
    const F_MAC: u32 = 1 << 5; // VIRTIO_NET_F_MAC
    const F_VERSION_1: u32 = 1; // bit 32 -> selector 1, bit 0

    pub fn new(backend: Box<dyn NetBackend>) -> Self {
        let mac = backend.mac();
        Self {
            backend,
            mac,
            tx_limit: crate::limiter::TokenBucket::from_env("AMBER_NET_BPS"),
            rx_limit: crate::limiter::TokenBucket::from_env("AMBER_NET_BPS"),
            pending_rx: None,
            tx_debt: None,
        }
    }
}

impl VirtioDevice for NetDevice {
    fn device_id(&self) -> u32 {
        Self::DEVICE_ID
    }
    fn num_queues(&self) -> usize {
        2
    }
    fn device_features(&self, sel: u32) -> u32 {
        match sel {
            0 => Self::F_MAC,
            1 => Self::F_VERSION_1,
            _ => 0,
        }
    }
    fn config(&self, off: u64) -> u32 {
        // The MAC occupies config bytes 0..6; everything else reads as zero.
        let mut b = [0u8; 4];
        for (i, byte) in b.iter_mut().enumerate() {
            let idx = off as usize + i;
            if idx < self.mac.len() {
                *byte = self.mac[idx];
            }
        }
        u32::from_le_bytes(b)
    }
    fn rx_queue(&self) -> Option<usize> {
        Some(Self::RX)
    }
    fn wants_poll(&self) -> bool {
        // A rate-deferred frame needs the loop to keep polling so it lands as
        // soon as the bucket refills.
        self.pending_rx.is_some() || self.backend.wants_poll()
    }
    fn handle(&mut self, queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32 {
        if queue == Self::TX {
            // Gather the readable descriptors into one frame, drop the virtio-net
            // header, and hand the Ethernet frame to the backend.
            let mut frame = Vec::new();
            for b in bufs.iter().filter(|b| !b.writable) {
                if frame.len() >= MAX_TX_FRAME {
                    break;
                }
                let take = (b.len as usize).min(MAX_TX_FRAME - frame.len());
                let mut d = vec![0u8; take];
                ram.read(b.addr, &mut d);
                frame.extend_from_slice(&d);
            }
            if frame.len() > VIRTIO_NET_HDR_LEN {
                if let Some(l) = &mut self.tx_limit {
                    // Record the deficit; the run loop sleeps it off off-lock.
                    self.tx_debt = l.debit((frame.len() - VIRTIO_NET_HDR_LEN) as u64);
                }
                self.backend.send(&frame[VIRTIO_NET_HDR_LEN..]);
            }
        }
        // tx consumes the buffer with no bytes written back; rx is pumped elsewhere.
        0
    }
    fn take_throttle(&mut self) -> Option<std::time::Duration> {
        self.tx_debt.take()
    }
    fn poll_rx(&mut self, _max_frame: usize) -> Option<Vec<u8>> {
        // Ethernet frames are whole and fit any posted rx buffer, so `max_frame`
        // (the guest's buffer capacity) is not a constraint here.
        // A deferred frame goes first (ordering); otherwise pull from the backend.
        let frame = match self.pending_rx.take() {
            Some(f) => f,
            None => self.backend.poll()?,
        };
        // Receive cap: never sleep on the rx pump (it runs on the vcpu loop) —
        // stash the frame and deliver once the bucket refills.
        if let Some(l) = &mut self.rx_limit {
            if !l.try_take(frame.len() as u64) {
                self.pending_rx = Some(frame);
                return None;
            }
        }
        // Prepend a zeroed modern header with num_buffers = 1 (LE u16 at byte 10).
        let mut out = vec![0u8; VIRTIO_NET_HDR_LEN];
        out[10] = 1;
        out.extend_from_slice(&frame);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::GuestMemory;
    use std::sync::{Arc, Mutex};

    const BASE: u64 = 0x4000_0000;
    const DATA: u64 = BASE + 0x1000;

    /// A backend that records transmitted frames and replays queued receives.
    #[derive(Clone, Default)]
    struct RecBackend {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        inq: Arc<Mutex<Vec<Vec<u8>>>>,
    }
    impl NetBackend for RecBackend {
        fn send(&mut self, frame: &[u8]) {
            self.sent.lock().unwrap().push(frame.to_vec());
        }
        fn poll(&mut self) -> Option<Vec<u8>> {
            self.inq.lock().unwrap().pop()
        }
    }

    fn ram(len: usize) -> GuestMemory {
        GuestMemory::new(BASE, len).unwrap()
    }

    #[test]
    fn tx_strips_header_and_forwards_ethernet() {
        let be = RecBackend::default();
        let sent = be.sent.clone();
        let mut dev = NetDevice::new(Box::new(be));
        let m = ram(0x2000);
        let r = m.ram();
        let payload = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x11];
        r.write(DATA, &[0u8; VIRTIO_NET_HDR_LEN]);
        r.write(DATA + VIRTIO_NET_HDR_LEN as u64, &payload);
        let bufs = vec![Buf { addr: DATA, len: (VIRTIO_NET_HDR_LEN + payload.len()) as u32, writable: false }];
        assert_eq!(dev.handle(NetDevice::TX, &r, &bufs), 0);
        let s = sent.lock().unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0], payload);
    }

    #[test]
    fn tx_throttle_is_deferred_not_slept() {
        // M1: a rate-limited tx notify must NOT sleep inside `handle` (it runs
        // under the shared virtio lock). It records the deficit for the run loop
        // to sleep off after releasing the lock.
        let be = RecBackend::default();
        let mut dev = NetDevice {
            backend: Box::new(be),
            mac: [0u8; 6],
            tx_limit: Some(crate::limiter::TokenBucket::new(1000)), // 1000 B/s
            rx_limit: None,
            pending_rx: None,
            tx_debt: None,
        };
        let m = ram(0x2000);
        let r = m.ram();
        let n = VIRTIO_NET_HDR_LEN + 2000; // 2000 payload bytes, 2x the per-sec budget
        r.write(DATA, &vec![0u8; n]);
        let bufs = vec![Buf { addr: DATA, len: n as u32, writable: false }];

        let t0 = std::time::Instant::now();
        dev.handle(NetDevice::TX, &r, &bufs);
        let elapsed = t0.elapsed();

        assert!(elapsed < std::time::Duration::from_millis(200), "handle slept: {elapsed:?}");
        assert!(dev.take_throttle().is_some(), "deficit not recorded");
        assert!(dev.take_throttle().is_none(), "deficit must drain on take");
    }

    #[test]
    fn tx_header_only_frame_is_dropped() {
        let be = RecBackend::default();
        let sent = be.sent.clone();
        let mut dev = NetDevice::new(Box::new(be));
        let m = ram(0x2000);
        let r = m.ram();
        let bufs = vec![Buf { addr: DATA, len: VIRTIO_NET_HDR_LEN as u32, writable: false }];
        dev.handle(NetDevice::TX, &r, &bufs);
        assert!(sent.lock().unwrap().is_empty()); // nothing past the header
    }

    #[test]
    fn tx_frame_is_capped() {
        let be = RecBackend::default();
        let sent = be.sent.clone();
        let mut dev = NetDevice::new(Box::new(be));
        let m = ram(1 << 20);
        let r = m.ram();
        let bufs = vec![
            Buf { addr: DATA, len: 60 * 1024, writable: false },
            Buf { addr: DATA + 60 * 1024, len: 60 * 1024, writable: false },
        ];
        dev.handle(NetDevice::TX, &r, &bufs);
        let s = sent.lock().unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].len(), MAX_TX_FRAME - VIRTIO_NET_HDR_LEN);
    }

    #[test]
    fn tx_ignores_writable_descriptors() {
        let be = RecBackend::default();
        let sent = be.sent.clone();
        let mut dev = NetDevice::new(Box::new(be));
        let m = ram(0x2000);
        let r = m.ram();
        // A writable buffer is part of the rx path, never gathered into a tx frame.
        let bufs = vec![Buf { addr: DATA, len: 100, writable: true }];
        dev.handle(NetDevice::TX, &r, &bufs);
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn rx_prepends_header_with_num_buffers() {
        let be = RecBackend::default();
        be.inq.lock().unwrap().push(vec![1, 2, 3, 4]);
        let mut dev = NetDevice::new(Box::new(be));
        let out = dev.poll_rx(65536).unwrap();
        assert_eq!(out.len(), VIRTIO_NET_HDR_LEN + 4);
        assert_eq!(out[10], 1); // num_buffers = 1
        assert_eq!(&out[VIRTIO_NET_HDR_LEN..], &[1, 2, 3, 4]);
    }

    #[test]
    fn tx_rate_cap_applies_backpressure() {
        let be = RecBackend::default();
        let sent = be.sent.clone();
        let mut dev = NetDevice::new(Box::new(be));
        let mut bucket = crate::limiter::TokenBucket::new(20 * 1024); // 20 KiB/s
        assert!(bucket.try_take(20 * 1024)); // drain the burst
        dev.tx_limit = Some(bucket);
        let m = ram(0x2000);
        let r = m.ram();
        let len = VIRTIO_NET_HDR_LEN as u32 + 2048; // 2 KiB payload ≈ 100 ms debt
        let bufs = vec![Buf { addr: DATA, len, writable: false }];
        dev.handle(NetDevice::TX, &r, &bufs);
        // Backpressure surfaces as a deferred debt (the run loop sleeps it off
        // off-lock), and the frame is throttled, not dropped.
        assert!(dev.take_throttle().expect("a debt") >= std::time::Duration::from_millis(60));
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn rx_rate_cap_defers_not_drops() {
        let be = RecBackend::default();
        be.inq.lock().unwrap().push(vec![7u8; 64]);
        let mut dev = NetDevice::new(Box::new(be));
        let mut bucket = crate::limiter::TokenBucket::new(1024); // 1 KiB/s
        assert!(bucket.try_take(1024)); // drain: 64 B need ~62 ms to refill
        dev.rx_limit = Some(bucket);
        assert!(dev.poll_rx(65536).is_none()); // deferred...
        assert!(dev.wants_poll()); // ...and the loop is told to keep polling
        std::thread::sleep(std::time::Duration::from_millis(150));
        let out = dev.poll_rx(65536).expect("delivered after refill");
        assert_eq!(&out[VIRTIO_NET_HDR_LEN..], &[7u8; 64]); // same frame, in order
    }

    #[test]
    fn config_exposes_mac() {
        let dev = NetDevice::new(Box::new(RecBackend::default()));
        assert_eq!(dev.config(0).to_le_bytes(), [0x52, 0x54, 0x00, 0x12]);
        let hi = dev.config(4).to_le_bytes();
        assert_eq!([hi[0], hi[1]], [0x34, 0x56]);
    }
}
