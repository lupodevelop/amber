# amber sandbox: run untrusted code in a microVM

When you are about to run a shell command or code you do not fully trust
(something destructive, irreversible, or that should not see the host), run it in
an amber microVM instead of on the host:

```sh
AMBER_NET=none amber run alpine:3 -- '<command>'
```

amber boots a real arm64 Linux microVM in tens of milliseconds; the command cannot
touch the host filesystem or processes. `AMBER_NET=none` keeps the guest offline
(amber's network is on by default), which is what you want for untrusted code; drop
it only when the task genuinely needs the network. stdout, stderr, and the exit
code come back unchanged. No daemon or setup is needed; `amber run` boots, runs, and
exits.

## When to use it

- The user asks you to run code or a command you do not fully trust.
- A destructive or irreversible operation (`rm -rf`, migrations, `dd`, `curl | sh`).
- Untrusted scripts, CTF binaries, scraped snippets.
- You just want a clean, disposable Linux userland to test something in.

## Choosing the userland and resources

Pick the image for the toolchain you need: `alpine:3` (default, with `apk`),
`python:3-alpine`, `rust:alpine`, `node:alpine`, and so on. Raise guest RAM for a
heavier build with `AMBER_MEM=2GiB`.

```sh
AMBER_NET=none AMBER_MEM=2GiB amber run rust:alpine -- 'cargo --version'
```

## Testing a project

Pipe a tar of the directory into the guest and unpack it; the host copy is never
touched:

```sh
tar -C <dir> -cf - . | AMBER_NET=none amber run alpine:3 -- \
  'mkdir /w && tar -xf - -C /w && cd /w && <command>'
```

To get changes back out, have the command emit a patch (for example
`git -C /w diff`) rather than writing to the host.

## Setup

Install amber once (arm64 host: Apple Silicon, or arm64 Linux with `/dev/kvm`):

```sh
curl -fsSL https://raw.githubusercontent.com/lupodevelop/amber/main/scripts/install.sh | sh
```

Then `amber` is on your `PATH`. See <https://github.com/lupodevelop/amber>.
