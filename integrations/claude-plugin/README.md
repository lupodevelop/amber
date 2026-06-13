# amber-sandbox — Claude Code plugin

Give an AI coding assistant a real, disposable **arm64 Linux microVM** to run
untrusted or destructive code in, instead of running it on your machine. Backed
by [amber](../../README.md): boots in tens of milliseconds, the guest can't touch
the host filesystem or processes, and the network is off by default.

## What's in it

- **`amber-sandbox` skill** — Claude reaches for this on its own when it's about
  to run risky/untrusted code: it runs the command in a throwaway microVM and
  reads back clean stdout / stderr / exit code.
- **`amber-install` skill** — requirements + troubleshooting (setup is automatic).
- **`/amber-sandbox:run <command>`** — run a command in the sandbox on demand.
- **`scripts/amber-exec.sh`** — the helper both use; usable standalone. It
  downloads a prebuilt amber on first use (`scripts/amber-fetch.sh`).

## Install

**Local (development / using the amber repo directly):**

```bash
claude --plugin-dir integrations/claude-plugin
```

**As a marketplace (from the repo):**

```text
/plugin marketplace add ./integrations      # or the GitHub repo
/plugin install amber-sandbox@amber
```

No build needed: the first sandbox run downloads a prebuilt amber (binary + resin
kernel + agent + userland) for this platform into `~/.cache/amber/`. You just need
an **arm64 host** (Apple Silicon, or arm64 Linux with `/dev/kvm`). To use your own
source build instead, set `AMBER_HOME` to a checkout — see the `amber-install` skill.

## Use

Just ask Claude to run something risky — it routes it through the sandbox — or
call it directly:

```text
/amber-sandbox:run rm -rf / --no-preserve-root ; echo "survived: $?"
```

```bash
AMBER_SANDBOX_IMAGE=python:3-alpine \
  integrations/claude-plugin/scripts/amber-exec.sh 'python3 -c "print(6*7)"'
```

| Env var | Default | Meaning |
|---|---|---|
| `AMBER_SANDBOX_IMAGE` | `alpine:3` | OCI image for the guest userland |
| `AMBER_SANDBOX_NET` | `0` | `1` to allow guest networking |
| `AMBER_HOME` | derived | amber checkout, if not auto-detected |

Each call is a fresh VM — no state persists between runs.

## Requirements

- macOS (Apple Silicon, Hypervisor.framework) or Linux arm64 with `/dev/kvm`.
- Docker (to build the kernel and the in-guest agent).
- The amber binary, codesigned on macOS (`make build`).
