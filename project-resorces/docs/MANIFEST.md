# MANIFEST

`amber.toml` declares the persistent scaffolding: which templates exist, how large each VM may grow, how many warm ambers to hold, and the total RAM budget the whole fleet must respect. It does not declare running workloads. Workloads are imperative and arrive through the API in `CONTROL-API.md`.

Read by `amber up`, reloaded with `amber reload`.

## Schema

```toml
[fleet]
ram_budget   = "4GiB"      # hard ceiling for the sum of all live VMs
reserve_for  = "model"     # never reclaim into a region tagged as the model's
host_gateway = "vsock"     # egress, if any, is mediated by the host over vsock
socket       = "$XDG_RUNTIME_DIR/amber.sock"

[template.pytools]
image     = "docker.io/library/python:3.12-slim"
vcpus     = 2
ram_cap   = "512MiB"       # per-VM ceiling; the fleet budget still wins
net       = "none"          # none | gateway
rootfs    = "erofs+overlay"
warm_pool = 3               # ambers kept ready to cast
snapshot  = "post-init"     # boot | post-init | none
reuse     = false           # discard after one run, or reset and return to pool
timeout   = "30s"

  [[template.pytools.mount]]
  host  = "./fixtures"
  guest = "/fixtures"
  mode  = "ro"

  [template.pytools.env]
  PYTHONUNBUFFERED = "1"

[template.node]
image     = "docker.io/library/node:22-alpine"
vcpus     = 1
ram_cap   = "384MiB"
net       = "gateway"       # this template may have mediated egress
warm_pool = 1
snapshot  = "post-init"
timeout   = "60s"

[template.scratch]
image     = "docker.io/library/alpine:3"
vcpus     = 1
ram_cap   = "128MiB"
net       = "none"
warm_pool = 0               # cold cast on demand, nothing held warm
snapshot  = "boot"
reuse     = false
```

## Field notes

- `ram_budget` is the whole point. A ceiling on the sum of live VMs, not a per-VM figure. `amber-budget` enforces it: when admitting a VM would breach it, reclaim from idle pooled VMs first, reject with a budget error if it still does not fit. The model's region is never a reclaim target.
- `snapshot = "post-init"` is the fast path: amber the VM after the runtime is initialized, cast from there, skip boot and init. `"boot"` ambers right after kernel handoff, cheaper to hold, slower to first user code. `"none"` boots cold every time.
- `warm_pool = N` trades RAM for latency. Held ambers in memory cast in single-digit milliseconds but cost their share of the budget while idle. `0` keeps the budget free and pays a fault-in latency on first use. The right value depends on how much of the machine the model has claimed. See the tradeoff at the end of `MEMORY.md`.
- `net = "none"` means no virtio-net device exists. Nothing to misconfigure. `net = "gateway"` adds the device, wired only to the host gateway.
- `mount` is host to guest, read-only, never the home directory by default. No implicit host filesystem passthrough.
- `env` is non-secret configuration. Secrets are injected at runtime over the control channel and never persist in the image, the template, or the amber.

## CLI

Docker-shaped, so there is nothing to learn. The amber theme stays out of the verbs.

```
amber up                         start amberd and warm the pools
amber down                       stop amberd, discard all VMs
amber reload                     re-read amber.toml, adjust pools

amber run <template> -- <argv>   cast from the template, run argv, stream, tear down
amber run <image>    -- <argv>   same, from a bare OCI ref with template defaults
amber exec <id> -- <argv>        run another command in an existing VM
amber ps                         list live VMs, template, RAM, age
amber rm <id>                    discard a VM
amber snapshot <template>        rebuild the template amber after an image bump
amber pull <image>               pre-pull and flatten an image into the cache
amber budget                     show the fleet RAM budget and current usage

amber serve --mcp                expose the MCP tool surface for agents
amber nodes                      list reachable nodes (homelab, see CLUSTER.md)
```

A bare `amber run python:3.12-slim -- python script.py` with no manifest works: pull, flatten, boot cold, run, tear down, using built-in defaults. The manifest exists to add warm pools and budgets, not to make the simple case verbose.
