# CLUSTER

Optional. amber is single-host first. This document describes the most a homelab gets, and it is deliberately small: a flat set of arm64 nodes and a function that picks one. No consensus, no leader, no etcd, no autoscaling, no failover. If a node is off, it is not in the list. That is the entire fault model.

This falls out of a seam amber needs anyway, so it costs almost nothing to support and stays optional.

## The seam: a node

A local `amberd` and a remote one expose the same surface. So a client talks to a `Node`, and a `Node` is local or remote. The placement layer never knows the difference.

```rust
/// One arm64 box running amberd. Local or remote, same surface.
#[async_trait]
pub trait Node {
    async fn budget(&self) -> Result<Budget>;            // free RAM, live VMs
    async fn has_amber(&self, template: &str) -> Result<bool>;
    async fn run_one_shot(&self, req: RunRequest) -> Result<RunStream>;
    async fn open(&self, template: &str) -> Result<VmId>;
    async fn exec(&self, id: VmId, cmd: Exec) -> Result<RunStream>;
}

struct LocalNode  { /* amberd in-process */ }
struct RemoteNode { /* authenticated client to a peer amberd */ }
```

The async lives here, on the network and control layer. The VMM core stays synchronous, one thread per vcpu, because the hot run loop does not want an async runtime on it. The division is clean: async above the node seam, sync below the hypervisor seam.

## Placement

```rust
/// A flat set of nodes and a placement function. No consensus.
struct Harbor { nodes: Vec<Box<dyn Node>> }

impl Harbor {
    async fn place(&self, req: &RunRequest) -> Result<&dyn Node> {
        // 1. a node that already HAS the template amber warm
        //    (fork in ms, no image transfer)
        // 2. otherwise the node whose budget fits best
        // 3. otherwise none -> BudgetExceeded
    }
}
```

Placement prefers locality of the snapshot, not balance for its own sake. A node that already holds the warm amber forks in milliseconds. A cold node would first have to pull and flatten the image, which is seconds. So the cheapest placement is the node that has done the work already, and only when none has it does the budget-fit rule decide.

## What comes free, and what stays optional

Three things drop out of the same seam. Treat them as optional polish, not core work.

- Locality-aware placement, above. The default and the reason the homelab is worth anything.
- Peer image pull. Images are content-addressed, so a cold node can pull a flattened image from a peer that already has it instead of going back to the registry. Faster, and it works on a LAN with no internet.
- mDNS discovery. amber finds your other boxes on the LAN without hand-configured IPs. `amber nodes` lists them.

The RAM coexistence budget from `MEMORY.md` stays per node, unchanged. Each box protects its own resident model. The Harbor never reasons about a global budget, only about which single node can take a single VM.

## Authentication

Cross-node calls do not use the bare local socket. They use an authenticated channel between nodes you own, with mutual authentication, on the LAN. The threat model in `SECURITY.md` treats a node as trusted only after it authenticates. A node that cannot authenticate is simply not in the set.

## Non-goals, restated

- No scheduler that migrates running VMs between nodes.
- No replication or high availability. A node going down loses its VMs, which were ephemeral by design.
- No global state store. The set of nodes is discovered or configured, not consensus-managed.
- No cross-node networking between guest VMs. A VM's network story is the same as single-host, the gateway in `SECURITY.md`.

If you find yourself wanting any of these, you have left the homelab and want a real cluster manager, which amber is not and will not become. The escape hatch is to embed amber as a library and let a real orchestrator drive it.
