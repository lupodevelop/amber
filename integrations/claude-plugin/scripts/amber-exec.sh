#!/usr/bin/env bash
# Run a shell command inside a disposable amber microVM and forward its stdout,
# stderr, and exit code unchanged. The guest runs `sh -c "<command>"` in a fork
# of a cached template — a fresh, isolated arm64 Linux userland that cannot touch
# the host filesystem, processes, or (by default) the network.
#
#   amber-exec.sh '<shell command>'
#
# On first use it downloads a prebuilt amber (binary + resin kernel + agent +
# userland) for this platform; nothing to build. Set AMBER_HOME to a source
# checkout or an existing bundle to skip the download.
#
# Env:
#   AMBER_HOME            amber bundle/checkout dir (default: auto-downloaded)
#   AMBER_SANDBOX_IMAGE   OCI image for the guest userland (default: alpine:3)
#   AMBER_SANDBOX_NET     "1" to allow guest networking (default: off)
set -euo pipefail

cmd="${1:-}"
[ -n "$cmd" ] || { echo "usage: amber-exec.sh '<shell command>'" >&2; exit 2; }

self="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
home="${AMBER_HOME:-}"
[ -n "$home" ] || home="$("$self/amber-fetch.sh")"   # download prebuilt on first use
cd "$home"

bin=./amber
[ -x "$bin" ] || bin=target/release/amber           # source checkout layout
[ -x "$bin" ] || { echo "no amber binary in $home" >&2; exit 127; }
[ -e assets/Image ] || { echo "no guest assets in $home (assets/Image missing)" >&2; exit 127; }

image="${AMBER_SANDBOX_IMAGE:-alpine:3}"
net="none"; [ "${AMBER_SANDBOX_NET:-0}" = "1" ] && net="smoltcp"
mem="${AMBER_SANDBOX_MEM:-}"   # e.g. 2GiB, for heavier toolchains/builds

# Keep our stdin (which may be a tarball for the command) away from the daemon and
# the template build — their console readers would otherwise consume it. Only the
# final `exec` inherits it.
"$bin" up >/dev/null 2>&1 </dev/null || true
# The template bakes in the image, the net device, and the RAM size, so they're
# all part of the cache key — changing one builds a new template, not a stale one.
key="$(printf '%s' "$image" | tr '/:@' '___')_${net}_${mem:-default}"
tpl=".amber-cache/sandbox/$key"
if [ ! -f "$tpl/meta.json" ]; then
  echo "amber: building sandbox template ($image, net=$net, mem=${mem:-default})…" >&2
  AMBER_NET="$net" AMBER_MEM="$mem" "$bin" template "$image" "$tpl" </dev/null >&2
fi

# Warm fork + run. `amber exec` forwards stdout/stderr as distinct streams and
# exits with the command's own code.
exec "$bin" exec "$tpl" -- "$cmd"
