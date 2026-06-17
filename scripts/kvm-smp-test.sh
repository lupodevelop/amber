#!/usr/bin/env bash
# Multi-vcpu (SMP) teardown test for the KVM backend, hardware-free.
#
# Boots resin under KVM with two vcpus, confirms the guest sees both (nproc=2),
# then powers off. The point is teardown: when the primary takes the PSCI
# SYSTEM_OFF, request_stop must kick the secondary out of its blocking KVM_RUN
# so it observes running=false and joins — otherwise amber hangs and the outer
# init never prints `amber-boot-rc`. PASS requires both nproc=2 and a clean exit.
#
# Reuses the cached outer kernel + the Docker amber build from scripts/kvm-test.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-/tmp/amber-kvm-test}"
mkdir -p "$WORK"
command -v qemu-system-aarch64 >/dev/null || { echo "need qemu"; exit 1; }
command -v docker >/dev/null || { echo "need docker"; exit 1; }
[ -f "$ROOT/assets/Image" ] || { echo "no assets/Image"; exit 1; }
[ -f "$WORK/outer-Image" ] || { echo "no cached outer kernel — run scripts/kvm-test.sh first"; exit 1; }

echo "==> building amber (linux) + SMP initramfs"
docker run --rm -v "$ROOT:/work" -v "$WORK:/out" -w /work rust:bookworm bash -euc '
  apt-get update -qq >/dev/null && apt-get install -y -qq cpio gzip >/dev/null
  CARGO_TARGET_DIR=/work/target-linux cargo build --release -p amber 2>&1 | grep -E "^error|Finished" | tail -1
  AMBER=/work/target-linux/release/amber

  # inner guest: report the cpu count, then power off (exercises teardown).
  ii=/tmp/smp; mkdir -p $ii/bin $ii/lib $ii/dev $ii/proc
  cp /work/assets/irx/bin/busybox $ii/bin/busybox
  cp /work/assets/irx/lib/ld-musl-aarch64.so.1 $ii/lib/
  ln -sf ld-musl-aarch64.so.1 $ii/lib/libc.musl-aarch64.so.1
  printf "#!/bin/busybox sh\n/bin/busybox --install -s /bin\nmount -t proc proc /proc\nmount -t devtmpfs dev /dev\necho KVM_SMP_OK nproc=\$(nproc)\npoweroff -f\n" > $ii/init
  chmod +x $ii/init
  ( cd $ii && find . | cpio -o -H newc 2>/dev/null | gzip > /out/smp-inner.gz )

  oo=/tmp/outer-smp; mkdir -p $oo/bin $oo/lib $oo/dev $oo/proc $oo/sys
  cp /work/assets/irx/bin/busybox $oo/bin/busybox
  cp /work/assets/irx/lib/ld-musl-aarch64.so.1 $oo/lib/
  ln -sf ld-musl-aarch64.so.1 $oo/lib/libc.musl-aarch64.so.1
  cp $AMBER $oo/amber
  for lib in $(ldd $AMBER | grep -oE "/[^ ]+\.so[^ ]*" | sort -u); do
    mkdir -p "$oo$(dirname $lib)"; cp -L "$lib" "$oo$lib"
  done
  cp /work/assets/Image $oo/resin
  cp /out/smp-inner.gz $oo/smp-inner.gz
  # Two vcpus; the trailing amber-boot-rc only prints if amber exited (teardown ok).
  printf "#!/bin/busybox sh\n/bin/busybox --install -s /bin\nexport PATH=/bin\nmount -t proc proc /proc\nmount -t sysfs sysfs /sys\nmount -t devtmpfs dev /dev\necho ==SMP==\nAMBER_NET=none AMBER_VCPUS=2 RUST_LOG=info /amber boot /resin /smp-inner.gz < /dev/null\necho amber-boot-rc=\$?\npoweroff -f\n" > $oo/init
  chmod +x $oo/init
  ( cd $oo && find . | cpio -o -H newc 2>/dev/null | gzip > /out/smp-initramfs.gz )'

echo "==> booting (QEMU TCG, EL2-emulated; slow)…"
log="$WORK/smp-run.log"
qemu-system-aarch64 -M virt,virtualization=on,gic-version=3 -cpu max -accel tcg -smp 2 -m 4G \
  -kernel "$WORK/outer-Image" -initrd "$WORK/smp-initramfs.gz" -append "console=ttyAMA0" \
  -nographic -no-reboot > "$log" 2>&1 &
qp=$!
for _ in $(seq 1 400); do
  grep -qaE "amber-boot-rc=|Kernel panic|backend error" "$log" 2>/dev/null && break
  kill -0 $qp 2>/dev/null || break
  sleep 5
done
sleep 4; kill $qp 2>/dev/null || true; pkill -f qemu-system-aarch64 2>/dev/null || true

echo "--- markers ---"
grep -aE "==SMP==|KVM_SMP_OK|amber-boot-rc=|panic|backend error" "$log" || true

if grep -qa "KVM_SMP_OK nproc=2" "$log" && grep -qa "amber-boot-rc=0" "$log"; then
  echo ">>> PASS: 2-vcpu resin booted under KVM and tore down cleanly"
else
  echo ">>> FAIL — boot log (last 100 lines):"; tail -100 "$log"; exit 1
fi
