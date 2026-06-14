#![no_main]
//! Fuzz the guest→host vsock packet parser. The guest fully controls this byte
//! string in production, so no input may panic, over-read, or loop.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    amber_core::vsock::fuzz_on_guest_packet(data);
});
