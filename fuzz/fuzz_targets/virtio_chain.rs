#![no_main]
//! Fuzz the virtqueue descriptor walkers. The guest owns the descriptor table,
//! the avail/used rings, and the queue indices, so the walkers must stay bounded
//! and in-RAM for any ring contents.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    amber_core::virtio::fuzz_descriptor_chain(data);
});
