# DEVLOG

Observations, discoveries, difficulties, and choices made while building amber.
Chronological by milestone. The goal is that the next person (or the next me)
does not re-learn the same things the hard way. Facts here were true when
written; verify against the code before trusting a detail.

Target host throughout: Apple Silicon, macOS, arm64, Hypervisor.framework (HVF).
Guest for bring-up: the Alpine `virt` arm64 kernel + busybox (borrowed, not yet
bundled).

---

## M0 — boot to userspace

The skeleton shipped a correct-by-construction HVF backend that had never been
compiled. The FFI constants and the `vm-fdt` API — the two things STATUS.md
flagged as risky — both turned out **correct** when checked against the real SDK
headers. Every real bug was in *behavior*, found by reading the guest fault, not
by guessing.

**Three HVF bring-up bugs, each blocking boot:**

1. **vcpu starts at EL0t, not EL1.** `set_boot_regs` set PC and x0–x3 but not
   PSTATE. A fresh HVF vcpu comes up at EL0t, so the kernel's first instructions
   ran unprivileged and trapped to a zero VBAR → fault at PC=0x400 with no output.
   Fix: set CPSR to `0x3c5` (EL1h, DAIF masked) — the arm64 Linux entry state.
   The skeleton's comment claimed HVF starts at EL1; it does not.

2. **HVF advances PC past an HVC; we advanced it again.** The PSCI-over-HVC
   handler did `PC += 4`, but for an HVC exit HVF already reports PC at the next
   instruction. The extra step skipped the instruction that loaded the result
   pointer → NULL deref at the PSCI probe. Fix: don't step PC for HVC. (Contrast:
   for a trapped sysreg / data abort, ELR *is* the faulting instruction, so we
   *do* step. The rule differs per exception class.)

3. **WFI arrives as an exception (EC=0x01), not an idle exit.** The run loop
   expected a CANCELED/VTIMER exit; HVF delivered EC=0x01. Misclassified as fatal.
   Fix: map EC=0x01 to Idle.

**Other M0 notes:**

- Unmodeled sysreg traps (EC=0x18) made inert (reads→0, writes dropped) so boot
  reaches its ceiling instead of faulting. This zeroed `CNTVCT`, which is why M0
  timestamps were all `[0.000000]` — a clue that paid off at M0.5.
- `run()` was refactored recurse→loop: HVC/sysreg handling resumes in place, and
  a busy boot does enough of them to overflow a recursing stack.
- The guest booted all the way to the initramfs `/init` and WFI'd — M0 exceeded
  its "earlycon only" goal.

**Workflow gotcha that bit repeatedly:** every `cargo build` invalidates the code
signature, and HVF refuses (`hv_vm_create` → `0xfae94007` = `HV_DENIED`) without
the `com.apple.security.hypervisor` entitlement. **Re-`codesign` after every
build.** This cost a debugging detour at least once.

---

## M0.5 — GIC + timer + interactive console

**Native vGIC (`hv_gic_*`, macOS 15+):**

- Creation order is strict: after `hv_vm_create`, **before** any vcpu (the GIC
  allocates per-vcpu CPU-interface state). Set vcpu `MPIDR_EL1` affinity (vcpu 0 →
  `0x80000000`) or the kernel can't find its redistributor.
- The redistributor region HVF reports is **huge** (`0x2000000` = 32 MiB) — sized
  for the max cpu count (256), not our 1 vcpu. It does not fit the 16 MiB hole
  below the PL011. Placed it in the wide gap *above* the PL011 and below RAM.
- **The big surprise:** with the vGIC, HVF handles **WFI internally** — it parks
  the vcpu and wakes it on a GIC interrupt, and **never exits to us** for WFI.
  Zero WFI exits at the shell prompt was the tell. This invalidated the first
  console design (poll stdin during the Idle exit — the Idle exit never comes).

**Live clock:** once the timer sysregs were no longer trapped/zeroed and the GIC
existed, `CNTVCT` returned real time and timestamps advanced (0 → 5s). The
`arch_timer` showed up at GICv3 INTID 27.

**Interactive console — the hardest part of M0.5.** Three sub-problems:

1. **`ttyAMA0` would not register.** The `amba-pl011` driver binds via the AMBA
   **PrimeCell ID** registers (0xFE0–0xFFC) — our PL011 read them as 0, so no
   match. It also needs an **`apb_pclk` clock** in the DTB (`amba_device_add`
   calls `clk_get(dev,"apb_pclk")` and *defers forever* without it), plus
   `uartclk`. Adding the PrimeCell IDs + a `fixed-clock` node fixed binding.
   Symptom before the fix: `Serial: AMBA PL011 UART driver` printed but no
   `ttyAMA0 at MMIO ...` line, and `Warning: unable to open an initial console`.

2. **RX needs an interrupt, delivered from another thread.** Because HVF parks
   the vcpu inside `hv_vcpu_run` on WFI (see above), console input must arrive as
   an interrupt to wake it. Design: a reader thread reads host stdin, pushes bytes
   into the PL011 RX FIFO, and raises the UART's GIC SPI (`hv_gic_set_spi`) — which
   wakes the parked vcpu. The PL011 line is level-triggered off its mask/status.
   `hv_gic_set_spi` is safe to call from another thread while the vcpu thread is
   in `hv_vcpu_run`.

3. **Input fed before the prompt is lost.** Bytes injected at boot (e.g. a piped
   file delivered at t=0) get consumed by the kernel console before the shell
   opens the tty — gone by the time the shell reads. A human typing after the
   prompt is fine; for automated tests, feed input *after* boot reaches the prompt
   (`(sleep 8; printf ...) | amber ...`).

**`^[[6n` artifact:** busybox's line editor sends `ESC[6n` (cursor-position query)
when the tty winsize is 0×0 and then *reads input looking for the `R` reply* —
swallowing piped commands. A real terminal answers it silently, so a human is
fine; for non-tty input use a `read`/`eval` loop or set the winsize first.

**Host terminal:** `amber boot` puts the host tty in raw mode (cfmakeraw,
restored on drop) so keystrokes reach the guest unprocessed; the guest tty does
the echo. No-op when stdin isn't a terminal.

**Timekeeping detail:** `CNTVCT`/`CNTV_CVAL` share `mach_absolute_time`'s timebase
(`CNTVCT == mach_absolute_time() - vtimer_offset`). `libc`'s `mach_*` are
deprecated in favor of the `mach2` crate; kept `libc` with `#[allow(deprecated)]`.

**Debugging gotcha:** `RUST_LOG=debug` on the per-exit path floods ~60k lines and
slows the vcpu ~100×, so the guest may not reach the shell within a watchdog
window. Debug-trace narrowly, or raise the log level.

---

## M1 — OCI images and `amber run`

**The kernel is modular.** The Alpine `virt` kernel ships `virtio_mmio`,
`virtio_blk`, `overlay`, `erofs`, `squashfs` as **loadable modules**, not built
in. Our custom initramfs has no modules, so the erofs+virtio-blk path needs
either module-loading or a rebuilt kernel. Decision: keep the existing kernel and
`insmod` modules from the bootstrap initramfs. `modules.dep` showed the four we
need (`virtio_mmio`, `virtio_blk`, `squashfs`, `overlay`) have **no deps** — the
virtio core is built in — so loading them in order is enough.

**squashfs over erofs, for now.** DESIGN prefers erofs, but `erofs.ko` lives in
the modloop (a squashfs we'd have to download + unsquashfs), while `squashfs.ko`
is right there in the early initramfs, and `mksquashfs` is `brew`-installable
(`mkfs.erofs` is not present by default but `erofs-utils` is brewable too). Same
ro-base + overlay architecture; erofs is a drop-in swap later. This is exactly
ROADMAP's "ship whichever keeps M1 small."

**Scratch = tmpfs in RAM.** The overlay upper/work dirs are a tmpfs, so writes
never need a writable disk → the virtio-blk device only implements **IN (read)**.
Much simpler, and fine for ephemeral run-once. Real CoW base sharing comes with
snapshot/fork (M3/M4).

**virtio-blk implementation:** virtio-mmio **version 2 (modern)**, one split
virtqueue, advertise only `VIRTIO_F_VERSION_1` (no indirect/event-idx, so the
ring handling stays simple). The device gets a raw `GuestRam` view of guest
memory (single-threaded, vcpu thread only) to read descriptor rings and data
buffers. Interrupt is level-triggered off `InterruptStatus`, driven into the GIC
by the run loop after each MMIO access (same pattern as the PL011). Worked on the
first full boot: `virtio_blk virtio0: [vda] 7936 512-byte logical blocks`,
squashfs mounted, rootfs listed.

**Passing the command in:** rather than escape argv through the kernel cmdline,
`amber run` *generates* the bootstrap `init` with the command baked in, using
POSIX single-quote escaping (`' -> '\''`). It exports the image's env (which
crosses `chroot`, since chroot inherits the environment) and runs
`chroot /newroot /bin/sh -c 'cd <wd>; exec <argv>'`. Robust against spaces and
metacharacters without cmdline parsing.

**crng ~10s stall.** A real image (python) blocks ~10s on `random: crng init
done` — the guest has no entropy source. The fix is a **virtio-rng** device
(easy, and it helps spawn latency too). Noted, not yet done.

**OCI pull notes:** sync HTTP via `ureq` (no async runtime, fits the sync VMM).
Docker Hub token flow: GET manifest → 401 with `Www-Authenticate` → GET token
from the realm → retry with bearer. Multi-arch index → pick `arm64`/`linux`.
Official images need the `library/` prefix. Layers are gzipped tars applied in
order; whiteouts (`.wh.<name>`, `.wh..wh..opq`) act on the accumulated tree.
Device/fifo nodes in layers are skipped (need root; the guest's devtmpfs provides
`/dev`).

---

## Post-M1 cycles

### rng-seed — killing the crng stall

The ~10s `random: crng init done` stall (flagged at M1) turned out to need no
device at all. The kernel reads `/chosen/rng-seed` from the DTB at early boot and
credits it as entropy via `add_bootloader_randomness`, so crng inits immediately.
Added a 64-byte `rng-seed` property filled from the host `/dev/urandom`
(best-effort; an unseeded boot only stalls, never fails). Result: `crng init
done` moved to `[0.000000]` and a python run's **guest** boot+run dropped from
~10s to ~0.18s. The remaining wall time on a cold `amber run` is host-side
pull+pack, which caching and warm pools address later — not guest boot.

This was the cheaper fix than virtio-rng; a real virtio-rng device is still worth
adding for continuous entropy, but the boot stall is no longer a reason to.

### virtio transport refactor + virtio-rng

Before growing the device model (net, vsock later), pulled the virtio-mmio
transport out of the block device into a reusable `VirtioMmio` (the register
state machine + split-virtqueue mechanics) behind a small `VirtioDevice` trait
(device id, feature bits, config space, `handle(ram, bufs)`). Block and entropy
devices now both plug in, and the VM holds a list of them at separate MMIO
windows with consecutive SPIs (device i at `BASE + i*STRIDE`, SPI `2+i`). The DTB
emits one node per device from the same list, so addresses/IRQs can't drift.

Added `RngDevice` (device id 4), filling request buffers from the host
`/dev/urandom`. Gotchas:

- The guest `virtio-rng.ko` needs `rng-core.ko` first (`Unknown symbol
  hwrng_register`) — `modules.dep` lists the dependency; load it before.
- Confirmed end to end: the guest's `hw_random/rng_current` reads `virtio_rng.0`,
  so its hardware RNG is now our device.

The refactor was validated for free: the block device kept working (still
`/dev/vda`, now device index 0) through the new transport.

### Why M2 before M3 (a deliberate call)

Investigated M3 (snapshot) first and the facts argued against doing it now:
HVF has **148** system registers in `hv_sys_reg_t` to save/restore, GIC state is
an **opaque object** (`hv_gic_state_create` → `get_size` → `get_data`), and the
guest busybox has **no `devmem`** for an easy snapshot trigger. That is a
multi-cycle, bring-up-grade effort. More importantly, snapshot's *payoff* (spawn
in ms) is realized through the warm pool, which lives in the control plane — so
snapshot has nowhere to land until templates and a pool exist. Chose M2 first:
the roadmap order, lower risk, and it makes amber a usable tool now.

### M2 slice — amber.toml manifest + run by name

Sliced M2 to the immediately-useful, zero-risk part (deferred the
daemon/socket/pool, which is scaffolding ahead of its payoff until M4 fork).
`amber run <name>` now resolves a bare arg against `amber.toml`: if a
`[template.<name>]` matches, it uses that template's `image`, `ram_cap`
(→ guest RAM), and `env` (overriding the image's by key); otherwise the arg is a
plain OCI reference. Pool/budget/timeout/snapshot fields are parsed
forward-compatibly (and `#![allow(dead_code)]`'d) but not yet acted on.

Verified: `amber run alpine` → alpine:3 with `MemTotal` ≈ 128 MiB; `amber run
pytools` → python:3.12-slim with `AMBER_TEMPLATE`/`PYTHONUNBUFFERED` set and
`MemTotal` ≈ 384 MiB. A size parser handles IEC/SI suffixes (`512MiB`, `4GiB`).

### M2 slice — amberd supervisor + control socket

**Forcing fact:** HVF is **one VM per process** (`hv_vm_create` returns no
handle — it's the process's singleton). So a single daemon *cannot* host
multiple guests in-process. amberd is therefore a **supervisor**: it owns the
unix-socket API and a VM registry, and spawns one child `amber __vm` process per
VM. This is not a workaround — it's the security model amber wanted anyway (each
VM in its own restricted host process, Firecracker-jailer style). The child
inherits the binary's hypervisor entitlement, so no extra signing.

Protocol (`proto.rs`): length-prefixed frames `[tag][len][payload]`; control
frames are JSON (`Request`/`Reply`), the `Stdout` frame is raw bytes so binary
console output passes through. `RunOneShot` spawns the child with its stdout
piped and relays it to the client frame by frame, then sends `Exit{code}`;
`List`/`Kill`/`Shutdown` manage the fleet. `Kill` finds the child by pid and
`libc::kill`s it (the registry stores pid, not the `Child`, since the relaying
thread owns the handle).

CLI: `amber up` spawns a detached `amber serve` (logs to
`amber-cache/amberd.log`) and waits for the socket; `down` sends `Shutdown`
(the daemon exits as it replies, so a dropped connection is success); `ps` →
`List`, `rm <id>` → `Kill`. `amber run` routes through the daemon when reachable,
else runs in-process (`__vm` is that same in-process worker).

Verified the whole loop: `up` → `ps` (empty) → `run alpine` (child VM, stdout
streamed back) → a backgrounded `sleep` VM shows in `ps` as `vm1/<pid>/alpine`
→ `rm vm1` kills it → `ps` empties → `down`.

Deferred (not throwaway-scaffolding yet): warm pool (pays off with M4 fork) and
interactive stdin *through* the daemon (the fiddly bidirectional relay; one-shot
stdout streaming works now).

### M2 iteration — interactive stdin through the daemon

Made `amber run` via the daemon fully interactive. The socket is now full-duplex:
a `TAG_STDIN` frame carries client keystrokes. On the client, after sending the
request, a side thread forwards `libc::read(0)` to `TAG_STDIN` frames; the main
thread keeps reading `Stdout`/`Exit`. On the daemon, `run_one_shot` pipes the
child's stdin and a side thread (on a `try_clone`'d socket) relays incoming
`TAG_STDIN` into it. The client wraps the call in `RawTerm` so keystrokes reach
the guest unbuffered.

Care taken on the framing: only one thread writes each socket direction — the
request is written *before* the stdin thread is spawned, so there's no
interleaving with frame writes. Side threads are detached and self-terminate on
disconnect/EOF; the client's blocked `read(0)` thread is reaped by process exit
on the terminal `Exit`. Verified: `(sleep 7; echo hello-via-daemon) | amber run
alpine -- sh -c 'read x; echo daemon-got:[$x]'` prints `daemon-got:[hello-via-daemon]`.

### M2 iteration — daemon audit + hardening

Ran a focused audit of the control-plane code (a code-review agent over
`daemon.rs`/`proto.rs`/`manifest.rs`/`main.rs`). It surfaced real bugs, fixed
here:

- **Orphaned VM on client disconnect (high).** The old relay only noticed a dead
  client when a stdout write failed — so a quiet/idle guest whose client was
  killed kept running forever, daemon thread blocked in `wait()`. Fix: a
  supervisor loop (`try_wait` + 50 ms poll) plus a `client_gone` flag set by
  *either* relay; the stdin relay's `read_frame` returning EOF is the reliable
  disconnect signal even when the guest is silent. On disconnect (or `rm`) the
  child is killed and reaped. Verified: killing the client of a `sleep 60` VM
  reaps the child and leaves no stray `__vm` process.
- **Leaked/blocked relay threads (high).** Threads are now joined; the socket is
  `shutdown(Both)` after the terminal reply so both relays unblock and exit.
- **Kill by raw pid → PID-reuse race (med).** `Kill`/`rm` now sets a per-VM
  `AtomicBool`; the owning supervisor (the only reaper) kills and waits. No
  signalling a pid we no longer own.
- **Unbounded frame length → OOM/DoS (high).** `read_frame` rejects frames over
  `MAX_FRAME` (16 MiB) before allocating.
- **No socket access control (high).** Socket lives in a 0700 per-uid dir, is
  chmod'd 0600, and every connection is checked with `getpeereid` — only the
  owner's euid may drive it.
- **Errors dropped the connection silently (med).** Malformed/oversized/wrong
  frames now get a structured `Reply::Error` instead of a bare disconnect.
- **`up` wasn't a real daemon (med).** `serve` is spawned with `setsid` and stdin
  `/dev/null`, so it outlives the launching shell.
- **`parse_size` accepted negatives/NaN/zero (low).** Now rejected.

Known limitation kept: the daemon and its `__vm` children use cwd-relative
`assets/` and `amber-cache/`, so amberd must run from the repo root for now —
tied to the "borrowed guest assets" debt; resolves when the kernel is bundled.

### M2 iteration — image build cache (spawn latency)

Each run used to re-pull, re-flatten, and re-pack the image. Added
`amber_image::build()`, which caches the packed squashfs base and the run config
under `bases/<config-digest>.{sqfs,json}` — keyed by the image's **content id**,
so a moved tag rebuilds but identical content is reused. A warm run skips the
layer download, flatten, and pack and goes straight to boot. Measured (alpine):
cold **3.2s** → warm **1.5s** wall. (The warm 1.5s still does the manifest fetch
to learn the digest plus boot+run; a reference→digest cache could drop the
network too, at the cost of tag-staleness — deferred deliberately.)

Audit of this change caught a real one: amberd builds VMs **concurrently**, but
the first cut flattened into a shared `rootfs` dir and wrote the base in place —
two parallel builds would clobber each other. Fixed: flatten into a per-build
temp dir (`rootfs-<id>.<pid>`), pack to a temp file, then atomic-rename into the
content-addressed base, and clean up the temp tree. Verified: alpine and
python:3.12-slim built in parallel via the daemon both succeed, two bases cached,
no leftover temp dirs.

### M2 iteration — offline warm runs (reference→id cache)

The warm path still did a registry round-trip per run (fetch the manifest to
learn the digest). Added a `reference -> content-id` cache (`amber-cache/refs.json`,
written atomically): `build(refresh=false)` (what `run` uses) takes the cached id
and a present base **without any network**; `build(refresh=true)` (what `pull`
uses) always re-resolves and updates the mapping, so a moved tag is picked up.
Docker's model: `run` uses the local cache, `pull` refreshes it.

Effect on `amber run alpine -- true` wall time: cold (build + network)
**3239 ms** → warm-with-network **~1500 ms** → **warm offline ~185 ms** (steady
~180–190 ms across repeats). The network was ~1.3 s of the old warm path. What
remains (~185 ms) is host setup + the cold microVM boot itself (~125–130 ms
guest) — which is exactly what snapshot/fork (M3/M4) is meant to remove.

### M2 iteration — measure the warm-start breakdown

Before optimizing further, measured where the ~185 ms warm start actually goes
(opt-in `AMBER_TIME=1`, phases around `build` / `Vm::prepare` / `run`). On a warm
`amber run alpine -- true`:

```text
build=0ms   prep=5–9ms   boot+run+teardown=132–173ms   total ≈ 137 ms
```

The finding redirects the effort: **host overhead is already negligible** (cache
hit ~0 ms, `prepare` ~7 ms to mmap + load the 34 MB kernel + build/gzip the
initramfs). The whole warm cost is the **guest kernel boot** itself. So the
"cheap wins" I'd listed (cache the initramfs, mmap the kernel) would shave ~7 ms
total — not worth it. The only real levers on spawn latency are the boot: a
trimmed/built-in kernel (less device probing, no `insmod`) to lower the floor,
and **snapshot/fork (M3/M4) to skip boot entirely** for single-digit-ms forks.
Measuring first stopped us from optimizing the 7 ms and missing the 130 ms.

### M2 iteration — quiet boot (halve the boot time)

The measurement pointed at the boot; the cause turned out to be cheap to fix.
The guest's boot dmesg (~150 lines) streams to the PL011 **one character per MMIO
vmexit** — thousands of round-trips that dominated the boot. Changed the default
kernel cmdline from `earlycon=pl011,0x9000000 console=ttyAMA0` to `console=ttyAMA0
quiet`: `console=` still gives the app its tty, but `quiet` suppresses the kernel
boot spam (and earlycon, the other char-per-exit firehose, is dropped). App
output is unaffected (it's not kernel printk).

Effect on warm `amber run alpine -- true`: boot+run+teardown **137 ms → ~77 ms**,
total **~81 ms** (steady). Roughly half the old boot was console vmexits.
`AMBER_VERBOSE=1` restores earlycon + full dmesg for boot debugging. Net warm
spawn this session: **~185 ms → ~81 ms** (offline cache + quiet boot), with host
overhead ~5 ms — the rest is real kernel boot, whose floor needs a trimmed kernel
or snapshot/fork to break.

### M2 iteration — `ps` age column

Small Docker-shaped polish: `VmInfo` gained a `started` (epoch seconds) stamped
when the daemon registers a VM, and `amber ps` shows an `AGE` column
(`5s`/`3m`/`2h`). Verified it grows across calls.

### M2 iteration — detached run + logs

Until now every VM was tied to a client connection, so `ps`/`rm`/`age` only ever
saw VMs with someone attached. Added `amber run -d` (detached): the daemon spawns
the VM with stdin `/dev/null` and stdout/stderr to `amber-cache/logs/<id>.log`, a
background supervisor reaps it on exit/kill, and the client gets the id back
immediately. `amber logs <id>` streams that log (works during and after the run).
Now the whole `ps`/`logs`/`rm`/`age` suite is meaningful for long-running
sandboxes. The `run` flag parsing was reworked to split `-d`/target (before `--`)
from the command argv (after it).

Audit of this iteration caught one: `amber down` exited the daemon but **orphaned
detached VMs** (their `__vm` children kept running). Fixed: `Shutdown` SIGKILLs
every registered VM before exiting. Verified: a detached `sleep 60` VM is reaped
by `down`, no stray `__vm` left.

### M2 wrap — top-level README

Wrote a top-level `README.md` documenting the implementation as it stands
(status table, build+codesign, the real CLI, `amber.toml`, the `AMBER_TIME`/
`AMBER_VERBOSE` knobs, crate layout). Smoke-tested every documented command
end-to-end (`run`, template env, `-d`, `ps`, `logs`, `rm`, `down`) so the doc
matches the code.

**M2 is functionally complete:** manifest + templates, the `amberd` supervisor
(one process per VM) over an owner-only unix socket, interactive and detached
runs with I/O relay, `ps`/`logs`/`rm`/age, an audit pass, and the image cache +
quiet boot that cut warm spawn to ~80 ms. Remaining roadmap items are either
marginal in the current design (`reload` — workers re-read the manifest already)
or need a guest control channel (`exec`, M7 vsock). The data-justified next big
lever for spawn latency is **M3 snapshot/fork** (skip the ~75 ms boot), which now
has a control plane to land in.

---

## M3 — snapshot (in progress)

### Step 1 — capture (de-risk the APIs)

Snapshot is the headline (freeze a booted runtime, fork it in ms) but the riskiest
code so far, so it's being built in steps, capture first. The unknowns were: does
`hv_gic_state_*` work, do all 112 `hv_sys_reg_t` reads succeed, and the SIMD/FP +
vtimer capture. Built the capture path and verified all of it.

The on-disk amber is a directory: `mem.bin` (raw guest RAM), `gic.bin` (the
opaque GIC-state blob from `hv_gic_state_create`→`get_size`→`get_data`),
`cpu.json` (x0–x30, PC, CPSR, FPCR/FPSR, vtimer offset, all 112 system registers
as `(id, value)`, V0–V31), and `meta.json` (memory layout). Capture is
backend-driven via new trait methods `Vcpu::capture` / `Hypervisor::capture_gic`
(amber-core owns the format and I/O; the register set is HVF-specific). Generated
the 112-entry sysreg id table from the SDK header rather than hand-listing it.

FFI note: the FP registers are a NEON vector type passed *by value* — an ABI
hazard — but `hv_vcpu_get_simd_fp_reg` takes an out-pointer, so capture sidesteps
it with `*mut [u8;16]`; the by-value `set` is a restore-time problem for step 2.

Trigger for now: `AMBER_SNAPSHOT=<dir>` captures at the first run-loop boundary
after `AMBER_SNAPSHOT_AFTER_MS` (default 2000), then stops. This fires only when
the guest exits to us (an MMIO/console access), so the test workload echoes a
counter each second; a precise/idle capture needs `hv_vcpus_exit` (deferred).
Verified: a guest printing `tick0 tick1` captured at ~1.5 s yields `cpu.json`
with 112 sysregs + 32 FP regs + a kernel-VA PC, a 126 KB `gic.bin`, and a
`mem.bin` of exactly the guest RAM size.

Open for step 2 (restore): map `mem.bin` into a fresh VM, `hv_gic_set_state`, set
all registers (including the by-value FP set), recompute the vtimer offset across
processes, and resume — verifying the guest continues (`tick2 tick3 …`) without a
reboot.

### Step 2 — restore (resume proven)

`amber restore <dir>`: build a fresh VM, load `mem.bin` into guest RAM, re-open
the disk/devices, `hv_gic_set_state` the interrupt controller, set the vcpu
registers (x0–x30, PC, CPSR/FPCR/FPSR, all the writable system registers — the
read-only ID regs refuse and are skipped), and resume instead of booting. New
`Vm::restore_from` + a `run` branch; trait methods `restore_gic` / `Vcpu::restore`.

**It works.** A no-`sleep` counter loop captured at `tick21515` restored and
continued `tick21517 → tick77229` — **55 k more iterations**, no reboot, no boot
dmesg. RAM, registers, GIC interrupt delivery (console I/O), and execution all
resume correctly from the frozen instant. This is the headline mechanism
demonstrated end to end.

Two snags found, one deferred by ABI, one open:

- **FP registers not restored.** `hv_vcpu_set_simd_fp_reg` takes the value as a
  NEON vector by value, which stable Rust can't pass over FFI (`simd_ffi` is
  nightly). Capture keeps them; restore needs a tiny C shim — deferred,
  don't-care for the shell.
- **Virtual timer doesn't re-arm after restore (open).** A `sleep` loop resumes
  one tick (the in-flight timer, already due) then hangs: newly-armed vtimer
  deadlines never fire the interrupt. Isolated with the no-`sleep` test — every
  non-timer path resumes perfectly. Recomputing the vtimer offset across the
  process boundary (capture `mach_absolute_time`, derive a new offset so `CNTVCT`
  stays continuous) and clearing the vtimer mask were necessary but not
  sufficient; `hv_gic_set_state` seems not to fully re-establish the vGIC's
  timer-wake wiring. That, plus snapshotting device-emulation state (virtio queue
  registers live in the host, not guest RAM), is step 3.

### Step 3 — the vtimer, researched

Took the advice to research before guessing further, and it was decisive. Apple's
HVF has **known open bugs** for the vGIC + vtimer (FB21649319), and QEMU's HVF
platform-vGIC support is recent and experimental. The key find was a QEMU commit
that **reverted syncing `CNTV_CTL_EL0`/`CNTV_CVAL_EL0`** on HVF "because it breaks
VM save/restore", and QEMU's current `hvf.c` confirms the model: on restore the
virtual timer is re-established **only** through `hv_vcpu_set_vtimer_offset`, and
`CNTV_CTL/CVAL` are deliberately **not** written back; with the in-kernel vGIC,
HVF owns the timer→GIC delivery and the WFI wake.

Matched that exactly and characterised the behaviour:

- Writing `CNTV_CVAL`/`CNTV_CTL` on restore → the stale compare value on the new
  timeline makes the timer fire continuously → kernel **busy-spins at ~99%**.
- Not writing them (offset-only, the QEMU model) → the guest **halts cleanly at
  ~0%** but the periodic tick does not re-arm, so a `sleep` doesn't return.
- The vtimer offset is recomputed for counter continuity (capture
  `mach_absolute_time`; `offset = now - captured_cntvct`), and the mask is cleared.

So step 3's outcome is a **characterised, research-backed limitation, not a
mystery**: on HVF after restore, non-timer execution resumes perfectly (proven:
17304→53617 ticks of a busy loop), and a timer-waiting guest halts cleanly rather
than spinning. Fully resuming the periodic tick needs either an HVF detail Apple
hasn't exposed (their own bug) or the **KVM backend (M8)**, where the timer has a
complete, documented save/restore surface (`CNTVOFF`, granular vGIC state) — the
path Firecracker uses. Device-emulation-state capture (virtio queues) remains a
separate follow-up. The headline mechanism — freeze RAM+GIC+vcpu, restore into a
fresh process, resume mid-execution — is proven.

### Step 4 — the vtimer wall, mapped to the floor

Came back to the post-restore vtimer to settle whether it is fixable on HVF or
genuinely needs KVM. Dumping the symbols in `Hypervisor.framework` turned up a
seam the opaque blob hides: HVF exports **public, per-vcpu, granular GIC register
accessors** — `hv_gic_get/set_{distributor,redistributor,icc,ich,icv}_reg` (all in
`hv_gic.h`), reaching the redistributor PPI bits and the ICH list/active-priority
registers that the `hv_gic_state` blob round-trips as one chunk. That re-opened
the question, and a long instrumented session mapped the wall precisely:

- **Restoring `CNTV_CVAL`/`CNTV_CTL` after the offset** (the M3-step-3 skip) makes
  the one in-flight timer fire after restore — a `sleep` loop snapshotted mid-tick
  prints exactly one more line — and **does not** busy-spin, because ordering the
  offset write first keeps `CVAL`'s remaining delta correct. But the periodic tick
  still dies after that one fire.
- After that fire the guest parks at a **single fixed kernel PC** (an idle WFI),
  `CPSR.I=1`. HVF handles WFI **internally** (the M0.5 finding) — it never
  surfaces as `EC_WFX` — and it will **not** wake that parked vcpu from: a directly
  injected PPI 27 (`GICR_ISPENDR0` bit 27), a vtimer mask toggle, a forced
  `hv_vcpus_exit` "kick" + PC nudge, or even a real PL011 SPI raised on its line.
  HVF wakes a parked WFI only from its **own** internal vtimer event, which is
  dormant after a restore. No public call re-arms it.
- The clean structural fix is to **trap WFI** and own the idle in userspace
  (compute the deadline from `CNTV_CVAL`, park the host thread, then advance PC and
  inject) — the same shape as the M0-era `EC_WFX` path. On HVF that needs
  `HCR_EL2.TWI`. `HV_SYS_REG_HCR_EL2` is in the sysreg enum, but
  `hv_vcpu_set_sys_reg` on it returns **`HV_UNSUPPORTED` (0xfae9400f)** — HVF does
  not let the host write it, and there is no WFI-trap knob in `hv_vcpu_config`
  either.

So the conclusion is now exhaustive, not inferred: **with HVF's in-kernel vGIC the
post-restore periodic vtimer is unreachable from any public API.** And the reason
ties the whole thing together — HVF only handles WFI internally *because* the vGIC
is present (M0, before the vGIC, delivered WFI as `EC_WFX`). So the correct
trap-and-emulate-idle fix and the **software-GIC** path are the *same* project:
drop `hv_gic`, emulate GICv3 (distributor/redistributor MMIO + the `ICC_*` sysreg
interface) ourselves, and WFI traps natively again — at which point we own idle and
timer injection and snapshot/restore/fork work on Mac. That is the next real lever
for M3/M4 here; until then the KVM backend (M8) remains the other clean path.

### Step 5 — software GIC: GICv2, not GICv3 (build started)

Started the software GIC as an opt-in backend mode (`--features swgic`,
`AMBER_GIC=sw`; default stays the in-kernel vGIC). One correction up front:
**GICv2, not GICv3.** GICv3's CPU interface is the `ICC_*` system registers, and
HVF does not trap guest GIC-sysreg access — which is the whole reason Apple added
the in-kernel vGIC in macOS 15, and which makes a software GICv3 CPU interface
unimplementable here. GICv2's CPU interface is **memory-mapped** (GICC), so every
access is a stage-2 data abort we already decode. Injection uses
`hv_vcpu_set_pending_interrupt` (the IRQ line, available since macOS 11), and the
guest reads `GICC_IAR` over MMIO to learn the INTID. This is the classic HVF
non-vGIC model, and it gives back native WFI trapping for free.

First piece: `gicv2.rs`, a pure single-CPU GICv2 — distributor (CTLR/TYPER,
IS/ICENABLER, IS/ICPENDR, IS/ICACTIVER, IPRIORITYR, ICFGR, SGIR) and CPU interface
(CTLR/PMR/IAR/EOIR/HPPIR) — modelling enable/pending/active/priority per INTID,
level vs edge (devices are level and follow their line; the timer PPI and SGIs are
edge), a PMR + running-priority preemption gate, and the IAR/EOIR acknowledge
cycle. No HVF calls in it, so it unit-tests without a hypervisor: the
enable→pend→ack→EOI cycle, PMR masking, edge-latch consumption, and priority
ordering all pass.

### Step 6 — wired into HVF, boots to a shell with a live timer

Plumbed the software GIC through the backend behind `--features swgic` +
`AMBER_GIC=sw` (default stays the in-kernel vGIC, unchanged and regression-tested).
In swgic mode `HvfVm::create` skips `hv_gic_create` and holds a shared
`Arc<Mutex<GicV2>>`; `GicInfo` gained a `kind` (V2/V3) so the DTB emits the
`arm,cortex-a15-gic` node (distributor + CPU interface) instead of `arm,gic-v3`.
The vcpu run loop does the rest:

- **Injection.** Before every `hv_vcpu_run`, raise the vcpu IRQ line via
  `hv_vcpu_set_pending_interrupt(IRQ, gic.irq_pending())` — it is auto-cleared each
  run, so it is re-evaluated each iteration.
- **GICD/GICC MMIO.** Data aborts in the GIC windows are serviced against the
  software GIC in-process and resumed (PC stepped), never surfacing to `vm.rs`.
- **The timer.** Keep HVF's vtimer masked and drive INTID 27's line ourselves from
  the guest's `CNTV_CTL` (enabled, unmasked, ISTATUS) before each entry — so a
  re-armed timer is picked up without any HVF timer event at all.
- **WFI.** With no vGIC it traps as `EC_WFX` again (the M0 behaviour); step past it
  and surface Idle, and `vm.rs` parks until the timer is due exactly as before.

Result, first boot on the software GIC: the guest comes up, `/proc/interrupts`
shows `27 GIC-0 arch_timer` and `33 GIC-0 uart-pl011` (our INTIDs, our
controller), and it powers down cleanly. And the headline check — a `sleep` loop,
the exact thing that hangs after a restore on the vGIC — runs perfectly:
`TICK 1..5` at uptimes `0.07, 1.08, 2.08, 3.08, 4.09`, real one-second gaps. The
periodic virtual timer works because we deliver it; no HVF parked-WFI is involved.

The cost is per-entry work (a `CNTV_CTL` read + a mutex + the inject syscall before
each `hv_vcpu_run`) and console latency bounded by the Idle park (≤50 ms), both
acceptable and optimisable later. The foundation the whole vtimer detour was for is
now real and on by choice.

### Step 7 — the payoff: the timer survives a restore

Capture/restore under swgic. The software GIC has no opaque host object, so its
whole state round-trips as a flat ~1.5 KB blob (`capture`/`restore` on `GicV2`)
instead of the vGIC's 126 KB `hv_gic_state`. `capture_gic`/`restore_gic` route to
it in swgic mode; the vcpu restore also writes back `CNTV_CVAL`/`CNTV_CTL` (the
vGIC path skips them to avoid the busy-spin, but here HVF's vtimer is masked and we
read the compare value ourselves, so the guest's deadline must be restored).

And it works — the thing the entire detour was for:

```text
capture at TICK 3 (mid-sleep)  →  restore  →  TICK 4,5,6,7,8,9,10,11,12,13,14,15…
```

A `sleep` loop snapshotted mid-tick restores into a fresh process and the periodic
timer **keeps firing**, one tick per second, indefinitely — where the in-kernel
vGIC delivered exactly one stale tick and then hung forever. The reason is the
whole point of the software GIC: we don't depend on any HVF-internal timer event or
a parked-WFI wake; we poll the guest's `CNTV` each entry and inject INTID 27
through a GIC whose state we fully own and restore. **M3 (snapshot/restore) is now
functionally complete on Apple Silicon without KVM**, and M4 (fork from a warm
template) is unblocked behind the same `AMBER_GIC=sw` switch.

One footgun to close later: a swgic snapshot and a vGIC snapshot have incompatible
`gic.bin` formats, and `meta.json` does not yet record which, so a restore must use
the same `AMBER_GIC` mode it was captured with. Recording the GIC kind in the
snapshot and refusing a mismatch is the obvious guard.

### Step 8 — closing the swgic debts

Three follow-ups from step 7, all done:

- **Snapshot kind guard.** `meta.json` now records `gic_kind` ("v2"/"v3"), and
  restore refuses a mismatch — `snapshot GIC is v2 but this backend is v3; restore
  with the matching AMBER_GIC` — instead of feeding a 1.5 KB software blob to
  `hv_gic_set_state` (or vice versa). Old snapshots with no recorded kind are
  accepted as before (the field is `Option`, best-effort).
- **Per-entry inject cost.** `hv_vcpu_set_pending_interrupt` is auto-cleared after
  every run, so the `false` case is a no-op — skip the syscall and only assert the
  `true` case. Halves the inject overhead on the common (no-pending) path.
- **Console latency.** The idle park was a fixed `nanosleep` capped at 50 ms, so
  software-GIC console input waited up to 50 ms (the vGIC wakes instantly via
  `hv_gic_set_spi`). Switched the park to `thread::park_timeout` and had the
  console reader `unpark` the vcpu thread the moment it raises the UART line, so
  input lands immediately; the 50 ms cap is now just a missed-wake backstop. The
  in-kernel-vGIC path is unaffected (it rarely reaches this idle at all).

Verified: a swgic snapshot still restores and keeps ticking (4→12), a cross-mode
restore is refused with the message above, and the default vGIC boot + `sleep` loop
is unchanged.

---

## M5 — RAM coexistence (in progress)

Chosen as the next milestone because the spawn half is in good shape but snapshot
(M3) and fork (M4) both hit the same HVF vtimer wall, while M5 — "the fleet stays
under a RAM budget and never starves the resident model" — is the *other half of
the thesis*, is independent of snapshot/fork, and is buildable now on HVF.

### Step 1 — budget accounting + admission control

`amberd` now enforces `[fleet].ram_budget`. Each VM is accounted at its template's
`ram_cap` (default 512 MiB); the daemon tracks the live sum and **refuses
admission** when a new VM would breach the ceiling, returning a structured
`BudgetExceeded { budget, used, requested }`. Added `amber budget` (shows
budget/used/free) and a `MEM` column to `ps`. Verified: with a 768 MiB budget and
256 MiB VMs, three are admitted, the fourth is refused with the structured error,
and `rm`-ing one frees room for the next.

Audit caught a real concurrency bug: the first cut checked `used` and inserted the
registry entry under two separate locks, so two concurrent admissions could both
pass and overcommit. Fixed with an atomic `reserve()` — the budget check and the
registry insert happen under one lock, reserving the slot (pid filled in after the
worker spawns); a failed spawn releases the reservation. Manifest file I/O stays
outside the lock.

This is admission control only — the *reclaim* levers (free-page reporting,
balloon, pool eviction) come next. Pool eviction waits for M4; the others don't.

### Step 2 — real RSS accounting

Admission counts each VM's `ram_cap` (a reservation), but the *real* footprint is
smaller — and showing that is half the coexistence story (and the DESIGN's "idle
overhead per VM" number). The daemon now samples each VM process's resident set
(via `ps -o rss=`, sampled outside the registry lock) and reports it: `ps` shows
`CAP` (reserved) next to `RSS` (real), and `amber budget` shows `reserved` vs
`real`. Verified: a `ram_cap = 256MiB` Alpine VM shows `CAP 256M / RSS 139M` —
the cap reserves 256 but the live footprint is 139 MiB (touched guest pages + the
34 MB kernel image + amber overhead). This is the measurement that the next step
(free-page reporting) will visibly shrink.

### Step 3 — virtio-balloon free-page reporting (active reclaim)

The cheapest reclaim lever (MEMORY.md), and the one buildable without an
amberd↔child control channel: it's **guest-driven**, handled entirely inside the
VM process. The guest's balloon driver, with `VIRTIO_BALLOON_F_REPORTING`,
proactively hands the host ranges of its *free* RAM on a reporting queue; the
device `madvise`s the backing host pages (macOS `MADV_FREE_REUSABLE`) so they
leave the resident set. We never inflate (`num_pages` stays 0), so the guest
keeps all its RAM — only genuinely free pages are dropped.

This needed a real device-model extension: the virtio-mmio transport was
**single-queue**, but balloon has three (inflate/deflate/reporting). Generalised
`VirtioMmio` to N queues selected by `QueueSel`, each with its own
desc/avail/used/ready state, and added `VirtioDevice::num_queues` + a queue index
to `handle`. blk and rng (one queue each) were unaffected — the block device kept
booting `/dev/vda` through the new transport.

`BalloonDevice` (device id 5) advertises only `F_REPORTING`; on the reporting
queue it `madvise`s each reported range via a new `GuestRam::host_ptr_at`. Loaded
`virtio_balloon.ko` in the bootstrap initramfs.

Demonstrated end to end: a VM that `dd`s 120 MiB into its tmpfs overlay then
deletes it: RSS **145 MiB (boot) → 264 MiB (allocated) → 52 MiB (freed)**. The
guest gave the RAM back and the host reclaimed it — real footprint shrank below
even the boot baseline (boot-time free pages were reported too). Combined with
step 2's accounting, this is the coexistence story working: cap reserves the
ceiling for admission, while the real footprint stays small and elastic.

### Step 4 — budget vs the machine

Tied the budget to the host: `amber budget` now reads `hw.memsize` and shows the
fleet ceiling against total RAM and the headroom left for the resident model —
e.g. `budget 4096 MiB of 16384 MiB machine, 12288 MiB left for model+host`. This
is the thesis made legible: the fleet is capped at a *fraction* of the machine on
purpose, and the rest is the model's. (A tagged, never-reclaimed model *region*
is the remaining refinement; today the headroom is implicit in `budget <
machine`.)

**M5 on HVF, achievable levers, done:** admission control, real-RSS accounting,
and free-page reporting (passive reclaim). The two remaining levers need other
milestones: active balloon *under pressure* needs an amberd↔child control channel
(M7-shaped), and pool eviction needs the warm pool (M4, which is gated on the
snapshot timer / KVM).

### Step 5 — active balloon + the control channel

Built the amberd↔child control channel — the piece M5's host-driven reclaim and
M7's `exec` both need — and used it for balloon inflation. When amberd spawns a VM
it makes a `socketpair`, `dup2`s the child end to fd 3 (named via
`AMBER_CONTROL_FD`), keeps its own end in the registry, and drops the child's. The
worker's `Vm::run` spawns a `control_reader` thread that polls fd 3 for 8-byte LE
targets; on one it moves the balloon's target, sets the config-dirty flag, and
raises the balloon's GIC line — which wakes the guest if parked. The transport now
surfaces the config-change interrupt (bit 1 of `InterruptStatus`, cleared on ack),
and the balloon's `config` exposes `num_pages` from a shared `AtomicU64`. The
inflate queue reads the guest's handed-back 4 KiB PFNs and `madvise`s them.

`amber balloon <id> <MiB>` drives it. Demonstrated: an idle 512 MiB VM logging
`MemFree` every second sat at ~459 MB free; `amber balloon vm2 200` dropped it to
~253 MB within a second (−206 MB) — the guest inflated on command and handed the
pages to the host. So M5 now has all three reclaim levers working: admission (the
backstop), free-page reporting (passive), and the balloon (active, host-driven).

The control channel generalises: it's a length-agnostic byte pipe per VM, ready
to carry `exec`/`write_file` for M7 (which still needs a guest-side agent), not
just balloon targets.

---

## M4 — fork from a warm template (CoW)

With snapshot/restore working on the software GIC, fork became reachable. The
mechanism is copy-on-write memory: instead of allocating a fresh anonymous region
and copying the 512 MiB `mem.bin` into it (the old restore path), restore now
`mmap`s `mem.bin` `MAP_PRIVATE` (`GuestMemory::from_snapshot_cow`). Reads come from
the file's page cache — shared physically across every fork of the same template —
and only the pages a fork writes fault a private copy. No up-front copy, and the
read-only base (kernel text, busybox, the booted page cache) is shared.

It works, and the numbers are the point:

- **Restore is correct over CoW** — a swgic template still resumes and keeps
  ticking (4→11), so the lazy file mapping is transparent to HVF: `hv_vm_map` does
  not wire the region, pages fault in on demand.
- **A fork costs ~10 MiB of private RAM**, not ~140 MiB. A single forked VM's RSS
  is ~9–10 MiB of its 512 MiB guest; three independent forks of one template total
  **30 MiB** of private memory (vs ~420 MiB to boot three), each running its own
  workload. This is the coexistence thesis made literal: dozens of sandboxes can
  share one template's RAM and barely dent the budget the resident model needs.
- **Resume is ~55 ms** to first guest instruction (vs ~100 ms+ to boot a kernel to
  a shell), because the ~75 ms kernel boot is skipped entirely — the guest wakes
  exactly where the template was frozen.

The remaining ~55 ms is process spawn + `hv_vm_create`/`hv_vm_map` + the register
restore, all per-fork and inherent to the one-process-per-VM model. The lever that
takes fork into single-digit ms is a **warm pool**: keep N forks pre-spawned and
paused so a request is a handoff, not a spawn — built next, on this working CoW
fork. (Templates and forks must use `AMBER_GIC=sw`: the periodic timer only
survives a restore on the software GIC, and the snapshot's `gic_kind` guard
enforces the match.)

### Warm pool — the handoff, not the spawn

The pool moves the restore off the request's critical path. A **pause gate** in the
run loop is the enabler: after a worker finishes the costly work (CoW map + GIC +
register restore) but *before* the guest's first instruction, if it was launched
paused it writes a ready byte to its control channel and blocks for a one-byte go.
The daemon keeps a per-template pool of these pre-staged, warmed workers; a `Fork`
request pops one and just sends the go (a ~free server-side handoff), then refills
the pool in the background. If the pool is empty it stages one inline (the cold
path). Pooled forks are ordinary registry VMs — they show in `ps`, count against
the budget, and are reaped by `down`/`rm` — just parked at the gate. The control
channel carries the go byte first, then the existing 8-byte balloon targets, so one
socket serves both.

`amber fork <template>` drives it. Measured against a ~13–15 ms client round-trip
floor (`amber ps`): a **cold** fork (empty pool, restore inline) is ~20 ms; a
**warm** fork (pre-staged) is ~8–11 ms — i.e. at the floor, the restore fully
hidden. Three warm forks of one template each resume independently (tens of
thousands of output lines apiece) at ~10 MiB private RAM each. The absolute numbers
are small because CoW restore is already cheap (~10 ms server-side); the pool's
value grows with restore cost (bigger guests, colder caches), and either way the
fork is now a handoff, not a spawn.

### Warm pool — budget-aware sizing and eviction

Two follow-ups made the pool fleet-safe. **Sizing:** how many warm forks to keep
per template comes from `[fleet].pool_size` (default one); after a fork the pool
tops up to that target in the background, and staging stops early if the RAM budget
is full — so the pool is self-sizing to whatever the budget allows. **Eviction:** pooled
workers count against the budget (they hold a reservation), but they are idle and
reconstructible, so a real admission reclaims them. `reserve_with_evict` loops:
try to reserve, and on a budget miss evict one pooled worker (drop it from the
registry to free its reservation, kill its process) and retry, giving up only when
nothing poolable is left. The fork's own warming uses the plain reserve (pooled
VMs don't evict each other). This is M5's last reclaim lever — the one that was
waiting on M4 — now in place.

Demonstrated with a 1024 MiB budget and `pool_size = 1`: a fork leaves one VM
running and one warm in the pool (1024 MiB reserved, ~15 MiB real); a subsequent
real `run` finds the budget full, evicts the pooled worker
("evicted pooled VM vm2 to free budget"), and is admitted.

### Warm pool — interactive I/O, and a device-state capture it forced

`amber fork -i` attaches the terminal to the resumed guest (full-duplex), where
plain `amber fork` stays detached-to-log. The enabling trick: a pooled worker is
paused at the gate, so it emits nothing until the go byte — which means it can be
spawned with its console as **pipes amberd holds** without anything blocking while
it waits. At fork time amberd grabs those pipes; interactive relays them to the
client (the same stdout-stream + `TAG_STDIN` machinery as `RunOneShot`, factored
into a shared `stream_client`), detached drains stdout to the log. A client
disconnect kills the worker; the guest exiting closes stdout and ends the relay.

Getting input to actually reach the guest exposed a real **device-state capture**
gap (the one M3 flagged and deferred). The PL011's registers live in the *host*
emulation, not guest RAM, and restore rebuilt a fresh `Pl011` — so the interrupt
mask the guest's driver had programmed (`UARTIMSC`) was lost, and `irq_level()`
(`ris & imsc`) stayed zero, so a keystroke's RX interrupt never fired and the
guest's `read` never woke. (Output was unaffected — TX is not interrupt-gated that
way — which is why it looked like only input was broken.) Fixed by snapshotting the
PL011 config registers (`cr/imsc/ibrd/fbrd/lcr_h/ifls`) into a new `dev.json` and
restoring them; old snapshots without it default cleanly. Verified end to end: an
echo-shell template, forked interactively through the pool, echoes piped input
(`pool-input-1` → `ECHO:[pool-input-1]`) back to the client.

### Device-state capture — virtio queues too

Closed the other half of the device-state gap: virtio-mmio. Each device's host-side
state — `status`, `interrupt_status`, and per virtqueue the ring addresses
(`desc`/`avail`/`used`), the `ready` flag, and the host's consumed index
(`last_avail`) — lives in the `VirtioMmio`, not guest RAM, so a fresh device after
restore forgets where the rings are; a post-restore queue kick would hang. Added
`VirtioMmio::capture`/`restore` and a `virtio: Vec<VirtioDevState>` in `dev.json`,
captured in creation order and reapplied after the devices are re-attached to guest
RAM. Older snapshots without it default to the reset state.

Verified with the path that actually exercises a queue past the page cache: an
eval-shell template captured 3 devices (blk/rng/balloon, 1/1/3 queues, ring at
`0x437bc000`), and after an interactive fork a raw `dd if=/dev/vda bs=512 count=2`
read 1024 bytes through the blk virtqueue — which would have hung against a
reset queue. With this, snapshot/restore captures the full host-side device state
(PL011 + virtio), so a fork resumes a faithful machine, not just its RAM and vcpu.

The big milestones this opened — snapshot/restore on HVF via the software GIC, and
CoW fork + warm pool with interactive I/O on top — are working end to end.

---

## M7 — exec: a fresh command per fork

Fork resumes a template's *frozen* workload; exec runs a *new* command in a fork.
The pieces were all in hand — warm pool, interactive relay, console I/O, exit-code
plumbing — so M7 is a thin layer: a guest **agent** plus two commands.

The agent is the template's init: `stty -echo; echo __AMBER_READY__; read -r c;
sh -c "$c"; echo __AMBER_RC__$?; <quiet printk>; poweroff -f`. `amber template
<image> <dir>` boots it on the software GIC and snapshots once it is blocked on the
`read` — a ready-to-exec template. `amber exec <template> -- <cmd>` forks that
template (warm pool), writes the command line to the agent's `read` over the
console, streams the command's stdout to the client, and parses the `__AMBER_RC__`
line for the exit code (filtering it out of the output). `stty -echo` stops the
console echoing the command back; silencing printk before `poweroff` drops the
"reboot: Power down" dmesg.

Two refinements made it clean and fast. The exit code rides back in the marker line
the client strips, so `amber exec … -- 'exit 7'` returns 7 and `false` returns 1.
And the client returns the moment it sees the marker — the command is done and its
output is flushed — dropping the connection so the relay reaps the fork, instead of
waiting for the guest to finish powering off. That cut a warm exec from ~113 ms
(poweroff-bound) to **~15 ms**.

Verified end to end: `echo`, `uname -sr` → `Linux 6.12.81-0-virt`, `echo $((21*2))`
→ `42`, `id -un; cat /etc/alpine-release` → `root` / `3.23.4`, and exit codes
1/5/7/9 — each a different command in its own warm fork, ~15 ms apiece. This is the
thesis in one command: a fresh, isolated sandbox per task, spawned in milliseconds.

### Software GIC as the default

With snapshot/restore/fork/exec all riding on the software GIC and its boot cost
measured to be a wash (warm `amber run … -- true` is ~118 ms either way), made it
the default: the `swgic` cargo feature is on by default and the runtime uses the
software GIC unless `AMBER_GIC=hw` selects the in-kernel vGIC (a
`--no-default-features` build drops it entirely). This removes a real footgun — the
features and snapshot/restore/exec now work out of the box with no `AMBER_GIC`
env, and a plain `cargo build` no longer silently produces a binary that can't
restore its own templates. Verified: a snapshot taken with no env records
`gic_kind: v2`, restores with the timer alive, and `amber exec` runs — all without
touching `AMBER_GIC`.

---

## Networking — Firecracker-shaped, pluggable backend

Firecracker gives the guest virtio-net and pumps frames to a host TAP fd; the host
kernel NATs. macOS has no TAP, so amber keeps Firecracker's *architecture* — a
virtio-net device that pumps Ethernet frames across a `NetBackend` seam — and
swaps the backend: a userspace netstack on macOS (rootless, no entitlement), a TAP
fd on Linux/KVM later. The seam means the device never changes when the host
strategy does, and the backends (smoltcp / gvproxy / vmnet / tap) are
interchangeable, one default and the rest flagged.

**Foundation (device + seam).** `NetBackend` (send a guest frame, poll for one to
deliver) plus a virtio-net `NetDevice` (id 1): feature negotiation (MAC +
VERSION_1), config-space MAC, the tx path (drain the readable descriptors, strip
the 12-byte virtio-net header, hand the frame to the backend). Receive is the
subtle half — frames arrive off the guest's notify cycle — so the device declares a
receive queue that `process` does *not* consume on notify; instead the run loop
calls `pump_rx` each iteration, which pulls a posted buffer and writes one frame
(prepending a zeroed header) per `poll_rx`. A `CaptureBackend` logging tx proved it:
the guest sees `eth0` and transmits ARP/IPv6 frames the backend receives.

**Module A — smoltcp, end to end.** A new `amber-net` crate (smoltcp behind a
default feature) implements the backend as a userspace gateway: a smoltcp
`phy::Device` bridged to two frame queues, an `Interface` at 10.0.0.1/24. `send`
feeds a guest frame and polls the stack; `poll` polls and returns whatever the
stack emitted. With the guest statically 10.0.0.2, smoltcp answers ARP and ICMP for
the gateway with no socket needed — so `ping 10.0.0.1` from the guest returns
`64 bytes from 10.0.0.1 … 0% packet loss`. That round-trip exercises the entire
path: guest tx → virtio-net → smoltcp → reply → `pump_rx` → guest rx.

**Module B — outbound TCP, via DNAT.** The obvious approach — `set_any_ip(true)`
and a socket listening on the foreign destination — does not work: smoltcp 0.12
leaves the socket in `Listen` and never accepts the guest's SYN to a non-interface
address. The fix is destination NAT. On a fresh SYN to `ext_ip:ext_port` we connect
a host `TcpStream` and open a smoltcp socket listening on the **gateway's own** IP
and a unique ephemeral port; then every guest→external TCP frame is rewritten
in-place (dst → gateway:eph, IP+TCP checksums refilled) before smoltcp sees it, and
every gateway→guest reply is rewritten back (src → ext_ip:ext_port) on the way out.
smoltcp only ever deals with its own address, so it accepts and bridges normally;
`pump_flows` shuttles bytes between each smoltcp socket and its host stream. Result:
from the guest, `GET / HTTP/1.0` to `1.1.1.1:80` over the proxy returns
`HTTP/1.1 301 Moved Permanently / Server: cloudflare` — a real connection to the
internet through a host socket. (The host connect is currently blocking with a
short timeout, and responses arrive on the run loop's ≤50 ms poll; an async wake
thread — Module D — makes both prompt.)

**Module C — DNS.** The guest needs names, not just IPs. Cleanest path: the gateway
is the resolver, so no NAT is needed — a smoltcp UDP socket bound to the gateway's
own `10.0.0.1:53` receives the guest's queries (it owns that address). Each query
is forwarded to a host resolver (`1.1.1.1:53`) over a fresh non-blocking
`UdpSocket`, matched back to the asking guest endpoint, and the reply relayed to the
guest. With the guest's `resolv.conf` pointing at `10.0.0.1`, `nslookup example.com`
returns `104.20.23.154`, and the headline check — `wget http://example.com` —
resolves the name *and* fetches the page (`<title>Example Domain</title>`, full
HTML). DNS + TCP-DNAT together: real outbound connectivity by hostname, which is
what agent tasks need.

**Module D — never block the vcpu, answer promptly.** Two rough edges from B/C: the
host `connect` blocked the vcpu thread (a dead destination froze the guest for
seconds), and host-side replies only arrived on the run loop's ≤50 ms idle poll.
Fixed both without a wake thread. The connect now runs on a throwaway thread and
hands the stream back over a channel; the flow opens its smoltcp socket immediately
(so the guest's SYN is answered) and starts forwarding once the stream lands — a
failed connect aborts the guest side. And the device reports `wants_poll()` while it
has open flows or in-flight DNS, capping the idle park at ~1 ms so replies reach the
guest in ~ms. Verified: `wget http://example.com` still returns the page, and a
`sleep` loop keeps ticking on schedule (`0s, 1s, 2s`) *while* a connect to an
unreachable `10.255.255.1` is in flight — the vcpu never stalls. (A true
edge-triggered waker would also let long-lived idle connections sleep rather than
poll at 1 ms; for amber's short sandbox tasks the adaptive poll suffices.)

With A–D the software-GIC guest has working outbound networking by hostname, fast
and non-blocking — sandboxes can fetch, install, and call APIs. The other backends
(gvproxy, vmnet, the Linux TAP for KVM) plug into the same seam when wanted.

**Auto-config.** So networking is on by simply asking for it, the bootstrap init now
configures the guest itself: gated on `eth0` existing (i.e. a virtio-net device is
present, which only happens when `AMBER_NET` is set), it brings the link up, assigns
the static `10.0.0.2/24`, sets the default route via `10.0.0.1`, and writes
`nameserver 10.0.0.1` into the chroot's `resolv.conf` — addresses matched to the
backend. So `AMBER_NET=smoltcp amber run alpine:3 -- wget http://example.com`
returns the page with no manual `ip`/`resolv.conf` in the workload.

**On by default.** Networking is now the default (like the software GIC): a plain
`amber run alpine:3 -- wget http://example.com` reaches the internet with no
`AMBER_NET` at all, matching the container-like expectation that a sandbox has a
network. `AMBER_NET=none` opts out (no device, init skips the config block),
`AMBER_NET=capture` is the bring-up logger, any other value names a backend.

**Networking through the warm pool (exec/fork).** Until now net only worked on the
boot path; a forked or `exec`'d sandbox had none, which is exactly where agent
tasks need it. The catch is the snapshot device set: a restore must recreate the
same virtio devices in the same order or the queue state misaligns. So the snapshot
now records `meta.net`, `amber template` builds with a net device (it sets
`AMBER_NET` so the template boots a configured eth0), and `restore_from` recreates
the net device when `meta.net` — with a *fresh* backend (its host-side state isn't
snapshotted), while the guest's driver state and eth0 config come back in restored
RAM. So a fork resumes with working networking immediately. Verified end to end:
`amber exec <net-template> -- wget http://example.com` returns `Example Domain`, and
`nslookup` resolves — a warm-forked sandbox, spawned in ms, with real internet.
This is the whole point: `amber exec <template> -- <command>` runs an arbitrary
networked task in a fresh, isolated, fast sandbox.

---

## Cross-cutting choices

- **Backend seam holds.** Every milestone added capability above the
  `Hypervisor`/`Vcpu` traits (timer deadline, IRQ injection, GIC info) as small
  trait methods with defaults; `amber-core` still names no hypervisor.
- **Single-threaded device model**, except the console reader thread. The vcpu
  thread owns the devices and guest-RAM views; only the PL011 is shared (a Mutex)
  because the reader feeds it. virtio-blk is touched only on the vcpu thread.
- **Borrowed guest userland.** Kernel, busybox, musl, and modules come from Alpine
  artifacts under `assets/` (gitignored). Bundling a purpose-built,
  built-in-everything kernel is the "single-binary" debt, deferred per ROADMAP.

## Open debts

- virtio-rng for continuous entropy (the boot stall itself is fixed by rng-seed).
- erofs base instead of squashfs.
- Bundled built-in kernel → drop module-loading and the Alpine dependency.
- Working dir is honored; ownership/uid in flattened layers is not (host-side
  flatten can't set it; `-all-root` at pack time).
- No vsock yet — argv/output go via cmdline-baked init and the console. The
  control channel (`Exec`, `WriteFile`) is M2+.
