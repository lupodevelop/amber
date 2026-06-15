# Architecture

How the pieces fit. This is the design as built, not a plan.

## The seam

`amber-core` is the VMM and names no hypervisor. It owns the boot path (kernel and
DTB into guest RAM), the device model (PL011 UART, virtio-mmio block/rng/net/
balloon/vsock), the snapshot format, and the vcpu run loop. It talks to the
hardware only through two traits, `Hypervisor` and `Vcpu` (in
`amber-core/src/hypervisor.rs`).

Two crates implement those traits:

- `amber-hvf` wraps Apple's Hypervisor.framework. macOS only.
- `amber-kvm` wraps Linux KVM through the rust-vmm `kvm-ioctls` crate. Linux only.

Each links only on its host; `amber-core` depends on neither. The `amber` binary
picks the backend at build time by target OS. The point of the seam is that the
run loop, device emulation, and snapshot code are written once and run on both.

## What differs between the backends

On KVM the kernel provides the interrupt controller (in-kernel vGICv3), the
architected timer, and PSCI. The backend is thin: it forwards register state and
device-attribute banks, nothing more.

On macOS there is no in-kernel GIC that can be saved and restored with a working
timer, so `amber-hvf` emulates a GICv2 in userspace (the "software GIC", the
default build). This is what makes snapshot, restore, and fork work on Apple
Silicon at all. It also needs one extra mechanism: a low-rate preemption thread
forces periodic vcpu exits so the emulated timer interrupt can be injected even
while the guest spins in a compute loop. KVM needs none of this; its timer is the
kernel's.

The `Hypervisor` trait carries this difference as default-no-op methods (`kick`,
`set_yield`, `capture_gic`, and so on). KVM overrides what it needs; HVF overrides
more.

## The run loop

`amber-core/src/vm.rs` is the backend-neutral loop. It loads the kernel and DTB,
creates the vcpus, applies any CPU template, locks the process down, and then runs
each vcpu until exit. An exit is one of:

- MMIO: dispatched to the PL011 or a virtio device by address. A device with no
  backing address reads as zero and drops writes.
- WFI / idle: the vcpu parks until the timer is due or a device wakes it.
- Shutdown or fault: the loop ends.

The primary vcpu runs on the calling thread; each secondary (SMP) runs on its own
thread and parks inside `Vcpu::run` until the guest brings it up with PSCI CPU_ON.
Device state lives behind a `Mutex` shared across those threads.

## The daemon

HVF allows one VM per process, and that per-process boundary is also the isolation
boundary, so amber does not host VMs inside one long-lived process. `amberd`
(`amber serve`) owns a unix control socket and a registry, and spawns one child
`amber __vm` process per VM. The client commands (`run`, `exec`, `ps`, `rm`,
`pause`, and so on) talk to it over that socket.

The daemon also keeps a warm pool: pre-staged, paused forks of a template that a
fork or exec can hand off in milliseconds instead of spawning cold. Admission
control sums each VM's reserved RAM against `[fleet].ram_budget` and refuses or
evicts to stay under it.

## Lockdown

Before the first guest instruction, each VM process drops privileges. Everything
it needs is already open (guest RAM, the disk fd, the control channel, the vsock
listener), so it gives up the ability to acquire anything new: no exec or fork,
the filesystem read-only except a snapshot destination, and no network unless the
VM actually has a net device.

The policy is platform-agnostic (`amber-core/src/lockdown.rs`); the mechanism is
per-OS. macOS uses a seatbelt profile. Linux uses a seccomp-bpf filter (deny
execve, and socket when the VM has no network) plus Landlock (confine writes to
the allowed paths), with `no_new_privs` set. If a guest escapes into the VMM, it
lands in a process that cannot spawn a shell, write outside its sandbox, or open a
socket.

## Untrusted input

The guest is treated as hostile. The code that parses guest-controlled data, the
virtio descriptor walkers (`virtio.rs`) and the vsock packet parser (`vsock.rs`),
bounds-checks every length and index against guest RAM and never trusts a
descriptor's claimed size. Those parsers are fuzzed (`fuzz/`, `make fuzz`). The
daemon's frame reader caps frame length before allocating.

## Map

```text
amber-core/src/
  vm.rs          the run loop, SMP, snapshot capture, lockdown wiring
  hypervisor.rs  the Hypervisor/Vcpu seam
  virtio.rs      virtio-mmio transport + blk/rng/balloon devices
  net.rs         virtio-net device (host side behind a NetBackend)
  vsock.rs       virtio-vsock device, bridged to host unix sockets
  bus.rs         the PL011 UART
  snapshot.rs    the on-disk snapshot format
  memory.rs      guest RAM (mmap, CoW restore, bounds-checked access)
  dtb.rs         device tree
  loader.rs      kernel image placement
  lockdown.rs    seccomp/Landlock/seatbelt policy
amber-hvf/       HVF backend + software GICv2
amber-kvm/       KVM backend + vGICv3 save/restore
amber-net/       smoltcp netstack backend
amber-image/     OCI pull, flatten, squashfs pack, cpio
amber/           CLI + amberd
```
