# amber

A small, fast microVM monitor for arm64. It puts code in a hardware-isolated VM
and runs it like a function call: spawn in milliseconds, optionally give it the
network, run a command, throw it away. It takes Firecracker's idea local, narrowed
to arm64 and optimized for spawn time. Two backends sit behind one core: Apple
Silicon (Hypervisor.framework) and arm64 Linux (KVM).

This README describes the implementation as it stands. The network backend design
is in [`docs/networking-backends.md`](docs/networking-backends.md). For how the
pieces fit together, see [`docs/architecture.md`](docs/architecture.md) and
[`docs/snapshot-and-exec.md`](docs/snapshot-and-exec.md).

## What it does

- **Boots a real Linux microVM** and runs an OCI image (squashfs base plus tmpfs
  overlay) with an interactive console, via `amber run`.
- **Snapshots and restores** a running VM (RAM, vcpu, interrupt controller, device
  state), resuming mid-execution.
- **Forks from a warm template** copy-on-write: a new sandbox in about 10 MiB of
  private RAM, handed off from a pool in milliseconds.
- **Execs a fresh command per fork** with `amber exec <template> -- <cmd>`: a
  clean, isolated sandbox per task in about 15 ms.
- **Networks by default**: a userspace netstack gives the guest outbound TCP and
  DNS by hostname, plus inbound port-forwards, rootless.
- **Stays under a RAM budget**: admission control, real-RSS accounting,
  virtio-balloon reclaim (passive and active), and warm-pool eviction.

On macOS there is no KVM, so amber emulates the interrupt controller in userspace
(a software GICv2). That is what makes snapshot, restore, fork, and exec work on
Apple Silicon: the in-kernel vGIC cannot restore the timer.

## Status

| Capability | What works |
| --- | --- |
| **Run** | boot an OCI image (squashfs base plus tmpfs overlay), interactive console, virtio-blk/rng; SMP up to 8 vcpus (PSCI CPU_ON through the software GIC) |
| **Storage** | read-only squashfs root plus ephemeral tmpfs overlay; persistent ext4 data disks (`amber disk create`, `AMBER_DISK=img[:ro]`) auto-mounted at `/data` |
| **Networking** | outbound TCP and DNS by hostname, inbound port-forward; on by default, rootless |
| **vsock** | guest↔host `AF_VSOCK` streams bridged to host unix sockets (Firecracker-hybrid), credit flow-controlled; `AMBER_VSOCK` |
| **Snapshot / restore** | capture and resume a VM mid-execution: RAM, every vcpu (SMP included), interrupt controller, and PL011/virtio device state; the periodic timer survives |
| **Fork** | copy-on-write from a template (about 10 MiB/fork), warm pool (millisecond handoff), budget-aware sizing and eviction; interactive `fork -i` |
| **Exec** | `amber exec <template> -- <cmd>` runs a fresh command in a warm fork (about 15 ms), with exit codes |
| **RAM budget** | fleet ceiling and admission control, real-RSS accounting, virtio-balloon reclaim (passive and active), pool eviction |
| **VMM lockdown** | the VM process drops privileges before the guest runs: no exec/fork, filesystem read-only, network only if the VM has a net device |
| **Daemon** | `amberd` over a unix socket; `ps`/`logs`/`pause`/`resume`/`rm`/`budget`; templates and budget from `amber.toml` |

Rough numbers (Apple Silicon, resin kernel, alpine:3 image): a warm `amber run --
true` is about 100 ms; a warm `amber exec` is about 15 ms; a fork costs about
10 MiB of private RAM; free-page reporting shrinks a VM's real footprint (for
example 264 MiB → 52 MiB after the guest frees memory).

## Install

Prebuilt, for an arm64 host (Apple Silicon, or arm64 Linux with `/dev/kvm`):

```sh
curl -fsSL https://raw.githubusercontent.com/lupodevelop/amber/main/scripts/install.sh | sh
```

This downloads the latest release, unpacks it to `~/.local/lib/amber`, and symlinks
`amber` onto your `PATH` (`PREFIX=/usr/local` for a system-wide install). On macOS it
also clears the download quarantine. The binary finds its assets next to itself, so
`amber run alpine:3 -- echo hello` works from any directory.

amber keeps pulled images and templates under `~/.cache/amber` (override with
`AMBER_CACHE`). To build and install from source instead, see Build below, then run
`make install`.

## Build

Apple Silicon, macOS. HVF needs the hypervisor entitlement, and **every `cargo
build` invalidates the code signature**, so build and sign together. `make` does
both:

```sh
make            # cargo build --release + codesign (use this, not a bare cargo build)
```

By hand:

```sh
cargo build --release
codesign --entitlements amber.entitlements -s - target/release/amber
```

On **arm64 Linux** there is no codesigning: `cargo build --release -p amber`
links the KVM backend, and the host needs `/dev/kvm` (your user in the `kvm`
group). `make check-linux` builds, lints, tests, and runs the lockdown probe in an
arm64 Linux container, so the Linux paths can be checked from a Mac.

amber boots its own guest kernel, **resin**: a trimmed arm64 kernel with
everything amber needs built in (no loadable modules; 20M, boots in about 60 ms).
The defconfig is committed, and the build compiles upstream kernel source in an
arm64-native Docker container. The minimal userland (busybox and musl) is a
borrowed Alpine artifact, not redistributed here:

```sh
brew install squashfs        # one-time: amber packs OCI images with mksquashfs
./scripts/fetch-assets.sh    # busybox + musl into assets/
make kernel                  # build resin into assets/Image (Docker required)
```

(`./scripts/fetch-assets.sh --alpine-kernel` fetches the borrowed modular Alpine
`virt` kernel and modules instead. That is the pre-resin setup, kept as a fallback.
The same amber binary boots either; it loads modules only if a modules dir exists.)

The software GIC is the default. To build the in-kernel-vGIC-only variant (no
software GIC): `cargo build --release --no-default-features`.

## Quickstart

```sh
# 1. run a command in a fresh microVM (pulls the image the first time)
amber run alpine:3 -- echo hello

# 2. with the network (on by default): resolve a name and fetch a page
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
amber ps                                         list VMs (ID PID STATE AGE CAP RSS IMAGE)
amber logs <id>                                  stream a detached VM's output
amber pause <id> | resume <id>                   freeze / unfreeze a running VM in place
amber rm <id>                                    kill a VM
amber budget                                     fleet RAM: budget, reserved, real, host
amber balloon <id> <MiB>                         ask a VM to give RAM back (active reclaim)
amber pull <image>                               pre-pull/refresh an image into the cache
amber boot <kernel-Image> [initramfs] [disk]     boot a raw kernel (no OCI)
```

`amber run` routes through `amberd` when one is up (full interactive
stdin/stdout), otherwise it runs the VM in-process. Each VM is its own host
process: HVF is one-VM-per-process, which is also the isolation boundary.

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

`amber fork <template>` hands off a forked VM detached (id plus `amber logs`);
`amber fork -i <template>` attaches your terminal to it. Forks are copy-on-write:
the read-only base pages are shared, only writes cost private RAM (about 10 MiB
per fork). The full mechanism is in
[`docs/snapshot-and-exec.md`](docs/snapshot-and-exec.md).

### Networking

On by default. The guest gets `eth0` (`10.0.0.2`), a gateway and resolver at
`10.0.0.1`, and outbound TCP and DNS, so `wget`, `curl`, `pip`, and API calls work
by hostname. The init configures it; there is no setup in your workload.

```sh
amber run alpine:3 -- wget -qO- http://example.com      # just works
AMBER_NET=none amber run alpine:3 -- echo no-network    # opt out

# inbound: forward a host port to a guest port
AMBER_PORTS=8080:80 amber run alpine:3 -- <server on :80>
curl localhost:8080                                     # reaches the guest
```

Templates carry the network device, so `amber exec` and `fork` sandboxes have the
network too. The backend is a userspace netstack (smoltcp); gvproxy, vmnet, and a
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

With `[fleet].ram_budget` set, admission refuses VMs that would breach the
ceiling, and warm-pool workers are evicted first to admit a real VM.
`[fleet].pool_size` sets how many warm forks to keep per template.

## Configuration (`amber.toml`)

Loaded from the working directory.

```toml
[fleet]
ram_budget = "4GiB"     # hard ceiling for the sum of live VMs (admission control)
pool_size  = 2          # warm forks to keep per template

[template.pytools]
image    = "docker.io/library/python:3.12-slim"
ram_cap  = "384MiB"     # guest RAM and the amount accounted against the budget
vcpus    = 2            # guest CPUs (default 1)
disk_bps = "50MiB"      # optional I/O rate caps (token bucket, 1s burst)
net_bps  = "10MiB"
  [template.pytools.env]
  PYTHONUNBUFFERED = "1"
```

`amber run pytools -- ...` resolves the template by name.

## Environment knobs

| var | effect |
| --- | --- |
| `AMBER_GIC` | `sw` (default) software GICv2; `hw` the in-kernel vGIC (faster boot, no working snapshot timer) |
| `AMBER_NET` | `smoltcp` (default) userspace netstack; `none` no network; `capture` log tx frames |
| `AMBER_PORTS` | inbound forwards, `hostport:guestport,...` |
| `AMBER_VCPUS` | guest CPUs (1–8, default 1); secondaries boot via PSCI CPU_ON, one host thread each |
| `AMBER_DISK` | data disk image(s), comma-separated, `path[:ro]`; ext4 images auto-mount at `/data` (make one with `amber disk create <path> <size>`) |
| `AMBER_VSOCK` | guest↔host vsock channel over a host unix socket (guest gets CID 3); guest dials CID 2, host side is `<sock>_<port>`, Firecracker-hybrid |
| `AMBER_DISK_BPS` / `AMBER_NET_BPS` | I/O rate caps in bytes/s (`K`/`M`/`G` suffix), token-bucket with 1 s burst; unset = unlimited |
| `AMBER_SNAPSHOT` / `AMBER_SNAPSHOT_AFTER_MS` | capture a snapshot to a dir after N ms (default 2000), then stop |
| `AMBER_TIME` | print a `build / prep / boot+run` latency breakdown |
| `AMBER_VERBOSE` | restore the kernel boot dmesg (off by default; it roughly doubles boot via char-per-vmexit) |

Snapshots record their GIC kind; a restore must use the matching `AMBER_GIC` (the
default `sw` matches templates built by `amber template`).

## Architecture

```text
crates/
  amber-core   backend-agnostic VMM: boot, DTB, devices (PL011, virtio-mmio/blk/rng/
               balloon, virtio-net, vsock), snapshot format, the run loop. Names no hypervisor.
  amber-hvf    Apple Silicon Hypervisor.framework backend + the software GICv2
  amber-kvm    Linux/KVM backend (in-kernel vGICv3, arch timer, PSCI), via kvm-ioctls
  amber-net    host-side network backends behind a NetBackend seam (smoltcp today)
  amber-image  OCI pull, layer flatten, squashfs pack, cpio initramfs, build cache
  amber        CLI + amberd control plane (run/exec/template/fork/ps/budget/...)
```

The `Hypervisor`/`Vcpu` traits are the seam, with two backends behind it:
`amber-hvf` (Apple HVF, macOS) and `amber-kvm` (Linux/KVM, via the rust-vmm
`kvm-ioctls` crate). Each links only on its host; `amber-core` names neither. On
KVM the interrupt controller (in-kernel vGICv3), the arch timer, and PSCI are the
kernel's, so that backend is thin: no software GIC, no timer-preemption thread.
amberd is a supervisor with one `amber __vm` child per VM, because HVF is
one-VM-per-process; that is also the per-sandbox isolation. A fuller walkthrough
is in [`docs/architecture.md`](docs/architecture.md).

Each VM process also **locks itself down** before the first guest instruction.
Everything it needs is already open (guest RAM, disk fd, control channel,
listeners), so it drops the ability to acquire more: no exec or fork, the
filesystem turns read-only (except a snapshot destination), and the network is
denied unless the VM has a net device. The policy is platform-agnostic
(`amber-core::lockdown`); the mechanism is per-OS (macOS seatbelt; Linux
seccomp-bpf plus Landlock). A guest escape into the VMM lands in a process that
cannot spawn, drop files, or phone home.

## Limitations & roadmap

- **macOS/HVF and Linux/KVM**, arm64. Both backends run the full pipeline: run,
  exec, fork, snapshot, restore, SMP, and pause. The KVM path is tested
  hardware-free with `scripts/kvm-*.sh` (QEMU emulates EL2 under TCG, a KVM-host
  Linux boots, and amber-kvm boots resin inside it), covering boot,
  snapshot/restore, SMP teardown, and pause/resume; these run nightly in CI. It
  has not yet been validated on real arm64 KVM hardware.
- The kernel is amber's own (resin), but busybox and musl are still borrowed
  Alpine artifacts and `assets/` ships separately; bundling everything into the
  amber binary ("single binary") is future work.
- Networking is outbound plus inbound-forward over a userspace netstack; IPv6,
  arbitrary UDP, and kernel-speed throughput need the alternative backends
  (documented, not built).
- Every `cargo build` invalidates the code signature; re-codesign before running.

## License

Undecided (Apache-2.0 intended, to stay inside the `rust-vmm` ecosystem).
