# CONTROL-API

How anything drives a VM: the CLI, a script, or an AI agent. One protocol underneath, two front doors, a raw JSON socket and an MCP tool surface.

## Transport

Primary transport is a unix domain socket, default `$XDG_RUNTIME_DIR/amber.sock`. No TCP listener by default. Binding one is an explicit, discouraged flag, because the API runs arbitrary code and must never be reachable off-host without intent. Cross-node calls in the homelab use a separate authenticated channel, see `CLUSTER.md`.

In-guest control, a VM spawning a child VM, is not supported in the first version. Nesting is deferred until the flat budget model is proven.

## Wire format

Length-prefixed JSON, one request, one or more responses. Long-running calls stream multiple frames before a terminal frame.

## Request types

- `CreateVm { template, overrides? } -> { vm_id }`
  Cast a VM from the template amber, or boot one if the template has no amber. `overrides` may tighten `ram_cap`, `timeout`, and `net`, never loosen them past the template.

- `Exec { vm_id, argv, env?, stdin?, cwd? } -> stream of { stdout | stderr } then { exit, code }`

- `WriteFile { vm_id, path, bytes, mode? } -> { ok }`
  How code gets in without a network or a host mount.

- `ReadFile { vm_id, path } -> { bytes }`
  How results come back.

- `Kill { vm_id } -> { ok }`

- `List { } -> [ { vm_id, template, ram_bytes, age, state } ]`

- `Pool { template, n } -> { ok }`
  Resize a template's warm pool at runtime.

- `Snapshot { vm_id, label? } -> { amber_id }`
  Capture the current VM as a reusable amber. Used to build the post-init template amber.

- `Budget { } -> { ram_budget, ram_used, per_template }`

## One-shot convenience

- `RunOneShot { template, argv, env?, stdin?, files? } -> stream of output then { exit, code }`
  Take-from-pool-or-cast, optional `WriteFile` per entry in `files`, `Exec`, stream, discard. This is what `amber run` issues, and what an agent issues for "run this and tell me what happened".

## Errors

Structured, not strings. The ones a caller must handle distinctly:

- `BudgetExceeded { ram_budget, ram_used, requested }` admission refused to protect the budget. Retry later or smaller.
- `Timeout { vm_id, limit }` the invocation exceeded the template timeout and was killed.
- `ImageUnavailable { reference }`
- `TemplateUnknown { name }`

## MCP surface

`amber serve --mcp` exposes the same operations as MCP tools, so an LLM agent drives sandboxes through tool calls. Few and blunt:

- `sandbox.run { template, command, files? }` cast, optionally write files, run, return combined output and exit code, discard. The everyday tool.
- `sandbox.open { template } -> { sandbox_id }` for multi-step work that keeps state across calls.
- `sandbox.exec { sandbox_id, command }`
- `sandbox.write_file { sandbox_id, path, content }`
- `sandbox.read_file { sandbox_id, path } -> { content }`
- `sandbox.close { sandbox_id }`
- `sandbox.list -> [...]`

The tool descriptions tell the model the three truths it needs: the sandbox has no network unless the template grants it, cannot see the host filesystem, and is discarded on close. A `BudgetExceeded` surfaces as a normal tool error with the numbers attached, so a well-built agent can wait or ask for a smaller run rather than fail blindly.

## Runtime value injection

A value that should not be baked into an image can be pushed into a running guest over the control channel after the amber was cast, for example with `WriteFile` to a tmpfs path or as an entry in the `Exec` env. Because it arrives after the snapshot, it is never captured in the amber, never on the base image, and not reachable from any other VM. Where such a value comes from is the caller's concern, not amber's. amber injects, it does not manage. Handling guidance is in `SECURITY.md`.
