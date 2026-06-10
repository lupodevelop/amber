#!/usr/bin/env bash
# Build "resin" — amber's trimmed, built-in-everything arm64 guest kernel.
#
# Everything amber drives (virtio-mmio/blk/net/balloon/rng, PL011, GICv2, arch
# timer) and every filesystem its rootfs uses (squashfs, overlay, tmpfs) is
# compiled in, so amber boots resin with an empty modules dir and skips insmod.
#
# Built natively inside an arm64 Linux container — Docker on Apple Silicon runs
# arm64 images natively, so there is no cross toolchain to manage. The kernel
# source is upstream (GPL); this repo ships only the config fragment + this script.
#
# Output: assets/Image (override OUT=). Kernel version: KVER= (default below).
set -euo pipefail

KVER="${KVER:-6.12.93}"
KMAJ="${KVER%%.*}"
OUT="${OUT:-assets/Image}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || echo 4)}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

[ -f "$ROOT/kernel/resin.config" ] || { echo "missing kernel/resin.config"; exit 1; }
command -v docker >/dev/null || { echo "docker not found"; exit 1; }

# Cache the kernel source tarball on the host so re-runs (config iteration) skip
# the ~140 MiB download and go straight to compile.
CACHE="$ROOT/kernel/.cache"
mkdir -p "$CACHE"

echo "==> building resin $KVER (arm64, -j$JOBS) -> $OUT"
docker run --rm -v "$ROOT:/work" -v "$CACHE:/cache" -w /work debian:bookworm bash -euc '
  set -euo pipefail
  export DEBIAN_FRONTEND=noninteractive
  echo "--> toolchain"
  apt-get update -qq
  apt-get install -y -qq build-essential flex bison bc libssl-dev libelf-dev \
    cpio xz-utils gzip curl perl >/dev/null

  tarball="/cache/linux-'"$KVER"'.tar.xz"
  if [ ! -f "$tarball" ]; then
    url="https://cdn.kernel.org/pub/linux/kernel/v'"$KMAJ"'.x/linux-'"$KVER"'.tar.xz"
    echo "--> fetch $url"
    curl -fSL --retry 3 "$url" -o "$tarball.tmp" && mv "$tarball.tmp" "$tarball"
  else
    echo "--> using cached source $(basename "$tarball")"
  fi
  cd /tmp
  tar -xf "$tarball"
  cd "linux-'"$KVER"'"

  # Base config: Alpine'\''s linux-virt (a proven virt-guest config) if present,
  # else the generic arm64 defconfig. Then overlay the resin fragment.
  if [ -f /work/kernel/alpine-virt.config ]; then
    echo "--> base: Alpine linux-virt config + resin fragment"
    cp /work/kernel/alpine-virt.config .config
  else
    echo "--> base: arm64 defconfig + resin fragment"
    make ARCH=arm64 defconfig >/dev/null
  fi
  ./scripts/kconfig/merge_config.sh -m -O . .config /work/kernel/resin.config >/dev/null
  make ARCH=arm64 olddefconfig >/dev/null

  echo "--> compile Image"
  make ARCH=arm64 -j'"$JOBS"' Image >/dev/null
  cp arch/arm64/boot/Image "/work/'"$OUT"'"
  ls -lh arch/arm64/boot/Image
'
echo "==> done: $OUT"
