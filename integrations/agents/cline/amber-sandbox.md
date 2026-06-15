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
code come back unchanged. No daemon or setup is needed.

Reach for it when: the user asks you to run code you do not fully trust; a
destructive or irreversible operation (`rm -rf`, migrations, `dd`, `curl | sh`); or
untrusted scripts and scraped snippets.

Pick the image for the toolchain you need (`python:3-alpine`, `rust:alpine`,
`node:alpine`, ...) and raise guest RAM with `AMBER_MEM=2GiB` for a heavier build.

Test a project against an isolated copy (the host is untouched):

```sh
tar -C <dir> -cf - . | AMBER_NET=none amber run alpine:3 -- \
  'mkdir /w && tar -xf - -C /w && cd /w && <command>'
```

Install amber once (arm64 host): `curl -fsSL
https://raw.githubusercontent.com/lupodevelop/amber/main/scripts/install.sh | sh`.
