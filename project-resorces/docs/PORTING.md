# PORTING

amber is arm64-only by design (see `project-resorces/docs/DESIGN.md`), and the
near-term roadmap stays there: HVF on Apple Silicon now, KVM on arm64 Linux at
M8. This document is the opposite of that: it collects what a port *off* arm64 —
to x86-64, to Windows, to other hypervisors — would actually take, plus the
external references worth reading before starting. It is a parking lot, not a
plan. Nothing here is committed work.

## The seam, restated

Everything above the `Hypervisor` / `Vcpu` traits in `amber-core` is written
once and is host-agnostic: the run loop, the MMIO bus, the device tree builder,
the loader, snapshot/fork (later). A new backend implements two things — how
guest memory is mapped, and how a vcpu runs and decodes its exits. The cost of a
port is concentrated in (a) that backend and (b) anything in `amber-core` that
silently assumes arm64. Today those arm64 assumptions are: the Linux `Image`
boot protocol + DTB (`loader.rs`, `dtb.rs`), the ESR data-abort decode
(`hypervisor.rs::decode_data_abort`), PSCI-over-HVC, the GIC, and the
architected timer. An x86 port replaces all of those; a same-ISA port (arm64
Linux/KVM) replaces almost none.

## Targets, cheapest first

### arm64 Linux / KVM — the planned M8, low risk

Same guest, same boot protocol, same device tree, same GIC and timer model.
Only the backend changes: `KVM_SET_USER_MEMORY_REGION` instead of `hv_vm_map`,
`KVM_RUN` instead of `hv_vcpu_run`, an in-kernel GICv3 via `KVM_CREATE_DEVICE`
instead of `hv_gic_create`, and `KVM_IRQFD`/`KVM_IRQ_LINE` for injection. The
ESR decode, PSCI, the DTB, and the whole run loop are reused unchanged. This is
the "small file" the architecture promises, and it doubles as a Firecracker
correctness oracle on the same host.

### x86-64 Linux / KVM — a second architecture, high effort

This is not a backend swap; it is a second guest architecture. What `amber-core`
would have to grow:
- A second boot protocol. x86 Linux uses the bzImage/`boot_params` protocol or
  PVH, not the arm64 `Image` + DTB path. New loader, new in-memory structures
  (zero page, e820 map, command line), no device tree.
- A different interrupt story. LAPIC + IOAPIC (or the PVH/MSI path) instead of
  the GIC. The "inject an IRQ line" abstraction survives; everything under it is
  new.
- Different exit decoding. VMX/SVM exit reasons and instruction decode for MMIO,
  instead of the ESR data-abort syndrome. x86 does not hand you a clean
  syndrome with the access size and source register the way `ESR.ISS` does, so
  MMIO emulation needs real instruction decoding.
- CPUID, MSRs, segment/descriptor setup, real-mode-ish entry quirks.
The seam still holds, but the body of work is comparable to the original arm64
bring-up, not a small file.

### Windows — Windows Hypervisor Platform (WHP)

WHP is the public user-mode API over Hyper-V (`WHvCreatePartition`,
`WHvMapGpaRange`, `WHvCreateVirtualProcessor`, `WHvRunVirtualProcessor`). It is
x86-64 in practice, so a Windows port is really "x86-64 port" plus "WHP
backend". Pairs naturally with the x86 work above. Hyper-V/MSHV is the
lower-level interface; WHP is the one to target for a userspace VMM.

## External references

### Cross-platform VMM building blocks
- **rust-vmm** — the shared crates amber already builds on: `vm-memory`,
  `virtio-queue`, `linux-loader` (has x86 boot-protocol support too),
  `vmm-sys-util`, `event-manager`, `kvm-ioctls`, `kvm-bindings`. The x86 boot
  and KVM plumbing live here. https://github.com/rust-vmm
- **Cloud Hypervisor** — a production rust-vmm VMM covering KVM (x86 + arm64)
  and MSHV; the reference for how the same device model spans architectures and
  hypervisors. https://github.com/cloud-hypervisor/cloud-hypervisor
- **Firecracker** — KVM, x86-64 and arm64 Linux. The device-model and snapshot
  reference, and a direct correctness oracle on Linux.
  https://github.com/firecracker-microvm/firecracker

### Hyperlight (the repo flagged for this note)
- **hyperlight** — Microsoft's micro-sandbox: runs *no-OS* guests (no kernel,
  function-call-fast) behind KVM, Hyper-V/MSHV, and Windows Hypervisor Platform.
  https://github.com/hyperlight-dev/hyperlight
  - Relevance: **x86-64 only, no HVF / arm64 / GIC**, so nothing reusable for the
    current M0.5 HVF timer/GIC work. But it is the closest thing in *spirit* to
    amber's thesis (seal untrusted code in a hardware-isolated, millisecond-spawn
    sandbox), and it is the best Rust reference for the **WHP and MSHV backends**
    if amber ever wants Windows/x86. Worth reading its `src/hyperlight_host`
    hypervisor abstraction before writing a WHP backend.

### Apple Silicon / HVF (current backend)
- Apple `Hypervisor` framework docs, esp. the `hv_gic_*` GICv3 API and the
  vtimer: https://developer.apple.com/documentation/hypervisor
- **cloud-hypervisor/hypervisor-framework** — Rust bindings for the macOS
  Hypervisor framework, including the GIC and vcpu APIs.
  https://github.com/cloud-hypervisor/hypervisor-framework
- **applevisor** / **applevisor-sys** — higher-level Rust wrappers over the HVF
  arm64 API; the fallback if hand-maintaining `amber-hvf/src/ffi.rs` becomes a
  burden. https://docs.rs/applevisor
- QEMU's HVF platform-vGIC + nested-virt series (the authoritative worked
  example of `hv_gic_create` + vtimer handling): search the qemu-devel archive
  for "HVF: Add support for platform vGIC and nested virtualisation", e.g.
  https://www.mail-archive.com/qemu-devel@nongnu.org/msg1173071.html

### arm64 image-to-rootfs prior art (relevant to any target)
- **libkrun** / **krunvm** — microVM-from-OCI on both KVM and HVF; direct prior
  art for the image layer. https://github.com/containers/libkrun

## Rule of thumb

If a port keeps arm64, it is a backend file. If it changes ISA, it is a second
guest architecture: a new boot protocol, a new interrupt model, and new exit
decoding, with the trait seam intact but a large body beneath it. Decide which
one you are signing up for before starting, and keep `amber-core` honest by
moving anything arm64-specific behind a clear boundary the moment a second ISA
appears.
