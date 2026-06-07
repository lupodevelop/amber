# amber

A small, fast microVM monitor for arm64. Seal untrusted code in a
hardware-isolated VM and run it like a function call. Apple Silicon (HVF) today;
arm64 Linux (KVM) is on the roadmap.

This README describes the **implementation as it stands**. The design and
positioning live in [`project-resorces/docs/`](project-resorces/docs/); the
build journal (bugs, decisions, measurements) is in [`docs/DEVLOG.md`](docs/DEVLOG.md).

## Status

| Milestone | What works |
|-----------|------------|
| M0 | Boot a bundled arm64 kernel to userspace on HVF |
| M0.5 | GICv3 + architected timer; interactive PL011 console (RX/TX) |
| M1 | `amber run <image>`: pull OCI, flatten, squashfs base + tmpfs overlay, virtio-blk, virtio-rng |
| M2 | `amber.toml` templates; `amberd` daemon over a unix socket; `run`/`ps`/`rm`/`logs`; image cache; `-d` detached |
| M3 | snapshot **capture + restore**: a VM resumes mid-execution from disk (the periodic-timer re-arm is an HVF gap — see DEVLOG; clean on KVM) |
| M5 | RAM coexistence: fleet `ram_budget` + admission control; real RSS accounting; **virtio-balloon free-page reporting** (the guest returns free RAM, the host reclaims it) |

Warm spawn is ~**80 ms** (`amber run alpine -- true`), of which host overhead is
~5 ms — the rest is the guest kernel boot. Cold (first pull) is ~3 s. A VM's real
footprint is elastic: free pages are returned to the host (e.g. 264 MiB → 52 MiB
after the guest frees memory).

## Build

Apple Silicon, macOS. HVF needs the hypervisor entitlement, so the binary must be
codesigned before running:

```sh
cargo build --release
codesign --entitlements amber.entitlements -s - target/release/amber
```

A guest kernel + busybox + modules are expected under `assets/` for now (borrowed
Alpine `virt` artifacts; a bundled kernel is future work). See `docs/DEVLOG.md`.

## Use

```sh
# one-shot: pull, run a command, stream output, tear down
amber run python:3.12-slim -- python -c 'print(2**10)'

# templates from amber.toml, by name
amber run pytools -- pytest -q

# a persistent daemon + fleet
amber up                       # start amberd (warms nothing yet)
amber run -d alpine -- sh -c 'sleep 300'   # detached, prints a VM id
amber ps                       # ID / PID / AGE / CAP / RSS / IMAGE
amber logs <id>                # the VM's captured output
amber rm <id>                  # kill it
amber budget                   # fleet RAM budget, reserved vs real, vs machine
amber down                     # stop amberd (reaps all VMs)

# snapshot (M3): capture a running VM, restore it later (resumes mid-execution)
AMBER_SNAPSHOT=/tmp/snap amber run alpine -- sh -c 'while true; do echo .; sleep 1; done'
amber restore /tmp/snap
```

`amber run` routes through `amberd` if one is up (interactive console, full
stdin/stdout), otherwise runs the VM in-process. Each VM is its own host process
(HVF is one-VM-per-process), which is also the isolation model.

Environment knobs: `AMBER_TIME=1` prints a `build/prep/boot` latency breakdown;
`AMBER_VERBOSE=1` restores the kernel boot dmesg (off by default — it streams one
char per vmexit and roughly doubles boot time).

### amber.toml

```toml
[fleet]
ram_budget = "4GiB"

[template.pytools]
image   = "docker.io/library/python:3.12-slim"
ram_cap = "384MiB"
  [template.pytools.env]
  PYTHONUNBUFFERED = "1"
```

`ram_cap` and `env` are applied today; pool/budget/timeout fields are parsed for
forward compatibility. Schema: [`project-resorces/docs/MANIFEST.md`](project-resorces/docs/MANIFEST.md).

## Layout

```
crates/
  amber-core    backend-agnostic VMM: boot, DTB, devices (PL011, virtio-mmio/blk/rng), run loop
  amber-hvf     Apple Silicon Hypervisor.framework backend
  amber-image   OCI pull, layer flatten, squashfs pack, cpio initramfs, build cache
  amber         CLI + amberd control plane (run/up/down/ps/rm/logs)
```

## License

TBD (Apache-2.0 intended, to stay inside the `rust-vmm` ecosystem).
