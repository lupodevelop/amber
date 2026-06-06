# STATUS

This is the M0 skeleton, not a finished VMM. It is structured so the seam is real
and the hard, knowledge-dense part (the HVF arm64 bring-up) is written rather than
stubbed. The flesh (virtio devices, image layer, snapshot, budget, the real CLI)
is yours to write, per `docs/ROADMAP.md`.

## Not compiled here

I wrote this without an arm64 macOS toolchain to build it on, so treat it as a
correct-by-construction starting point, not a green build. Two classes of risk:

1. The HVF FFI constants in `crates/amber-hvf/src/ffi.rs` marked `VERIFY`. The
   enum discriminants for `hv_reg_t` (PC, CPSR), `hv_exit_reason_t`, and the HVC
   exception class are the things most likely to differ from what I wrote. Check
   them against the SDK headers before trusting a boot:
   `$(xcrun --show-sdk-path)/usr/include/Hypervisor/hv_vcpu_types.h`.
   If you would rather not maintain raw FFI, retarget `lib.rs` to the `applevisor`
   crate; nothing in `amber-core` changes.

2. The `vm-fdt` API calls in `crates/amber-core/src/dtb.rs`. The method names
   (`property_string_list`, `property_array_u64`, `property_null`) match the crate
   as I recall it; pin the version and adjust if the signatures differ.

## What is solid

- The crate split and the two seams. `amber-core` names no hypervisor; the backend
  implements `Hypervisor` + `Vcpu` and nothing else.
- The arm64 boot logic: Image header parse, 2 MiB-aligned load, x0=DTB, PC=entry.
- The ESR data-abort decoder in `hypervisor.rs` (EC 0x24, ISV, SAS, SRT, WnR). This
  is the core of MMIO emulation and it is the standard decode.
- The QEMU `virt` memory map, which is the right reference and what earlycon expects.
- The run loop: MMIO read/write dispatch, PC+4 step, PSCI SYSTEM_OFF via HVC.

## What M0 actually proves

Not "boot to a shell". That needs a GIC and a working timer, which is M0.5. M0
proves the kernel boots far enough to emit earlycon output through the emulated
PL011 over the MMIO trap path. When you see Linux early boot text on stdout, the
boot protocol, the memory map, the DTB, the run loop, and the exit decoding are
all correct. The kernel will then stall once it needs the timer, which is expected
and is the next milestone.

## Build and run

```sh
# on Apple Silicon, macOS
cargo build --release

# HVF needs the hypervisor entitlement; sign the binary before running
codesign --entitlements amber.entitlements -s - target/release/amber

# a stock arm64 Linux kernel Image (e.g. from a distro arm64 build)
./target/release/amber boot path/to/Image
```

If you see kernel early-boot lines, M0 is done. If `hv_vm_create` fails with a
permission error, the entitlement/codesign step did not take.

## Next, in order

- M0.5: wire the GIC (newer macOS exposes `hv_gic_*`) and the architected timer,
  reach a shell with a small initramfs.
- M1: `virtio-mmio` + `virtio-vsock` and `virtio-blk`, then the OCI image layer.

See `docs/ROADMAP.md`.
