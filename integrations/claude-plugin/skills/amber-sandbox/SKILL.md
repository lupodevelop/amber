---
name: amber-sandbox
description: >-
  Run untrusted, risky, or destructive shell commands and code inside an isolated
  amber arm64 microVM instead of on the host. Use when executing code you don't
  fully trust, trying a destructive or irreversible operation, running something
  that shouldn't see the host filesystem or network, or when you just need a clean
  disposable Linux environment. Each run is a fresh, throwaway VM.
allowed-tools: Bash(*)
---

# Running code in an amber microVM sandbox

amber boots a real arm64 Linux microVM (HVF on macOS, KVM on Linux) in tens of
milliseconds. A command run inside it cannot touch the host's filesystem,
processes, or, by default, the network. Use this instead of running risky code
directly on the host.

## When to reach for it

- The user asks you to run code or a command you don't fully trust.
- A destructive or irreversible operation you'd rather try in a throwaway box
  first (`rm -rf`, migrations, `dd`, package installs, `curl | sh`).
- Code that should be isolated from the host (untrusted scripts, CTF binaries,
  scraped snippets).
- You just want a clean Linux userland to test something in.

## How to run a command

Call the helper with a single shell command string. It prints the command's
**stdout** to stdout, its **stderr** to stderr, and exits with the command's own
**exit code**. Clean, separate streams, no marker parsing.

```bash
"${CLAUDE_PLUGIN_ROOT}/scripts/amber-exec.sh" 'echo hello; uname -m; exit 0'
```

Multi-line scripts work; pass the whole thing as one argument:

```bash
"${CLAUDE_PLUGIN_ROOT}/scripts/amber-exec.sh" '
set -e
apk add --no-cache jq >/dev/null      # the default image is alpine:3
echo "{\"x\":1}" | jq .x
'
```

The first call for a given image builds a template (a few seconds, one time);
later calls are warm forks (milliseconds).

## Knobs (environment variables)

- `AMBER_SANDBOX_IMAGE=<oci-image>`: the guest userland (default `alpine:3`),
  e.g. `AMBER_SANDBOX_IMAGE=python:3-alpine`.
- `AMBER_SANDBOX_NET=1`: allow the guest outbound network (default off). Turn it
  on only when the task needs it (for example installing packages).
- `AMBER_SANDBOX_MEM=<size>`: guest RAM (default about 512 MiB), e.g. `2GiB` for a
  heavier toolchain or build.
- `AMBER_HOME=<path>`: the amber checkout, if not auto-detected.

```bash
AMBER_SANDBOX_IMAGE=python:3-alpine "${CLAUDE_PLUGIN_ROOT}/scripts/amber-exec.sh" \
  'python3 -c "print(6*7)"'
```

## Testing a repo or directory you're working on

To run something against a project (build it, run its tests, try a risky script on
it), copy the directory **into** the sandbox and run there. The host copy is never
touched; the command runs on an isolated copy.

```bash
"${CLAUDE_PLUGIN_ROOT}/scripts/amber-sandbox-repo.sh" <dir> '<command in the copy>'
```

It tars `<dir>` in (excluding `.git`, `target`, `node_modules`, `.venv`, `dist`,
`__pycache__`), unpacks it to `/work`, and runs the command there. The tar streams
to the guest over the host→guest vsock channel, so a whole project copies in fine.

Output and exit code come back. To get *changes* back out, have the command emit a
patch you can review and apply on the host (for example `… && git -C /work diff`),
rather than mutating the host directly, which keeps the isolation.

## Toolchains and tests

Two ways to get a compiler, interpreter, or build tool in the sandbox:

1. **Use a base image that already has it** (preferred: offline, deterministic,
   fast, no network needed):

   ```bash
   AMBER_SANDBOX_IMAGE=python:3-alpine "${CLAUDE_PLUGIN_ROOT}/scripts/amber-exec.sh" 'python3 -c "print(6*7)"'
   AMBER_SANDBOX_IMAGE=rust:alpine      "${CLAUDE_PLUGIN_ROOT}/scripts/amber-exec.sh" 'cargo --version'
   AMBER_SANDBOX_IMAGE=node:alpine      "${CLAUDE_PLUGIN_ROOT}/scripts/amber-exec.sh" 'node -e "console.log(6*7)"'
   ```

2. **Install at runtime**: needs `AMBER_SANDBOX_NET=1`, and you must install and
   use it **in the same command** (each call is a fresh VM, so installs don't
   persist):

   ```bash
   AMBER_SANDBOX_NET=1 "${CLAUDE_PLUGIN_ROOT}/scripts/amber-exec.sh" \
     'apk add --no-cache gcc musl-dev >/dev/null && echo "int main(){return 0;}" > t.c && gcc t.c -o t && ./t && echo OK'
   ```

   Networking works, including HTTPS for `apk`/`pip`/`npm`/`git`; the guest clock
   is seeded from the host so TLS is valid.

The writable layer is **tmpfs (RAM)**. The default guest RAM is about 472 MB
usable, fine for light installs and small builds. For a heavier toolchain or build,
raise it: `AMBER_SANDBOX_MEM=2GiB` (rebuilds the template once).

## Reading results

- Exit code 0 → success; non-zero → the command failed (propagated verbatim).
- stdout and stderr are separate, so capture them separately if you need to.
- Each invocation is a **fresh** VM: state does not persist between calls. To
  carry state, put it all in one command, or use a writable data disk (advanced).

## Setup

Nothing, usually: the **first run downloads a prebuilt amber** (binary, resin
kernel, agent, userland) for this platform into `~/.cache/amber/` and uses it. You
need an **arm64 host**: Apple Silicon (macOS, HVF) or arm64 Linux with `/dev/kvm`.
If a run fails on a host requirement, a download problem, or you want to build from
source, see the **`amber-install`** skill.

## What this does NOT do

- It is not a persistent container: no state between runs by default.
- The network is off unless you opt in; don't assume internet inside the VM.
- It runs arm64 Linux; x86-only binaries won't run.
