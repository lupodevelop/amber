#!/usr/bin/env bash
# Build "resin" — amber's trimmed, built-in-everything arm64 guest kernel.
#
# Self-contained: upstream kernel source (kernel.org) + amber's own committed
# kernel/resin_defconfig. No distro dependency at build time — resin is shipped
# ready. Everything amber drives (virtio-mmio/blk/net/balloon/rng, PL011, GICv2,
# arch timer) and every filesystem its rootfs uses (squashfs, overlay, tmpfs) is
# built in, so amber boots resin with no modules dir and skips insmod.
#
# Built natively inside an arm64 Linux container — Docker on Apple Silicon runs
# arm64 images natively, so there is no cross toolchain to manage. The kernel
# source is upstream GPL; this repo ships the defconfig + this script.
#
# Output: assets/Image (override OUT=). Kernel version: KVER= (must match the
# version resin_defconfig was generated against).
set -euo pipefail

KVER="${KVER:-6.12.93}"
KMAJ="${KVER%%.*}"
OUT="${OUT:-assets/Image}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || echo 4)}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

[ -f "$ROOT/kernel/resin_defconfig" ] || { echo "missing kernel/resin_defconfig"; exit 1; }
command -v docker >/dev/null || { echo "docker not found"; exit 1; }

# Cache the kernel source tarball on the host so re-runs skip the ~140 MiB
# download and go straight to compile.
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

  echo "--> resin_defconfig"
  cp /work/kernel/resin_defconfig .config
  make ARCH=arm64 olddefconfig >/dev/null

  echo "--> compile Image"
  make ARCH=arm64 -j'"$JOBS"' Image >/dev/null
  cp arch/arm64/boot/Image "/work/'"$OUT"'"
  # vmlinux + System.map (symbols) for debugging a boot hang. Both gitignored.
  cp vmlinux /work/kernel/vmlinux 2>/dev/null || true
  cp System.map /work/kernel/System.map 2>/dev/null || true
  ls -lh arch/arm64/boot/Image
'
echo "==> done: $OUT"
