# Snapshot, restore, fork, exec

This is the part that makes a sandbox cheap: boot once, then fork the booted state
in milliseconds and run a command in it. Here is how it works.

## A snapshot is a directory

Capturing a VM writes five files:

- `mem.bin` the full guest RAM image.
- `gic.bin` the interrupt controller state (vGICv3 banks on KVM, the software
  GICv2 on HVF).
- `cpu.json` every vcpu's registers (all of them, SMP included).
- `dev.json` device state: the PL011 registers and each virtio device's queue
  positions.
- `meta.json` the configuration needed to restore: GIC kind, vcpu count, whether
  there was a net or vsock device, and the backing disk path.

`meta.json` records the GIC kind because a restore must use the matching one. A
template built by `amber template` is a software-GIC snapshot, so it restores
under `AMBER_GIC=sw` (the default).

## Capturing

Run with `AMBER_SNAPSHOT=<dir>` set. The capture happens between two vcpu runs,
when the guest is stopped and RAM and registers are consistent. Two things can
trigger it:

- A console marker. The PL011 watches the guest's output stream; when it ends with
  an armed sentinel, the capture fires. This is deterministic: the guest decides
  the exact instant by printing the marker.
- A wall-clock deadline (`AMBER_SNAPSHOT_AFTER_MS`, default 2000) as a fallback.

With SMP, the primary signals the secondaries to leave guest execution and park
with their registers captured before RAM is dumped, so the image is coherent
across all vcpus.

The software GIC matters here. On Apple Silicon the in-kernel vGIC cannot be
restored with a working timer, so amber emulates the GIC in userspace and can
therefore serialize and restore it. That is the whole reason snapshot, restore,
and fork work on macOS.

## Restoring

`amber restore <dir>` maps `mem.bin` copy-on-write (`MAP_PRIVATE` on the file),
restores the registers, GIC, and device queue positions, and resumes the vcpus
where they stopped. Because the mapping is copy-on-write, the restored VM shares
the file's pages until it writes, and only dirtied pages cost private RAM.

## Templates and fork

A **template** is a snapshot of a VM booted just up to a tiny in-guest agent. The
agent prints a readiness marker and then blocks; the marker triggers the capture,
so the snapshot is of a guest parked right before it does anything. `amber
template <image> <dir>` produces one.

A **fork** is a restore of that template. The read-only base pages are shared
through the file's page cache across every fork of the same template, so a fork
adds only its own dirtied pages, about 10 MiB in practice. The daemon keeps a
**warm pool** of pre-restored, paused forks; a fork or exec request takes one and
resumes it with a single byte on its control channel, a handoff of a few
milliseconds, instead of restoring cold.

## Exec over vsock

`amber exec <template> -- <cmd>` runs one command in a fresh fork and returns its
output and exit code. The agent baked into the template is what runs it.

When the fork resumes, the agent dials the host over `AF_VSOCK` (host CID 2,
port 1). The daemon is listening on a per-exec unix socket that the vsock device
bridges to. The wire protocol is small:

- Host to agent: `[clen u32][cmd bytes][ilen u32][input bytes]`. The input is the
  command's stdin.
- Agent to host: repeated `[tag u8][len u32][payload]` frames. Tag 1 is stdout,
  2 is stderr, 3 is the exit code (a little-endian i32).

stdout and stderr come back as distinct streams, and the exit code is a framed
value, so there is no in-band marker to misparse and binary output passes through
unchanged.

The template is captured before the agent dials, so no live vsock connection is
ever part of a snapshot. The connection only exists on the resumed fork, when the
daemon is there to answer it.

## Copy-in: testing a directory

Because the host-to-agent message carries an input blob, you can pipe a tar into
the command's stdin and unpack it in the guest:

```sh
tar -C ./myrepo -cf - . | amber exec ./box -- 'mkdir /work && tar -xf - -C /work && cd /work && run-tests'
```

The host copy is never touched; the command runs on an isolated copy inside the
VM. To get changes back out, have the command emit a patch (for example `git -C
/work diff`) rather than writing to the host.

The vsock device sizes each packet it sends to the guest's actually-posted receive
buffer, so a whole repository streams in without truncation. `amber exec` only
reads its stdin when there is input ready, so calling it without piping anything
does not block.
