# Apple Silicon (HVF) — M1 Pro

| Field | Value |
|---|---|
| Date | 2026-06-18 |
| Chip | Apple M1 Pro (10 cores) |
| RAM | 16 GB |
| OS | macOS 26.5.1 |
| Backend | HVF (Hypervisor.framework), software GICv2 |
| amber commit | 009d217 |
| Image | alpine:3 (cached), 512 MiB cap |

## Latency (end-to-end wall time from the CLI)

| Operation | Result |
|---|---|
| Cold run (boot image, run, power down) | ~205 ms (cold), 200–250 ms (warm cache) |
| Template build (one-time, boot + snapshot) | ~975 ms |
| **Fork (CoW restore to a live VM)** | **31–45 ms** |
| **Exec (fork + run cmd over vsock + exit code)** | **~30 ms** (5 runs: 30, 30, 31, 31, 32 ms) |

Fork/exec is ~7x faster than a cold boot — the snapshot path is the differentiator,
and the exec figure is the one that matters for the AI-agent use case (spawn a
disposable sandbox, run untrusted code, collect output).

## Density

| Field | Value |
|---|---|
| Forks held concurrently | 13 |
| Aggregate worker RSS | 213 MB |
| **Resident per fork** | **~16 MB** (cap 512 MiB) |

Resident RSS is ~3% of the cap thanks to CoW: forks share the template's pages and
only fault private copies on write. Back-of-envelope density: ~60 idle forks/GB
before RAM binds. (This is an idle figure; a fork running a real workload faults
more pages and uses more.)

## Not measured here

- KVM on real arm64 hardware — PENDING (CI only runs KVM under QEMU TCG, which is
  not a performance signal).
- Sustained throughput / forks-per-second under churn.
- Workloads heavier than `echo` (page-fault behaviour under real CPU/memory load).
