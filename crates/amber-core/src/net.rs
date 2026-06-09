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

/// virtio-net (device id 1). Queue 0 is receive (filled asynchronously as frames
/// arrive from the backend), queue 1 is transmit.
pub struct NetDevice {
    backend: Box<dyn NetBackend>,
    mac: [u8; 6],
}

impl NetDevice {
    const DEVICE_ID: u32 = 1;
    const RX: usize = 0;
    const TX: usize = 1;
    const F_MAC: u32 = 1 << 5; // VIRTIO_NET_F_MAC
    const F_VERSION_1: u32 = 1; // bit 32 -> selector 1, bit 0

    pub fn new(backend: Box<dyn NetBackend>) -> Self {
        let mac = backend.mac();
        Self { backend, mac }
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
    fn handle(&mut self, queue: usize, ram: &GuestRam, bufs: &[Buf]) -> u32 {
        if queue == Self::TX {
            // Gather the readable descriptors into one frame, drop the virtio-net
            // header, and hand the Ethernet frame to the backend.
            let mut frame = Vec::new();
            for b in bufs.iter().filter(|b| !b.writable) {
                let mut d = vec![0u8; b.len as usize];
                ram.read(b.addr, &mut d);
                frame.extend_from_slice(&d);
            }
            if frame.len() > VIRTIO_NET_HDR_LEN {
                self.backend.send(&frame[VIRTIO_NET_HDR_LEN..]);
            }
        }
        // tx consumes the buffer with no bytes written back; rx is pumped elsewhere.
        0
    }
    fn poll_rx(&mut self) -> Option<Vec<u8>> {
        let frame = self.backend.poll()?;
        // Prepend a zeroed modern header with num_buffers = 1 (LE u16 at byte 10).
        let mut out = vec![0u8; VIRTIO_NET_HDR_LEN];
        out[10] = 1;
        out.extend_from_slice(&frame);
        Some(out)
    }
}
