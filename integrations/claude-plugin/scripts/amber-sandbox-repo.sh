#!/usr/bin/env bash
# Run a command against a COPY of a directory inside the sandbox. The directory is
# tar'd in, extracted to /work in a throwaway microVM, and <command> runs there —
# the host copy is never touched by the (possibly untrusted) command. Its stdout,
# stderr, and exit code come back unchanged.
#
#   amber-sandbox-repo.sh <dir> '<command run inside the copy>'
#
# Heavy/uninteresting dirs (.git, target, node_modules, .venv, dist) are excluded
# to keep the transfer small. Set AMBER_SANDBOX_NET=1 / AMBER_SANDBOX_IMAGE /
# AMBER_SANDBOX_MEM as for amber-exec.sh (e.g. an image with your toolchain).
set -euo pipefail

dir="${1:-}"; cmd="${2:-}"
[ -n "$dir" ] && [ -n "$cmd" ] || { echo "usage: amber-sandbox-repo.sh <dir> '<command>'" >&2; exit 2; }
[ -d "$dir" ] || { echo "no such directory: $dir" >&2; exit 2; }

self="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# tar the dir to stdout; amber-exec forwards it to the guest command's stdin, which
# unpacks it into /work and runs the user command there.
tar -C "$dir" \
    --exclude=.git --exclude=target --exclude=node_modules --exclude=.venv \
    --exclude=dist --exclude=.next --exclude=__pycache__ \
    -cf - . \
  | "$self/amber-exec.sh" "mkdir -p /work && tar -xf - -C /work && cd /work && ( $cmd )"
