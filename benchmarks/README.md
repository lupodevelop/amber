# Benchmarks

How amber is measured, and the numbers behind the claims. Every figure here is
reproducible with the harness in this directory and `scripts/scale-load-test.sh`;
nothing is hand-waved. Where a number is missing (notably KVM on real arm64
hardware) it says so rather than substituting an emulated proxy.

## What is measured

| Metric | What it is | Why it matters |
|---|---|---|
| Cold run | `amber run <image> -- cmd`: boot a microVM from an OCI image, run, power down | The from-scratch path; the upper bound on latency |
| Template build | `amber template <image> <dir>`: boot once, snapshot to disk | One-time cost per base image |
| Fork | `amber fork <template>`: CoW-restore a snapshot into a live VM | The disposable-sandbox spawn path |
| Exec | `amber exec <template> -- cmd`: fork, run one command over the vsock agent, collect exit code | The AI-agent path (run untrusted code, get output) |
| Resident/fork | RSS of a forked worker process | Density: how many forks fit before RAM binds |

## How to reproduce

```sh
# build + (macOS) codesign for the HVF entitlement
cargo build --release -p amber
codesign --entitlements amber.entitlements -f -s - target/release/amber   # macOS only

# latency: one template, timed forks/execs
./target/release/amber up &
./target/release/amber template alpine:3 /tmp/t
for i in $(seq 1 5); do /usr/bin/time -p ./target/release/amber exec /tmp/t -- echo hi; done

# density + spawn distribution + leak check
N=20 IMG=alpine:3 ./scripts/scale-load-test.sh
```

## Caveats (read before quoting a number)

- **HVF (Apple Silicon) numbers are real hardware.** They are gathered on the host
  CPU via Hypervisor.framework, not emulation.
- **KVM (Linux/arm64) has no metal numbers yet.** CI exercises the KVM backend only
  under QEMU TCG (three levels of emulation), which proves correctness, *not*
  performance. Any KVM latency/density figure must come from real arm64 hardware
  (e.g. a bare-metal arm64 instance) and is marked PENDING until then.
- Latencies are end-to-end wall time from the CLI, so they include the client round
  trip to the daemon, not just the hypervisor cost. That is the number a caller
  actually sees, which is the point.
- RAM is the ceiling. Each VM reserves its `ram_cap`; CoW fork keeps *resident*
  memory far below the cap, so density is governed by resident RSS, not the cap.

## Results

Per-run results live in `results/`, one file per machine, each stamped with chip,
OS, and the amber commit. See [`results/`](results/).
