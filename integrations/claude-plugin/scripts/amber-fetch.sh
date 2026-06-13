#!/usr/bin/env bash
# Download a prebuilt amber bundle (binary + resin kernel + agent + userland) for
# this platform from the latest GitHub release and cache it. Prints the directory
# to use as AMBER_HOME (contains `amber` and `assets/`). Idempotent.
#
# Env: AMBER_REPO (default lupodevelop/amber), AMBER_RELEASE (default latest).
set -euo pipefail

repo="${AMBER_REPO:-lupodevelop/amber}"
rel="${AMBER_RELEASE:-latest}"
case "$(uname -s)" in Darwin) os=macos;; Linux) os=linux;; *) echo "unsupported OS" >&2; exit 1;; esac
case "$(uname -m)" in arm64|aarch64) arch=arm64;; *) echo "amber needs an arm64 host" >&2; exit 1;; esac

asset="amber-${os}-${arch}.tar.gz"
dest="${XDG_CACHE_HOME:-$HOME/.cache}/amber/${os}-${arch}"
if [ -x "$dest/amber" ] && [ -e "$dest/assets/Image" ]; then
  echo "$dest"; exit 0
fi

if [ "$rel" = latest ]; then
  url="https://github.com/$repo/releases/latest/download/$asset"
else
  url="https://github.com/$repo/releases/download/$rel/$asset"
fi
echo "amber: downloading prebuilt bundle ($asset) from $repo…" >&2
tmp="$(mktemp -d)"
curl -fSL --retry 3 "$url" -o "$tmp/$asset" >&2
tar -C "$tmp" -xzf "$tmp/$asset"
[ -x "$tmp/amber/amber" ] || { echo "bad bundle: no amber binary" >&2; exit 1; }
mkdir -p "$(dirname "$dest")"; rm -rf "$dest"; mv "$tmp/amber" "$dest"; rm -rf "$tmp"
# macOS: clear the download quarantine so HVF runs the ad-hoc-signed binary.
[ "$os" = macos ] && xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true
echo "$dest"
