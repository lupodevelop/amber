# ARCHITECTURE

## Layout

A small set of crates, one direction of dependency. The VMM core knows nothing about images, pools, or the control plane. Everything above it is replaceable.

```
amber          CLI, parses commands and the manifest, talks to amberd
amberd         control plane: pool, budget, gateway, API, node
  |
  +-- amber-image    OCI pull, layer flatten, erofs base + scratch overlay
  +-- amber-snap     snapshot capture, fork, copy-on-write restore
  +-- amber-budget   the RAM coexistence scheduler
  +-- amber-gw       vsock egress gateway, default deny, audit
  |
  +-- amber-core     the VMM: memory, vcpus, device model, boot, run loop
        |
        +-- amber-hvf   Hypervisor.framework backend, Apple Silicon
        +-- amber-kvm   KVM backend, arm64 Linux, via rust-vmm
```

The same binary is both `amber` and `amberd`. With no daemon running, the CLI starts an in-process control plane for one-shot use. With `amber up`, it starts a persistent `amberd` that holds the warm pools.

## The seam that matters: the hypervisor

Because both targets are arm64, the only thing that differs between Apple Silicon and a DGX Spark is how guest memory is mapped and how a vcpu runs. Everything else is shared code above one trait. Get this boundary right and a backend is a small file.

```rust
/// Implemented by amber-hvf and amber-kvm. Both arm64.
/// Everything ABOVE this trait is written once.
pub trait Hypervisor: Send + Sync {
    type Vcpu: Vcpu;

    fn create(cfg: &VmConfig) -> Result<Self> where Self: Sized;

    /// Map a host mmap region into guest physical space.
    /// hv_vm_map vs KVM_SET_USER_MEMORY_REGION.
    fn map_memory(&self, host: &MmapRegion, ipa: GuestAddr) -> Result<()>;

    /// The GIC. hv_gic_* vs an in-kernel GICv3 via KVM_CREATE_DEVICE.
    fn create_irq_chip(&self, cfg: &GicConfig) -> Result<()>;

    fn create_vcpu(&self, id: u8) -> Result<Self::Vcpu>;

    /// Raise an IRQ line, called by device backends.
    fn inject_irq(&self, irq: u32) -> Result<()>;
}

pub trait Vcpu {
    /// arm64 boot protocol: PC at the kernel entry, x0 at the DTB address.
    fn set_boot_regs(&mut self, entry: GuestAddr, dtb: GuestAddr) -> Result<()>;

    /// The only hot call. hv_vcpu_run vs KVM_RUN. Synchronous, one thread per vcpu.
    fn run(&mut self) -> Result<VmExit>;
}

/// The common vocabulary. Each backend translates its raw exit syndrome into this.
pub enum VmExit {
    Mmio { addr: GuestAddr, op: MmioOp },  // a virtio-mmio register to emulate
    Idle,                                   // WFI/WFE, park the thread
    Shutdown,                               // PSCI SYSTEM_OFF
    Fault(ExitFault),                       // unhandled, kill the VM
}
```

The run loop, the device tree builder, the virtio-mmio dispatch, vsock, blk, and snapshot are all written once against `VmExit`. The hot loop never knows whether it is on a Mac or a Spark.

## The VMM core

`amber-core` is the smallest thing that boots an arm64 Linux guest and talks to it. No firmware.

1. Allocate guest memory as a single `vm-memory` region backed by an `mmap`.
2. Load the bundled arm64 kernel `Image` and a minimal initramfs with `linux-loader`.
3. Build a minimal device tree: memory, cpus, the timer, PSCI for vcpu power, and the virtio-mmio nodes.
4. Create vcpus, set boot regs (PC at entry, x0 at the DTB), run.

The kernel is bundled with the binary and built once, the way `libkrunfw` bundles one. A bundled kernel is what makes start time predictable.

Device model, five devices, following Firecracker's count:

- `virtio-vsock`: the control and I/O channel. Host and guest talk over this with no network.
- `virtio-blk`, read-only: the flattened image rootfs as an erofs blob.
- `virtio-blk`, writable: a small scratch disk. The guest mounts an overlay, erofs base read-only below, scratch above, so writes never touch the shared base.
- `virtio-net`, optional and off by default: present only if a template opts in, and wired to the gateway, never a host bridge.
- serial console, debug builds only.

virtio-mmio over virtio-pci to start, because it is simpler and is what Firecracker uses on arm64.

## Image layer

`amber-image` turns an OCI reference into a bootable rootfs: pull (manifest, config, layers) with an `oci-client` style crate, cache content-addressed, flatten layers honoring whiteouts, pack the tree into a read-only erofs blob, and allocate a thin writable scratch disk per VM.

The flatten-and-pack step is Linux-centric in existing tooling (buildah, umoci), and there is no buildah on a Mac host. Two routes, decided in `ROADMAP.md`: carry a tiny builder VM that does the flatten on first use, or do the flatten and erofs packing in pure Rust. The pure-Rust route keeps the single-binary promise but the erofs writer support is immature. The builder-VM route ships sooner and breaks the single-binary promise on first run.

## Snapshot and fork

`amber-snap` is where cold start dies, and where the name lives in the code.

A snapshot, an amber, is vcpu register state plus the guest memory image captured at runtime ready: booted, overlay mounted, interpreter or toolchain initialized, user code not yet run. One amber per template, cast once.

A fork casts a new VM from an amber: map the snapshot memory copy-on-write, restore the vcpu state, resume. The guest resumes at the frozen instant, in single-digit milliseconds, no boot and no init. Every invocation is a fork. Forks of one amber share base pages until they write, so concurrent invocations of one template are page-deduplicated as a property of the fork model, not a separate pass. Mechanics in `MEMORY.md`.

## Control plane

`amberd` owns everything stateful: the warm pools, the budget scheduler, the gateway, the node interface, and the API socket.

```
            agent (MCP)        developer (CLI)        peer node
                 \                 |                   /
                  \                |                  /
                   +-----------------------------------+
                   |              amberd               |
                   |   API  pool  budget  gateway  node|
                   +-----------------------------------+
                      /            |             \
                  [vm]           [vm]           [vm]   cast from amber
                  core           core           core
                     \            |             /
                       Hypervisor (HVF / KVM)
```

The pool holds N ready ambers per template per the manifest. On a request, the scheduler checks the budget, casts from a warm amber if available or boots and ambers a new one if not, runs the invocation, and discards or resets per policy. The `node` interface is what makes the optional homelab work, see `CLUSTER.md`.

## Data flow of one invocation

1. Caller sends `RunOneShot(template, argv, stdin)` to the API.
2. `amber-budget` checks the fleet RAM budget. If admitting would breach it, reclaim from idle pooled VMs, else reject with a structured budget error.
3. Cast a VM from the template amber, or take a warm one from the pool.
4. argv, env, stdin, and any code files go to the guest over vsock into the scratch overlay.
5. The guest runs. stdout, stderr, exit status stream back over vsock.
6. The VM is discarded, or reset and returned to the pool if the template allows reuse.
