#!/bin/sh
# Install amber: download the latest prebuilt release and put `amber` on PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/lupodevelop/amber/main/scripts/install.sh | sh
#
# It unpacks the bundle (binary + resin kernel + agent + userland) to
# $PREFIX/lib/amber and symlinks $PREFIX/bin/amber to it. `amber` finds its assets
# next to the real binary, so it works from any directory.
#
# Env: PREFIX (default ~/.local), AMBER_REPO (default lupodevelop/amber),
#      AMBER_RELEASE (default latest).
set -eu

repo="${AMBER_REPO:-lupodevelop/amber}"
rel="${AMBER_RELEASE:-latest}"
prefix="${PREFIX:-$HOME/.local}"

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux)  os=linux ;;
  *) echo "amber: unsupported OS (need macOS or Linux)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=arm64 ;;
  *) echo "amber: needs an arm64 host (Apple Silicon, or arm64 Linux with /dev/kvm)" >&2; exit 1 ;;
esac

asset="amber-${os}-${arch}.tar.gz"
if [ "$rel" = latest ]; then
  url="https://github.com/$repo/releases/latest/download/$asset"
else
  url="https://github.com/$repo/releases/download/$rel/$asset"
fi

echo "amber: downloading $asset from $repo" >&2
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fSL --retry 3 "$url" -o "$tmp/$asset"

# Integrity: verify the bundle against SHA256SUMS from the same release before we
# unpack and run it. (Provenance is also attested — verify out of band with
# `gh attestation verify $tmp/$asset --repo $repo`.)
curl -fSL --retry 3 "${url%/*}/SHA256SUMS" -o "$tmp/SHA256SUMS"
want="$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$want" ] || { echo "amber: $asset not listed in SHA256SUMS" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
else
  got="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
fi
[ "$got" = "$want" ] || { echo "amber: checksum mismatch (want $want, got $got)" >&2; exit 1; }

tar -C "$tmp" -xzf "$tmp/$asset"
[ -x "$tmp/amber/amber" ] || { echo "amber: bad bundle (no binary inside)" >&2; exit 1; }

mkdir -p "$prefix/lib" "$prefix/bin"
rm -rf "$prefix/lib/amber"
mv "$tmp/amber" "$prefix/lib/amber"
ln -sf "$prefix/lib/amber/amber" "$prefix/bin/amber"
# macOS: clear the download quarantine so HVF runs the ad-hoc-signed binary.
[ "$os" = macos ] && xattr -dr com.apple.quarantine "$prefix/lib/amber" 2>/dev/null || true

echo "amber: installed to $prefix/bin/amber" >&2
case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *) echo "amber: add $prefix/bin to your PATH, e.g. export PATH=\"$prefix/bin:\$PATH\"" >&2 ;;
esac
echo "amber: try  amber run alpine:3 -- echo hello" >&2
