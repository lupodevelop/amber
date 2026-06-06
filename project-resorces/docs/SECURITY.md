# SECURITY

## Trust boundary

The host is trusted. Everything inside a VM is untrusted, because the reason a VM exists is to run code nobody vouches for: a developer's suspicious dependency, or an agent's generated script. The design assumes guest code is hostile and asks what it can reach.

## Layers, in order of reliance

1. The hypervisor. The primary boundary is the stage-2 page tables enforced by hardware through KVM or Hypervisor.framework. Guest code runs with its own kernel and cannot address host memory. This is the layer that does the real work and the reason a VM is stronger than any syscall filter.

2. The host process sandbox. The Firecracker lesson is to assume the virtualization boundary could one day be breached and to put a second wall behind it. Each VM's monitor runs in a restricted host process: on macOS a tight sandbox profile that allows only the hypervisor entitlement and the VM's own files; on Linux a seccomp allowlist plus namespaces plus landlock, in the spirit of Firecracker's jailer. A guest that somehow escapes the hypervisor lands in a process that can do almost nothing.

3. The minimal device model. Five devices. Every emulated device is attack surface, so there are as few as the job allows and none it does not need. No firmware, no PCI enumeration, no legacy controllers.

## Network: default deny

A VM with `net = "none"` has no virtio-net device. No interface to bring up, no route to misconfigure, no DNS to exfiltrate through. This is the default for every template.

A VM with `net = "gateway"` gets a virtio-net device wired only to the host gateway over vsock. There is never a bridge from a guest straight to the host network. The gateway in `amberd` is the single chokepoint: it decides which destinations are reachable, applies per-template allowlists, and logs every connection. For an agent this is the difference between code that can phone home and code that can reach exactly the hosts you allowed, with a log.

## Host filesystem

No implicit passthrough. A VM sees its erofs base, its scratch overlay, and nothing of the host unless a template declares a `mount`, which is read-only and an explicit host path. The home directory is never mounted by default. An agent running in a VM cannot read your SSH keys, your shell history, or your source tree unless you mounted them, and you would not.

Code and data enter over the control channel (`WriteFile`) into the scratch overlay, and results leave the same way (`ReadFile`). No shared host directory is required for normal use.

## Runtime values

A value that must not be baked into an image (a token, a key, a config secret) is pushed into the running guest over the control channel after the amber was cast: to a tmpfs path, or as an `Exec` env entry. Because it arrives after the snapshot, it is not captured in the amber, not on the base image, and not reachable from any other VM. It lives only in that VM's guest memory for its lifetime, and dies with the VM.

amber injects such values, it does not store or manage them. Where a value comes from is the caller's concern. A caller that wants scoped, expiring credentials provides them from whatever secrets system it already uses, and amber's only job is to deliver the value to one VM and never persist it. Keeping that boundary narrow is deliberate: amber is a sandbox, not a secrets manager.

## Snapshot integrity

Ambers are built only from trusted base images and are content-addressed. A VM forks an amber copy-on-write and cannot write back into the shared base, so one VM cannot poison the amber another VM casts from. Rebuilding a template amber is an explicit host action (`amber snapshot`), never something a guest can trigger.

## Multi-node trust

In a homelab (`CLUSTER.md`), a node is trusted only after it authenticates over the mutually-authenticated cross-node channel. A node that cannot authenticate is not in the set. The bare local socket is never exposed off-host. Each node enforces its own RAM budget and its own device defaults, so trusting a peer to run a VM does not weaken the local isolation of that VM.

## Out of scope

- Side channels. amber does not claim defense against microarchitectural side channels between a VM and a co-resident model or another VM on the same machine. On a single shared personal box this is a known limitation, stated rather than hidden. Workloads needing that guarantee belong on separate hardware.
- Nested sandboxing. A VM cannot spawn a child VM in v1. Nesting is deferred until the flat budget model is proven, because a nested VM's accounting against the fleet budget is not yet defined.
- Denial of service by a guest that refuses to balloon. The scheduler can cap and kill a VM that exceeds its `ram_cap` or `timeout`, but a guest filling its granted budget with junk is using resources it was given. The defense is a tight `ram_cap` and a short `timeout` per template, not trust in guest cooperation.
