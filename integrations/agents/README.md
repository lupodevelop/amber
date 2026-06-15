# amber sandbox: other AI coding agents

The Claude Code plugin lives in [`../claude-plugin`](../claude-plugin). This
directory holds the same sandbox wired for other agents (Codex, Cursor, Windsurf,
Cline). Each file is a small instruction in the tool's expected location and
format; the agent reads it and, when about to run risky or untrusted code, runs it
in an amber microVM via the `amber` CLI instead of on the host.

## Prerequisite: install amber

These adapters call `amber` directly, so install it once (arm64 host: Apple
Silicon, or arm64 Linux with `/dev/kvm`):

```sh
curl -fsSL https://raw.githubusercontent.com/lupodevelop/amber/main/scripts/install.sh | sh
```

That puts `amber` on your `PATH`. No per-tool scripts and no daemon: the adapters
use `amber run <image> -- '<cmd>'`, which boots a microVM, runs the command, and
exits.

## Per-tool install

| Tool | Copy this file | To |
| --- | --- | --- |
| **Codex** (and any `AGENTS.md`-aware agent) | `agents/AGENTS.md` | your repo root `AGENTS.md` (or append to it), or `~/.codex/AGENTS.md` |
| **Cursor** | `agents/cursor/rules/amber-sandbox.mdc` | `.cursor/rules/amber-sandbox.mdc` in your project |
| **Windsurf** | `agents/windsurf/rules/amber-sandbox.md` | `.windsurf/rules/amber-sandbox.md` in your project |
| **Cline / Roo** | `agents/cline/amber-sandbox.md` | `.clinerules/amber-sandbox.md` in your project |

## Status

These adapters are written to each tool's documented format but have not been
validated inside every tool. If one does not trigger, check that the tool picked up
the file (its rules/instructions UI) and that `amber` is on `PATH`. The underlying
sandbox (amber itself) is the same one the Claude Code plugin uses and is tested.
