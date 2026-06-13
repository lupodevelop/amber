#!/usr/bin/env bash
# Bundle a ready-to-run amber into dist/amber-<os>-<arch>.tar.gz: the binary plus
# the guest artifacts it needs at runtime (the resin kernel, the in-guest agent,
# and the busybox/musl userland). Run after building the binary and the assets.
#
#   package-release.sh <os> [arch]        # os: macos|linux, arch default arm64
#
# The tarball unpacks to a single `amber/` directory holding `amber` + `assets/`;
# run amber from inside it (it reads assets/ by relative path).
set -euo pipefail

os="${1:?usage: package-release.sh <macos|linux> [arch]}"
arch="${2:-arm64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

bin="$ROOT/target/release/amber"
[ -x "$bin" ] || { echo "no binary at $bin (build it first)" >&2; exit 1; }
for a in assets/Image assets/amber-agent assets/irx/bin/busybox assets/irx/lib/ld-musl-aarch64.so.1; do
  [ -e "$ROOT/$a" ] || { echo "missing $a — run 'make kernel', build-agent.sh, fetch-assets.sh" >&2; exit 1; }
done

stage="$(mktemp -d)"
dir="$stage/amber"
mkdir -p "$dir/assets/irx"
cp "$bin" "$dir/amber"
cp "$ROOT/assets/Image" "$dir/assets/Image"
cp "$ROOT/assets/amber-agent" "$dir/assets/amber-agent"
cp -R "$ROOT/assets/irx/." "$dir/assets/irx/"
[ -f "$ROOT/amber.entitlements" ] && cp "$ROOT/amber.entitlements" "$dir/amber.entitlements"
printf '%s\n' "amber prebuilt bundle ($os/$arch). Run ./amber from this directory." > "$dir/README"

mkdir -p "$ROOT/dist"
out="$ROOT/dist/amber-${os}-${arch}.tar.gz"
tar -C "$stage" -czf "$out" amber
rm -rf "$stage"
echo "$out"
