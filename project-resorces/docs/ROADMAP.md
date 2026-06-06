# ROADMAP

Milestones are ordered so each one runs, not so each one waits for the next. Apple Silicon first, because it is the reason the project exists. The KVM backend comes after the single-host product holds on the Mac, and it brings the DGX Spark target and a Firecracker oracle with it.

## M0 — boot

`amber-core` plus `amber-hvf` boot a bundled arm64 kernel and a minimal initramfs to a shell on Apple Silicon. One vcpu, `virtio-vsock`, one `virtio-blk`. Serial console for bringup. No images, no pool, no API. The deliverable is a guest printing from inside a hypervisor boundary. This is where new VMMs get stuck, so it is M0 and nothing else shares it.

## M1 — images

`amber-image` pulls an OCI reference, flattens layers honoring whiteouts, packs an erofs base, allocates a scratch overlay, and the guest mounts the overlay. `amber run <image> -- <argv>` boots cold, runs once, tears down. Decide here between pure-Rust flatten-and-pack and a tiny builder VM. Ship whichever keeps M1 small, revisit for the single-binary promise later.

## M2 — control plane and manifest

`amberd` exists. The CLI talks to it over the unix socket. `amber.toml` parsing for `[fleet]` and `[template.*]`. Commands `up`, `down`, `reload`, `run`, `ps`, `rm`, `pull`. Still cold boots, no snapshots. The deliverable is the Docker-shaped workflow over real microVMs.

## M3 — snapshot and cold restore

`amber-snap` captures an amber at runtime ready and restores it from disk. `snapshot = "post-init"` works end to end. Restore is correct but not yet fast: pre-fault on macOS. The deliverable is a restore that skips boot and init.

## M4 — fork and warm pool

Fork from an amber copy-on-write, restore vcpu state, resume in single-digit milliseconds. `warm_pool = N` holds ready ambers. `RunOneShot` takes from the pool. The deliverable is the cold-start kill: invocations in milliseconds, base pages shared across concurrent forks for free. Everything a developer needs exists at M4.

## M5 — coexistence budget

`amber-budget`. Fleet `ram_budget` enforced. Free-page reporting and balloon as reclaim levers. Pool eviction under pressure. Admission control with structured `BudgetExceeded`. The model's region tagged and excluded from every lever. The deliverable is provable yielding: the fleet stays under budget and never reclaims into the model. The thesis is not proven until M5.

## M6 — gateway

`amber-gw`. `net = "none"` means no device. `net = "gateway"` wires virtio-net to the host gateway over vsock, with per-template allowlists and a connection log. The deliverable is mediated, audited, default-deny egress.

## M7 — agent integration

`amber serve --mcp`. The MCP tool surface: `sandbox.run`, `sandbox.open`, `sandbox.exec`, `sandbox.write_file`, `sandbox.read_file`, `sandbox.close`, `sandbox.list`. Streaming I/O. Runtime value injection over the control channel, with the narrow handling rules in `SECURITY.md` and no dependency on any external secrets system. The deliverable is an agent that drives sandboxes through tool calls and runs generated code safely.

## M8 — KVM backend

`amber-kvm` over rust-vmm `kvm-ioctls`. Parity with HVF on an arm64 Linux host, reusing `userfaultfd` and free-page reporting where the kernel provides them. The deliverable is cross-platform amber with one CLI, one manifest, one API, and DGX Spark as a target. On this host, diff against Firecracker as a correctness oracle.

## M9 — homelab

The `Node` trait and the `Harbor` placement function from `CLUSTER.md`. `RemoteNode` over an authenticated cross-node channel, locality-aware placement, `amber nodes`. Peer image pull and mDNS discovery as optional follow-ons. The deliverable is "run it on whichever of my boxes has room", with no consensus and no new global state.

## Parked, build only on evidence

- Userspace lazy restore on macOS, a `userfaultfd` equivalent over Mach exception ports. Only worth touching if M5 measurement shows pre-fault is actually hurting the budget on real hardware. Do not build on spec.
- Cross-VM page merging in the style of KSM. The fork model already shares base pages, so this earns its place only if a real workload shows meaningful duplication across distinct templates, which is unlikely on a personal box.
- Embeddable library mode, a C ABI and a Rust crate so others embed amber the way libkrun is embedded. This is the answer to scaling beyond a homelab: do not grow an orchestrator, be the thing one embeds. Worth doing only when someone actually wants to embed it.

## Done

amber is a product when the four numbers and the binary size from `DESIGN.md` are good on both an Apple Silicon box and a DGX Spark, under one CLI: cold spawn latency, warm fork latency, idle overhead per VM against Firecracker's sub-5-MiB bar, spawn rate, and a small binary.
