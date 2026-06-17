#!/usr/bin/env bash
# Test the KVM backend (amber-kvm) without arm64 Linux hardware.
#
# QEMU emulates an arm64 machine with EL2 (TCG, `virtualization=on`), boots a
# KVM-host Linux, and inside it amber-kvm boots the resin guest via /dev/kvm and
# runs an init that prints KVM_INNER_OK. Three levels deep under pure emulation,
# so it is slow (minutes) but needs no hardware and runs anywhere QEMU + Docker
# do — including CI.
#
# Needs: qemu-system-aarch64, docker, kernel/.cache (the source tarball, shared
# with `make kernel`), assets/Image (resin) + assets/irx (busybox/musl). The
# outer KVM-host kernel is built once and cached under the work dir.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-/tmp/amber-kvm-test}"
KVER="${KVER:-6.12.93}"
mkdir -p "$WORK"
command -v qemu-system-aarch64 >/dev/null || { echo "need qemu (brew install qemu)"; exit 1; }
command -v docker >/dev/null || { echo "need docker"; exit 1; }
[ -f "$ROOT/assets/Image" ] || { echo "no assets/Image — run: make kernel"; exit 1; }
[ -f "$ROOT/kernel/.cache/linux-$KVER.tar.xz" ] || { echo "no cached kernel source — run: make kernel"; exit 1; }

# 1. Outer KVM-host kernel: arm64 defconfig (KVM=y), built once.
if [ ! -f "$WORK/outer-Image" ]; then
  echo "==> building outer KVM-host kernel (arm64 defconfig)"
  docker run --rm -v "$ROOT:/work" -v "$WORK:/out" -w /tmp debian:bookworm bash -euc "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq && apt-get install -y -qq build-essential flex bison bc libssl-dev libelf-dev cpio >/dev/null
    tar -xf /work/kernel/.cache/linux-$KVER.tar.xz && cd linux-$KVER
    make ARCH=arm64 defconfig >/dev/null && make ARCH=arm64 -j\$(nproc) Image >/dev/null
    cp arch/arm64/boot/Image /out/outer-Image"
fi

# 2. The Linux amber binary + the nested initramfs (amber + resin + inner guest).
echo "==> building amber (linux) + nested initramfs"
docker run --rm -v "$ROOT:/work" -v "$WORK:/out" -w /work rust:bookworm bash -euc '
  apt-get update -qq >/dev/null && apt-get install -y -qq cpio gzip >/dev/null
  CARGO_TARGET_DIR=/work/target-linux cargo build --release -p amber 2>&1 | grep -E "^error|Finished" | tail -1
  AMBER=/work/target-linux/release/amber

  # inner guest initramfs: prove resin booted under KVM, then power off.
  ii=/tmp/inner; mkdir -p $ii/bin $ii/lib $ii/dev $ii/proc
  cp /work/assets/irx/bin/busybox $ii/bin/busybox
  cp /work/assets/irx/lib/ld-musl-aarch64.so.1 $ii/lib/
  ln -sf ld-musl-aarch64.so.1 $ii/lib/libc.musl-aarch64.so.1
  printf "#!/bin/busybox sh\n/bin/busybox --install -s /bin\nmount -t proc proc /proc\nmount -t devtmpfs dev /dev\necho KVM_INNER_OK uname=\$(uname -r) nproc=\$(nproc)\npoweroff -f\n" > $ii/init
  chmod +x $ii/init
  ( cd $ii && find . | cpio -o -H newc 2>/dev/null | gzip > /out/inner.gz )

  # outer rootfs: amber + its glibc .so closure + resin + inner.gz + an init that
  # runs `amber boot` (the KVM backend) on resin.
  oo=/tmp/outer; mkdir -p $oo/bin $oo/lib $oo/dev $oo/proc $oo/sys
  cp /work/assets/irx/bin/busybox $oo/bin/busybox
  cp /work/assets/irx/lib/ld-musl-aarch64.so.1 $oo/lib/
  ln -sf ld-musl-aarch64.so.1 $oo/lib/libc.musl-aarch64.so.1
  cp $AMBER $oo/amber
  for lib in $(ldd $AMBER | grep -oE "/[^ ]+\.so[^ ]*" | sort -u); do
    mkdir -p "$oo$(dirname $lib)"; cp -L "$lib" "$oo$lib"
  done
  cp /work/assets/Image $oo/resin
  cp /out/inner.gz $oo/inner.gz
  printf "#!/bin/busybox sh\n/bin/busybox --install -s /bin\nexport PATH=/bin\nmount -t proc proc /proc\nmount -t sysfs sysfs /sys\nmount -t devtmpfs dev /dev\nRUST_LOG=info /amber __kvm-selftest\nRUST_LOG=info /amber boot /resin /inner.gz\necho amber-boot-rc=\$?\npoweroff -f\n" > $oo/init
  chmod +x $oo/init
  ( cd $oo && find . | cpio -o -H newc 2>/dev/null | gzip > /out/initramfs.gz )'

# 3. Boot the outer VM under QEMU TCG and look for the inner marker.
echo "==> booting (QEMU TCG, EL2-emulated; slow)…"
log="$WORK/run.log"
qemu-system-aarch64 -M virt,virtualization=on,gic-version=3 -cpu max -accel tcg -smp 1 -m 2G \
  -kernel "$WORK/outer-Image" -initrd "$WORK/initramfs.gz" -append "console=ttyAMA0" \
  -nographic -no-reboot > "$log" 2>&1 &
qp=$!
for _ in $(seq 1 72); do
  grep -qaE "KVM_INNER_OK|backend error|unhandled KVM|Kernel panic|amber-boot-rc" "$log" 2>/dev/null && break
  kill -0 $qp 2>/dev/null || break
  sleep 5
done
sleep 4; kill $qp 2>/dev/null || true; pkill -f qemu-system-aarch64 2>/dev/null || true

echo "--- relevant output ---"
grep -aE "booting on KVM|KVM:|KVM_INNER_OK|backend error|unhandled KVM|panic|amber-boot-rc" "$log" || true
if grep -qa "KVM_INNER_OK" "$log"; then
  echo ">>> PASS: resin booted under amber-kvm"
else
  echo ">>> FAIL — boot log (last 100 lines):"; tail -100 "$log"; exit 1
fi
