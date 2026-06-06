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
