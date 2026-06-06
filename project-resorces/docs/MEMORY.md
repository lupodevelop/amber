# MEMORY

Booting a microVM is solved. Booting one fast, on a machine where a model already owns most of the RAM, is the part that takes care. This document is about spawn speed and about not evicting the model. No more than that.

## The constraint

The target is a personal arm64 box with unified memory and a model resident, holding most of it. Two rules, everything else follows:

1. The fleet of VMs stays under a declared RAM budget that is a fraction of the machine.
2. The model's resident region is never a reclaim target. The sandbox yields to the model, never the reverse.

A microVM tool that does not treat these as first-class is the wrong tool for this machine, regardless of how fast it boots in isolation.

## Snapshot and fork

An amber is vcpu register state plus a guest memory image captured at runtime ready: booted, overlay mounted, interpreter or toolchain initialized, user code not yet run. One amber per template, cast once.

A fork maps the amber memory copy-on-write into a new VM and restores the vcpu state. The guest resumes at the frozen instant. No boot, no init. Target is single-digit milliseconds to user code, against the roughly 125 ms a cold microVM boot costs.

Forks of one amber share base pages until they write. So concurrent invocations of the same template are page-deduplicated as a property of the fork model itself, not a separate pass. This is why amber forks many VMs from one amber rather than booting many VMs and merging their pages after the fact. The dedup is structural.

## Where the two backends differ

The fork model needs two mechanics: copy-on-write of the snapshot pages, and a way to avoid materializing the whole snapshot before the VM runs. KVM on Linux gets both from the kernel. Apple Silicon does not have the same kernel features, so the macOS path is different. This is the one place the memory story is not shared between backends.

On arm64 Linux, with KVM:
- copy-on-write from `MAP_PRIVATE` mappings of the amber file.
- lazy restore from `userfaultfd`: register the guest memory, the kernel faults to a userspace handler the first time the guest touches an absent page, the handler installs it. The prefetch refinement is to pull the working set in one go instead of one page at a time.
- this path is also where Firecracker on the same host is a useful reference for correctness.

On Apple Silicon, with HVF:
- copy-on-write still works through a private file mapping, the kernel honors copy-on-write on private mappings.
- there is no `userfaultfd`. The v1 answer is simple and correct: pre-fault the amber on cast. Map it, touch it, run. It spends the amber's RAM up front, which is fine for the small ambers a single laptop holds, and it ships.

That is the whole macOS memory story for v1. Pre-fault, measure, move on. There is a fancier path (a userspace fault handler built over Mach exception ports, a hand-rolled equivalent of `userfaultfd`) and it is parked as optional in `ROADMAP.md`. It is only worth touching if measurement shows pre-fault is actually hurting the budget on real hardware, and it probably will not for a personal box. Do not build it on spec.

## The coexistence budget

`amber-budget` enforces the two rules. It tracks the resident set of every live VM against the fleet budget and admits, reclaims, or rejects. Reclaim levers, cheapest first:

- Free-page reporting. A cooperative guest reports free pages so the host can drop them, shrinking the real footprint without touching the workload.
- Balloon. Inflate a guest balloon to pull committed pages back under pressure, deflate when it eases.
- Pool eviction. Idle warm ambers are the first thing reclaimed when a live invocation needs the room. A warm pool is a cache, and a cache yields to a request.
- Admission control. When no reclaim fits a new VM under the budget, reject with a structured `BudgetExceeded` carrying the numbers, so the caller backs off instead of thrashing the machine.

The model's region is tagged and excluded from every lever. The scheduler can starve the warm pool to zero and reject invocations, but it cannot and will not reclaim into the model. That asymmetry is the design.

## Same-page sharing, scoped honestly

The only page sharing amber claims is fork-sharing: VMs cast from one amber share unwritten base pages through copy-on-write. General cross-VM merging in the style of KSM is a non-goal. On macOS there is no kernel KSM to lean on, and a userspace scanner would cost CPU the model wants. The fork model gives the realistic majority of the benefit without the scanner.

## The central tradeoff

The decision that defines behavior on a unified-memory machine, made per template, not globally:

- Hold warm ambers in RAM. Forking is single-digit milliseconds. The cost is the amber's RAM sitting idle inside the budget, which is RAM the model would otherwise have.
- Hold ambers on disk, restore on cast. Idle RAM cost is zero. The cost is fault-in latency on first use, which on macOS is the pre-fault penalty.

On a machine where the model wants every byte, the warm pool is expensive and the on-demand path is slower, and the right answer differs by how hot a template is. Hot templates earn a warm pool, cold templates restore from disk. The manifest exposes this per template (`warm_pool` and `snapshot`) precisely because there is no single correct value. The scheduler keeps the chosen mix under budget while never touching the model.
