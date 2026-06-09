# amber

A small, fast microVM monitor for arm64. Seal code in a hardware-isolated VM and
run it like a function call: spawn in milliseconds, give it the internet, run a
command, throw it away. Firecracker's idea taken local and narrowed to arm64,
optimized for spawn. Apple Silicon (Hypervisor.framework) today; arm64 Linux (KVM)
is on the roadmap.

This README is the **implementation as it stands**. The build journal (every bug,
decision, and measurement) is in [`docs/DEVLOG.md`](docs/DEVLOG.md); the network
backend design is in [`docs/networking-backends.md`](docs/networking-backends.md);
positioning lives in [`project-resorces/docs/`](project-resorces/docs/).

## What it does

- **Boots a real Linux microVM** on Apple Silicon and runs an OCI image (squashfs
  base + tmpfs overlay) with an interactive console — `amber run`.
- **Snapshots and restores** a running VM (RAM + vcpu + interrupt controller +
  device state), resuming mid-execution.
- **Forks from a warm template** copy-on-write: a new sandbox in ~10 MiB of private
  RAM, handed off from a pool in ~milliseconds.
- **Execs a fresh command per fork** — `amber exec <template> -- <cmd>` — a clean,
  isolated sandbox per task in ~15 ms.
- **Networking** out of the box: a userspace netstack gives the guest outbound
  TCP + DNS by hostname and inbound port-forwards, rootless.
- **Stays under a RAM budget**: admission control, real-RSS accounting, virtio-balloon
  reclaim (passive + active), and warm-pool eviction.

It does this on macOS without KVM by emulating the interrupt controller in
userspace (a software GICv2) — which is what makes snapshot/restore/fork/exec work
on Apple Silicon at all (the in-kernel vGIC can't restore the timer). See the
DEVLOG for the full story.

## Status

| Milestone | What works |
|-----------|------------|
| M0 / M0.5 | Boot a bundled arm64 kernel to userspace; GIC + timer; interactive PL011 console |
| M1 | `amber run <image>`: OCI pull, flatten, squashfs base + tmpfs overlay, virtio-blk/rng |
| M2 | `amber.toml` templates; `amberd` daemon over a unix socket; `ps`/`rm`/`logs`; image cache; `-d` detached |
| M3 | snapshot **capture + restore** — RAM + vcpu + GIC + PL011/virtio device state; periodic timer survives (on the software GIC) |
| M4 | **fork** from a template (CoW, ~10 MiB/fork) + **warm pool** (~ms handoff) + budget-aware sizing/eviction + interactive `fork -i` |
| M5 | RAM coexistence: fleet `ram_budget` + admission; real RSS; virtio-balloon free-page reporting **and** active reclaim; pool eviction |
| M7 | **`amber exec`** — run a fresh command in a warm fork (~15 ms), with exit codes |
| net | software GICv2 (default) unlocks it all; userspace netstack: outbound TCP+DNS by name, inbound port-forward, on by default |

Rough numbers (Apple Silicon, alpine): warm boot `amber run -- true` ~100 ms; warm
`amber exec` ~15 ms; a fork costs ~10 MiB private RAM; free-page reporting shrinks a
VM's real footprint (e.g. 264 MiB → 52 MiB after the guest frees memory).

## Build

Apple Silicon, macOS. HVF needs the hypervisor entitlement, and **every `cargo
build` invalidates the code signature**, so build and sign together — `make` does
both:

```sh
make            # cargo build --release + codesign (use this, not a bare cargo build)
```

Equivalently, by hand:

```sh
cargo build --release
codesign --entitlements amber.entitlements -s - target/release/amber
```

A guest kernel + busybox + modules are expected under `assets/` for now (borrowed
Alpine `virt` artifacts; a bundled kernel is future work).

The software GIC is the default. To build the in-kernel-vGIC-only variant (no
software GIC): `cargo build --release --no-default-features`.

## Quickstart

```sh
# 1. run a command in a fresh microVM (pulls the image the first time)
amber run alpine:3 -- echo hello

# 2. with the internet (on by default): resolve a name and fetch a page
amber run alpine:3 -- wget -qO- http://example.com

# 3. fast repeated execs: build a template once, then exec into warm forks
amber up                              # start the daemon
amber template alpine:3 ./py          # boot once, snapshot a ready-to-exec template
amber exec ./py -- 'echo $((6*7))'    # ~15 ms: a fresh sandbox runs the command -> 42
amber exec ./py -- uname -sr          # another fork, another command
amber down                            # stop the daemon (reaps all VMs)
```

## Commands

```text
amber run [-d] <image|template> [-- <argv>...]   boot a VM and run argv (interactive;
                                                  -d = detached, prints an id)
amber template <image> <dir>                     build a ready-to-exec template
amber exec <template-dir> -- <command>           run a command in a warm fork (exit code)
amber fork [-i] <template-dir>                    fork a template (-i attaches the terminal)
amber restore <snapshot-dir>                      resume a snapshot mid-execution

amber up | down                                  start / stop the amberd daemon
amber ps                                         list VMs (ID PID AGE CAP RSS IMAGE)
amber logs <id>                                  stream a detached VM's output
amber rm <id>                                    kill a VM
amber budget                                     fleet RAM: budget, reserved, real, host
amber balloon <id> <MiB>                         ask a VM to give RAM back (active reclaim)
amber pull <image>                               pre-pull/refresh an image into the cache
amber boot <kernel-Image> [initramfs] [disk]     boot a raw kernel (no OCI)
```

`amber run` routes through `amberd` when one is up (full interactive stdin/stdout),
else runs the VM in-process. Each VM is its own host process — HVF is
one-VM-per-process, which is also the isolation boundary.

## Guides

### Snapshot, restore, fork, exec

A **snapshot** is a directory (`mem.bin`, `gic.bin`, `cpu.json`, `dev.json`,
`meta.json`). Capture one by running with `AMBER_SNAPSHOT` set; restore resumes it:

```sh
AMBER_SNAPSHOT=/tmp/snap AMBER_SNAPSHOT_AFTER_MS=2500 \
  amber run alpine:3 -- sh -c 'i=0; while :; do i=$((i+1)); echo tick $i; sleep 1; done'
amber restore /tmp/snap          # continues ticking from where it froze
```

A **template** is a snapshot of a booted VM parked at a tiny agent that reads one
command, runs it, reports the exit code, and powers off. `amber exec` forks the
template (from a warm pool when available), feeds it the command, streams the
output, and returns the exit code:

```sh
amber template alpine:3 ./box
amber exec ./box -- 'cat /etc/alpine-release'   # 3.x
amber exec ./box -- 'exit 7'; echo $?           # 7
```

`amber fork <template>` hands off a forked VM detached (id + `amber logs`);
`amber fork -i <template>` attaches your terminal to it. Forks are copy-on-write:
the read-only base pages are shared, only writes cost private RAM (~10 MiB/fork).

### Networking

On by default. The guest gets `eth0` (`10.0.0.2`), a gateway/resolver at
`10.0.0.1`, and outbound TCP + DNS — `wget`, `curl`, `pip`, API calls work by
hostname. Auto-configured by the init; no setup in your workload.

```sh
amber run alpine:3 -- wget -qO- http://example.com      # just works
AMBER_NET=none amber run alpine:3 -- echo no-network    # opt out

# inbound: forward a host port to a guest port
AMBER_PORTS=8080:80 amber run alpine:3 -- <server on :80>
curl localhost:8080                                     # reaches the guest
```

Templates carry the network device, so `amber exec`/`fork` sandboxes have the
internet too. The backend is a userspace netstack (smoltcp); gvproxy / vmnet / a
Linux TAP are documented alternatives behind the same seam
([`docs/networking-backends.md`](docs/networking-backends.md)).

### Fleet & RAM budget

```sh
amber up
amber run -d alpine:3 -- sh -c 'sleep 600'   # detached
amber ps                                     # CAP (reserved) vs RSS (real)
amber budget                                 # budget / reserved / real / host
amber balloon <id> 128                       # ask the guest to reclaim toward 128 MiB
```

With `[fleet].ram_budget` set, admission refuses VMs that would breach the ceiling;
warm-pool workers are evicted first to admit a real VM. `[fleet].pool_size` sets how
many warm forks to keep per template.

## Configuration (`amber.toml`)

Loaded from the working directory.

```toml
[fleet]
ram_budget = "4GiB"     # hard ceiling for the sum of live VMs (admission control)
pool_size  = 2          # warm forks to keep per template

[template.pytools]
image   = "docker.io/library/python:3.12-slim"
ram_cap = "384MiB"      # guest RAM and the amount accounted against the budget
  [template.pytools.env]
  PYTHONUNBUFFERED = "1"
```

`amber run pytools -- ...` resolves the template by name. Schema:
[`project-resorces/docs/MANIFEST.md`](project-resorces/docs/MANIFEST.md).

## Environment knobs

| var | effect |
| --- | --- |
| `AMBER_GIC` | `sw` (default) software GICv2; `hw` the in-kernel vGIC (faster boot, no working snapshot timer) |
| `AMBER_NET` | `smoltcp` (default) userspace netstack; `none` no network; `capture` log tx frames |
| `AMBER_PORTS` | inbound forwards, `hostport:guestport,...` |
| `AMBER_SNAPSHOT` / `AMBER_SNAPSHOT_AFTER_MS` | capture a snapshot to a dir after N ms (default 2000), then stop |
| `AMBER_TIME` | print a `build / prep / boot+run` latency breakdown |
| `AMBER_VERBOSE` | restore the kernel boot dmesg (off by default — it ~doubles boot via char-per-vmexit) |

Snapshots record their GIC kind; a restore must use the matching `AMBER_GIC` (the
default `sw` matches templates built by `amber template`).

## Architecture

```text
crates/
  amber-core   backend-agnostic VMM: boot, DTB, devices (PL011, virtio-mmio/blk/rng/
               balloon, virtio-net), snapshot format, the run loop. Names no hypervisor.
  amber-hvf    Apple Silicon Hypervisor.framework backend + the software GICv2
  amber-net    host-side network backends behind a NetBackend seam (smoltcp today)
  amber-image  OCI pull, layer flatten, squashfs pack, cpio initramfs, build cache
  amber        CLI + amberd control plane (run/exec/template/fork/ps/budget/...)
```

The `Hypervisor`/`Vcpu` traits are the seam: a KVM backend (M8, arm64 Linux) is a
third crate implementing them. amberd is a supervisor — one `amber __vm` child per
VM — because HVF is one-VM-per-process; that is also the per-sandbox isolation.

## Limitations & roadmap

- **macOS only** today; the KVM backend (arm64 Linux) is M8 — needs the hardware to
  build and test. On KVM the in-kernel vGIC has a complete save/restore surface, so
  the software GIC becomes optional there.
- Borrowed Alpine kernel/modules under `assets/`; a bundled, trimmed, built-in-everything
  kernel ("single binary") is future work.
- Networking is outbound + inbound-forward over a userspace netstack; IPv6 /
  arbitrary UDP / kernel-speed need the alternative backends (documented, not built).
- Every `cargo build` invalidates the codesignature — re-codesign before running.

## License

TBD (Apache-2.0 intended, to stay inside the `rust-vmm` ecosystem).
