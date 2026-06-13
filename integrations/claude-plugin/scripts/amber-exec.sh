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

"$bin" up >/dev/null 2>&1 || true
tpl=".amber-cache/sandbox/$(printf '%s' "$image" | tr '/:@' '___')"
if [ ! -f "$tpl/meta.json" ]; then
  echo "amber: building sandbox template for $image (one-time)…" >&2
  AMBER_NET="$net" "$bin" template "$image" "$tpl" >&2
fi

# Warm fork + run. `amber exec` forwards stdout/stderr as distinct streams and
# exits with the command's own code.
exec "$bin" exec "$tpl" -- "$cmd"
