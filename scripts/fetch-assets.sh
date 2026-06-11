#!/usr/bin/env bash
# Fetch the guest userland amber's bootstrap needs: a static busybox + the musl
# loader (borrowed Alpine artifacts, not redistributed in this repo).
#
# The kernel is resin — amber's own built-in-everything arm64 kernel — built by
# `make kernel` into assets/Image. resin loads no modules, so the default fetch
# is just the userland:
#
#   assets/irx/bin/busybox                       static busybox
#   assets/irx/lib/ld-musl-aarch64.so.1          musl loader
#
# --alpine-kernel additionally fetches the borrowed Alpine `virt` kernel and its
# modules (the pre-resin setup, kept as a fallback/reference):
#
#   assets/Image                                 arm64 kernel (Alpine vmlinuz-virt)
#   assets/irx/lib/modules/<ver>/...             kernel modules + modules.dep
#
# Requires: curl, tar, gzip; with --alpine-kernel also unsquashfs (squashfs-tools
# — `brew install squashfs`). Override the Alpine release with ALPINE_VER=3.22.1,
# the output dir with OUT=./assets.
set -euo pipefail

ALPINE_VER="${ALPINE_VER:-3.22.1}"
ARCH=aarch64
BRANCH="v${ALPINE_VER%.*}"                # 3.22.1 -> v3.22
CDN="https://dl-cdn.alpinelinux.org/alpine/${BRANCH}/releases/${ARCH}"
NETBOOT="${CDN}/netboot-${ALPINE_VER}"
OUT="${OUT:-assets}"
ALPINE_KERNEL=0
[ "${1:-}" = "--alpine-kernel" ] && ALPINE_KERNEL=1

TOOLS="curl tar gzip"
[ "$ALPINE_KERNEL" = 1 ] && TOOLS="$TOOLS unsquashfs"
for tool in $TOOLS; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "error: '$tool' not found." >&2
    [ "$tool" = unsquashfs ] && echo "  install squashfs-tools: brew install squashfs" >&2
    exit 1
  }
done

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$OUT/irx/bin" "$OUT/irx/lib"

echo "==> Alpine ${ALPINE_VER} ${ARCH} userland -> ${OUT}/"

# busybox + musl loader, from the minirootfs tarball.
echo "  - busybox + musl (minirootfs)"
curl -fSL --retry 3 "${CDN}/alpine-minirootfs-${ALPINE_VER}-${ARCH}.tar.gz" -o "$tmp/root.tgz"
tar -xzf "$tmp/root.tgz" -C "$tmp" bin/busybox lib/ld-musl-aarch64.so.1
cp "$tmp/bin/busybox" "$OUT/irx/bin/busybox"
cp "$tmp/lib/ld-musl-aarch64.so.1" "$OUT/irx/lib/ld-musl-aarch64.so.1"
ln -sf ld-musl-aarch64.so.1 "$OUT/irx/lib/libc.musl-aarch64.so.1"
chmod +x "$OUT/irx/bin/busybox"

if [ "$ALPINE_KERNEL" = 0 ]; then
  # resin path: the kernel comes from `make kernel`, which loads no modules. A
  # stale Alpine modules dir would make the bootstrap insmod foreign .ko into
  # resin (harmless vermagic failures, but noisy) — clear it.
  rm -rf "$OUT/irx/lib/modules"
  if [ -f "$OUT/Image" ]; then
    echo "==> done: userland ready (existing ${OUT}/Image kept). Now: make"
  else
    echo "==> done: userland ready. Now: make kernel && make"
  fi
  exit 0
fi

# --alpine-kernel: the borrowed modular kernel + its modloop.
#
# Kernel: modern Alpine ships vmlinuz-virt as an EFI zboot image (a PE stub
# wrapping a compressed kernel); amber's loader wants the raw arm64 Image, so if
# it's zboot, extract the payload (offset/size in the header) and decompress per
# the header's compression field.
echo "  - kernel (vmlinuz-virt)"
curl -fSL --retry 3 "${NETBOOT}/vmlinuz-virt" -o "$tmp/vmlinuz"
if [ "$(dd if="$tmp/vmlinuz" bs=1 skip=4 count=4 2>/dev/null)" = "zimg" ]; then
  poff=$(od -An -tu4 -j8  -N4 "$tmp/vmlinuz" | tr -d ' ')
  psize=$(od -An -tu4 -j12 -N4 "$tmp/vmlinuz" | tr -d ' ')
  comp=$(dd if="$tmp/vmlinuz" bs=1 skip=24 count=8 2>/dev/null | tr -d '\0')
  dd if="$tmp/vmlinuz" bs=1 skip="$poff" count="$psize" 2>/dev/null > "$tmp/payload"
  case "$comp" in
    gzip)     gzip -dc  "$tmp/payload" > "$OUT/Image" ;;
    zstd)     zstd -dc  "$tmp/payload" > "$OUT/Image" ;;
    lzma|xz)  xz   -dc  "$tmp/payload" > "$OUT/Image" ;;
    *) echo "error: unsupported zboot compression '$comp'" >&2; exit 1 ;;
  esac
  echo "    (decompressed EFI zboot/$comp)"
else
  cp "$tmp/vmlinuz" "$OUT/Image"
fi

# Modules: the modloop is a squashfs of /lib/modules for this exact kernel.
echo "  - modules (modloop-virt, unsquashing)"
curl -fSL --retry 3 "${NETBOOT}/modloop-virt" -o "$tmp/modloop"
unsquashfs -q -f -d "$tmp/ml" "$tmp/modloop" >/dev/null
# The modloop's modules/ holds the versioned module tree plus a `firmware` dir;
# keep only the versioned dir (the one with a modules.dep).
rm -rf "$OUT/irx/lib/modules"
mkdir -p "$OUT/irx/lib/modules"
for d in "$tmp/ml/modules"/*/; do
  [ -f "${d}modules.dep" ] && cp -R "$d" "$OUT/irx/lib/modules/$(basename "$d")"
done

ver="$(ls "$OUT/irx/lib/modules" | head -1)"
echo "==> done: alpine kernel + modules ${ver}, busybox + musl. Now: make"
