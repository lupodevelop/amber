# amber

A small, fast microVM monitor for arm64. Seal untrusted code in a hardware-isolated VM, freeze a ready runtime in place, and cast a fresh sealed copy in milliseconds whenever something needs to run.

`amber` is Firecracker's idea taken local and narrowed to arm64. One small static binary. It boots a microVM in milliseconds, holds a few MiB of overhead, gives RAM back the moment it stops needing it, and runs on the two machines that matter for local AI work: Apple Silicon and arm64 Linux boxes like DGX Spark.

The name is the design. In the way amber traps and preserves a specimen, visible and intact, `amber` traps untrusted code behind a hypervisor boundary and freezes a booted runtime into a snapshot you can cast from. The snapshot is the amber. Every run is a fresh copy released from it.

## Why this exists

Firecracker already runs on arm64 Linux with KVM, so on a DGX Spark a Firecracker-class monitor is a solved thing. It does not run on Apple Silicon, because macOS has no KVM. So the reason `amber` exists is the Mac, and the arm64 Linux box comes along for free with the same binary and the same CLI. Running on Linux too is also a gift in disguise: on the same host you can diff `amber` against Firecracker and use it as a correctness oracle.

Two audiences, one tool.

For developers: a drop-in local sandbox. Run a flaky dependency, a postinstall script, or an untrusted gist without trusting it with your home directory. Docker muscle memory, a stronger boundary than a container, lighter than a full VM.

For AI agents: a place to execute generated code that is isolated by hardware, not by a syscall filter you can argue your way around. The agent asks for a sandbox, runs code, reads the result, the sandbox is gone. The model and the sandbox share the machine, so `amber` is built to never starve the resident model of RAM.

The target is a personal arm64 box with unified memory and a model already loaded: a Mac Studio, a MacBook, a DGX Spark. On all of them the scarce resource is RAM and something else already owns most of it. `amber` is a guest in that house.

## The shape

A good local tool is one small binary that drops into a workflow you already have and speaks a protocol you already know. `amber` follows that rule:

- One binary, static, arm64.
- OCI images for the unit of work. Any image or `Dockerfile` you already have is a sandbox.
- Docker-shaped commands for the CLI.
- JSON over a local socket, plus an MCP tool surface, for agents.

## Quickstart, developer

```sh
amber run python:3.12-slim -- python -c "print(2 ** 10)"

amber up                       # warm the pools from amber.toml
amber run pytools -- pytest -q
amber ps
```

## Quickstart, agent

```sh
amber serve --mcp
```

The agent calls `sandbox.run`, `sandbox.exec`, `sandbox.write_file`, `sandbox.read_file`. Network is off by default. The agent cannot read the host filesystem. Secrets, if any, arrive scoped and time-limited and never touch the image or the snapshot.

## Homelab, optional

If you have more than one arm64 box at home, `amber` can place a sandbox on whichever node has room, preferring the one that already holds the warm snapshot. A flat set of nodes and a placement function. No control plane to babysit, no consensus, no Kubernetes. See `docs/CLUSTER.md`.

## What it is not

- Not Kubernetes. Single host by default, a flat node set at most. See `docs/DESIGN.md`.
- Not a datacenter serverless runtime. Firecracker owns that and is excellent at it.
- Not a general VM. Five devices, no firmware, no desktop, arm64 only.
- Not a container builder. It consumes OCI images, it does not build them.

## Defining done

`amber` is finished when four numbers are good on both an Apple Silicon box and a DGX Spark, with the same CLI: cold spawn latency, warm fork latency, idle overhead per VM (Firecracker's sub-5-MiB is the bar), and spawn rate. Plus a small binary.

## Prior art

Built on `rust-vmm`. Device model and discipline learned from `Firecracker`. Heavy overlap with `libkrun`, which already boots a microVM from an OCI image on both KVM and Apple Silicon HVF. `Ignite` proved the Docker-shaped UX and is now archived. `amber` is its own VMM rather than a wrapper because that keeps it one clean Rust binary the author controls top to bottom. See `docs/DESIGN.md`.

## Documents

- `docs/DESIGN.md` thesis, positioning, non-goals, prior art
- `docs/ARCHITECTURE.md` crate layout, the hypervisor seam, boot path
- `docs/MANIFEST.md` the manifest and the CLI
- `docs/CONTROL-API.md` control plane and agent protocol
- `docs/MEMORY.md` snapshot, fork, and the RAM budget
- `docs/CLUSTER.md` the optional flat homelab
- `docs/SECURITY.md` threat model and isolation
- `docs/ROADMAP.md` milestones

## License

TBD. Apache-2.0 to stay inside the `rust-vmm` ecosystem.
