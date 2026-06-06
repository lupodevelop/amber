# DESIGN

## Thesis

Firecracker's idea, taken local, narrowed to arm64, optimized for spawn. Seal untrusted code in a microVM that boots in milliseconds, and never let it starve the model that shares the machine.

`amber` runs on one personal arm64 box at a time: a Mac with Apple Silicon, or an arm64 Linux box like a DGX Spark. Both have unified memory, both usually have a model already resident holding most of it. The scarce resource is RAM and something else owns it. A sandbox that balloons to fit its workload evicts the model and is useless. So the central constraint is speed under a hard memory budget that belongs to someone else.

This is the same discipline as `skeg`, which keeps a vector store frugal enough to coexist with a loaded model. `amber` keeps an execution sandbox frugal enough to do the same. Different artifact, identical thesis.

## Why the name

Amber traps a specimen and preserves it: sealed, visible, intact, recoverable. That is the product. The microVM is the amber around untrusted code. The snapshot, a runtime frozen at the ready moment, is a specimen set in amber: stopped, preserved, woken identical. Every run is a fresh copy cast from that frozen template. The name points straight at the headline feature, which is spawn from a frozen snapshot.

The theme lives in the name, the story, and the internal names. It does not touch the CLI. The CLI is Docker-shaped because the rule is that there is nothing to learn.

## Why Firecracker for local, and not Firecracker

On an arm64 Linux box, Firecracker already runs with KVM. That case is solved. The reason to build `amber` is Apple Silicon, where there is no KVM and Firecracker does not run. The arm64 Linux box then comes along for free under the same binary, and doubles as a correctness oracle: on the same host you can compare `amber` to Firecracker directly.

Taking it local lets you drop the weight Firecracker carries for the datacenter: the heavy jailer for hostile multi-tenant isolation, the rate limiters, the metadata service, the REST API ceremony for managing fleets. It is your own machine. Keep the two things that matter, a minimal device model and a fast boot, and add the one feature that is the headline for local use: spawn from a warm pool. For developers and agents, spawn latency is the metric.

## arm64 only, on purpose

Restricting to arm64 deletes half of a general VMM.

- One boot protocol: the arm64 Linux `Image` protocol with a device tree. No bzImage, no PVH, no x86 paths.
- One ISA, one SIMD story: NEON.
- One interrupt controller to deal with: the GIC.
- One guest. The guest kernel and rootfs you build run on both targets. Same guest, two host backends.

The two backends, HVF on Apple Silicon and KVM on arm64 Linux, share everything except two points: how guest memory is mapped, and how a vcpu is run and its exits decoded. Boot, device tree construction, device model, snapshot, and the entire run loop are written once. The VMM shrinks to a trait with two small implementations. See `ARCHITECTURE.md`.

## One tool, two audiences

The dev tool and the agent runtime are the same program, on purpose.

A developer wants to run code they do not fully trust without handing it their home directory: a postinstall hook, a transitive dependency, a downloaded script. They want it to feel like Docker and start like a function call.

An AI agent wants the same thing for the same reason. Generated code is untrusted code. The agent needs somewhere to run `pip install` and arbitrary scripts where a mistake or an injection cannot reach the host. The requirements are identical: strong boundary, fast start, clean teardown, no standing infrastructure. So the tool is one tool. The developer drives it from a shell, the agent from an MCP tool call, and both hit the same control API and the same warm pool.

## Why a microVM, and not the lighter options

```
seccomp / landlock   <   WASM   <   microVM   <   full VM
   (shared kernel)      (no OS)    (own kernel)   (own everything)
```

A syscall filter shares the host kernel, so one kernel bug crosses it. Fine for code you mostly trust, wrong for code you do not. WASM has no operating system: cold start in microseconds, real boundary, but it cannot run an arbitrary Linux binary or `pip install`, which disqualifies it for an agent running whatever it generates. A microVM gives the guest its own kernel behind a hardware boundary while keeping the device model small enough to start in milliseconds, and it runs arbitrary binaries in any language. A full VM is the wrong end: firmware, emulated hardware, seconds to boot, hundreds of MiB. For untrusted, arbitrary code on a shared machine, the microVM is the correct point.

## Deployment, and why it is not Compose

The instinct is "Compose for microVMs". It is wrong in one way. Compose describes long-lived services that stay up together. The `amber` workload is the opposite: ephemeral, per-invocation, cast from a warm snapshot, run, discarded. So the manifest borrows Compose's declarative shape for the persistent scaffolding (base images, pool sizes, the shared gateway, the total memory budget) and pairs it with an imperative API for the ephemeral part (cast one, run, stream, tear down). The model is a fleet definition plus an on-demand invocation API, not a graph of standing services. Schema in `MANIFEST.md`.

## Defining done as a product

Not a list of features, four numbers and a size, good on both targets with one CLI:

- cold spawn latency, boot to user code
- warm fork latency, snapshot to user code
- idle overhead per VM, against Firecracker's sub-5-MiB bar
- spawn rate, sandboxes per second
- binary size, small enough to be a drop-in

When those are right on a Mac and a DGX Spark, it is a product.

## Non-goals

- Multi-host orchestration with consensus, failover, autoscaling. A flat home node set with a placement function is the ceiling. See `CLUSTER.md`. If you outgrow that, embed `amber` as a library, do not grow it into an orchestrator.
- Datacenter serverless serving. Firecracker does thousands of hostile tenants per host with a hardened jailer. `amber` does not compete there.
- A general container runtime. It consumes OCI images, it does not replace containerd or build images.
- A general VM. No firmware, no GPU passthrough, no desktop, five devices, arm64 only.

## Prior art

- `Firecracker`. KVM, Linux, hardened for datacenter serverless. The device model, the jailer concept, and the snapshot work are reference material, and on arm64 Linux it is a direct correctness oracle.
- `rust-vmm`. The shared crates `amber` is built from: `vm-memory`, `virtio-queue`, `linux-loader`, `vmm-sys-util`, `event-manager`, and `kvm-ioctls` for the Linux backend.
- `libkrun`. The closest existing thing: a library that embeds a microVM and already supports KVM and Apple Silicon HVF, boots from an OCI image, and bundles a kernel. `amber` is its own VMM rather than a wrapper because that keeps it one clean Rust binary, controlled top to bottom, with no C library and bundled kernel to fight.
- `applevisor`. A Rust wrapper over the Hypervisor.framework arm64 APIs, used so the HVF backend is not raw FFI from scratch.
- `Ignite`. Weaveworks, Firecracker with a Docker UX, now archived. A precedent and a scope warning.
- `krunvm`. Builds a microVM from an OCI image on top of `libkrun`. Direct prior art for the image-to-rootfs path.
