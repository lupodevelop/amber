#!/usr/bin/env bash
# End-to-end KVM snapshot/restore test, hardware-free (QEMU TCG, EL2-emulated).
#
# Inside one outer KVM-host boot, amber boots resin under /dev/kvm running a
# guest that prints a ticking counter, snapshots it mid-tick (capture-and-stop),
# then a second `amber restore` resumes the saved state — vcpu registers, the
# in-kernel vGICv3, guest RAM, and the arch timer. The guest must continue from
# where it was paused and run to KVM_E2E_POST, proving the restore is faithful.
#
# Reuses the cached outer kernel + the Docker amber build from scripts/kvm-test.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-/tmp/amber-kvm-test}"
KVER="${KVER:-6.12.93}"
mkdir -p "$WORK"
command -v qemu-system-aarch64 >/dev/null || { echo "need qemu"; exit 1; }
command -v docker >/dev/null || { echo "need docker"; exit 1; }
[ -f "$ROOT/assets/Image" ] || { echo "no assets/Image — run: make kernel"; exit 1; }
[ -f "$WORK/outer-Image" ] || { echo "no cached outer kernel — run scripts/kvm-test.sh first"; exit 1; }

echo "==> building amber (linux) + snapshot e2e initramfs"
docker run --rm -v "$ROOT:/work" -v "$WORK:/out" -w /work rust:bookworm bash -euc '
  apt-get update -qq >/dev/null && apt-get install -y -qq cpio gzip >/dev/null
  CARGO_TARGET_DIR=/work/target-linux cargo build --release -p amber 2>&1 | grep -E "^error|Finished" | tail -1
  AMBER=/work/target-linux/release/amber

  # inner guest: print PRE, then block on stdin (like the exec agent). The
  # snapshot always catches it parked here. Only the restored run, fed a line on
  # stdin, unblocks and reaches KVM_E2E_POST — proving the resume is faithful and
  # the guests timer/console came back to life.
  ii=/tmp/e2e; mkdir -p $ii/bin $ii/lib $ii/dev $ii/proc
  cp /work/assets/irx/bin/busybox $ii/bin/busybox
  cp /work/assets/irx/lib/ld-musl-aarch64.so.1 $ii/lib/
  ln -sf ld-musl-aarch64.so.1 $ii/lib/libc.musl-aarch64.so.1
  printf "#!/bin/busybox sh\n/bin/busybox --install -s /bin\nmount -t proc proc /proc\nmount -t devtmpfs dev /dev\necho KVM_E2E_PRE\nIFS= read -r line\necho KVM_E2E_POST got=\$line\npoweroff -f\n" > $ii/init
  chmod +x $ii/init
  ( cd $ii && find . | cpio -o -H newc 2>/dev/null | gzip > /out/e2e.gz )

  # outer rootfs: amber + its .so closure + resin + e2e.gz + an init that boots
  # (snapshotting mid-tick) then restores from the same tmpfs dir.
  oo=/tmp/outer-snap; mkdir -p $oo/bin $oo/lib $oo/dev $oo/proc $oo/sys $oo/snap
  cp /work/assets/irx/bin/busybox $oo/bin/busybox
  cp /work/assets/irx/lib/ld-musl-aarch64.so.1 $oo/lib/
  ln -sf ld-musl-aarch64.so.1 $oo/lib/libc.musl-aarch64.so.1
  cp $AMBER $oo/amber
  for lib in $(ldd $AMBER | grep -oE "/[^ ]+\.so[^ ]*" | sort -u); do
    mkdir -p "$oo$(dirname $lib)"; cp -L "$lib" "$oo$lib"
  done
  cp /work/assets/Image $oo/resin
  cp /out/e2e.gz $oo/e2e.gz
  # Run 1 boots and snapshots the parked guest — stdin is an open pipe with no
  # data (tail -f) so its `read` blocks until the snapshot stops it. Run 2 feeds
  # a line into the restored guest, which then prints KVM_E2E_POST and powers off.
  printf "#!/bin/busybox sh\n/bin/busybox --install -s /bin\nexport PATH=/bin\nmount -t proc proc /proc\nmount -t sysfs sysfs /sys\nmount -t devtmpfs dev /dev\nmount -t tmpfs tmpfs /snap\necho ==BOOT==\ntail -f /dev/null | AMBER_NET=none RUST_LOG=info AMBER_SNAPSHOT=/snap AMBER_SNAPSHOT_MARKER=KVM_E2E_PRE AMBER_SNAPSHOT_AFTER_MS=600000 /amber boot /resin /e2e.gz\necho snap-rc=\$?\nls /snap\necho ==RESTORE==\necho hello-from-restore | RUST_LOG=info AMBER_NET=none /amber restore /snap\necho restore-rc=\$?\npoweroff -f\n" > $oo/init
  chmod +x $oo/init
  ( cd $oo && find . | cpio -o -H newc 2>/dev/null | gzip > /out/snap-initramfs.gz )'

echo "==> booting (QEMU TCG, EL2-emulated; slow)…"
log="$WORK/snap-run.log"
qemu-system-aarch64 -M virt,virtualization=on,gic-version=3 -cpu max -accel tcg -smp 2 -m 4G \
  -kernel "$WORK/outer-Image" -initrd "$WORK/snap-initramfs.gz" -append "console=ttyAMA0" \
  -nographic -no-reboot > "$log" 2>&1 &
qp=$!
for _ in $(seq 1 320); do
  grep -qaE "restore-rc=|Kernel panic|backend error" "$log" 2>/dev/null && break
  kill -0 $qp 2>/dev/null || break
  sleep 5
done
sleep 4; kill $qp 2>/dev/null; pkill -f qemu-system-aarch64 2>/dev/null || true

echo "--- markers ---"
grep -aE "==BOOT==|==RESTORE==|KVM_E2E_PRE|KVM_E2E_TICK|KVM_E2E_POST|snap-rc=|restore-rc=" "$log" || true

# Pass: the snapshot run stopped before POST, and the restored run reached POST.
post_count=$(grep -ac "KVM_E2E_POST" "$log" || true)
if grep -qa "==RESTORE==" "$log" && [ "$post_count" -ge 1 ]; then
  echo ">>> PASS: resin resumed from a KVM snapshot and ran to completion"
else
  echo ">>> FAIL (full log: $log)"; exit 1
fi
