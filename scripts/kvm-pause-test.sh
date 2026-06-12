#!/usr/bin/env bash
# Live pause/resume test for the KVM backend, hardware-free.
#
# The guest blocks on `read` (so its vcpu is WFI-halted inside KVM_RUN — the case
# that needs the SIGUSR1 kick to reach the pause gate). The harness sends PAUSE on
# the control fd, then feeds a line of input: while paused the console_reader
# queues it into the UART RX, but the parked vcpu can't consume it, so no `GOT=`
# is produced. After RESUME the vcpu runs, reads the queued line, and prints it.
# PASS = zero GOT while paused, exactly one after resume.
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

echo "==> building amber (linux) + pause initramfs"
docker run --rm -v "$ROOT:/work" -v "$WORK:/out" -w /work rust:bookworm bash -euc '
  apt-get update -qq >/dev/null && apt-get install -y -qq cpio gzip >/dev/null
  CARGO_TARGET_DIR=/work/target-linux cargo build --release -p amber 2>&1 | grep -E "^error|Finished" | tail -1
  AMBER=/work/target-linux/release/amber

  # inner guest: announce readiness, block on a line, echo it back, power off.
  ii=/tmp/pause; mkdir -p $ii/bin $ii/lib $ii/dev $ii/proc
  cp /work/assets/irx/bin/busybox $ii/bin/busybox
  cp /work/assets/irx/lib/ld-musl-aarch64.so.1 $ii/lib/
  ln -sf ld-musl-aarch64.so.1 $ii/lib/libc.musl-aarch64.so.1
  printf "#!/bin/busybox sh\n/bin/busybox --install -s /bin\nmount -t proc proc /proc\nmount -t devtmpfs dev /dev\necho PAUSE_READY\nIFS= read -r line\necho GOT=\$line\npoweroff -f\n" > $ii/init
  chmod +x $ii/init
  ( cd $ii && find . | cpio -o -H newc 2>/dev/null | gzip > /out/pause-inner.gz )

  oo=/tmp/outer-pause; mkdir -p $oo/bin $oo/lib $oo/dev $oo/proc $oo/sys
  cp /work/assets/irx/bin/busybox $oo/bin/busybox
  cp /work/assets/irx/lib/ld-musl-aarch64.so.1 $oo/lib/
  ln -sf ld-musl-aarch64.so.1 $oo/lib/libc.musl-aarch64.so.1
  cp $AMBER $oo/amber
  for lib in $(ldd $AMBER | grep -oE "/[^ ]+\.so[^ ]*" | sort -u); do
    mkdir -p "$oo$(dirname $lib)"; cp -L "$lib" "$oo$lib"
  done
  cp /work/assets/Image $oo/resin
  cp /out/pause-inner.gz $oo/pause-inner.gz
  # Control fd 3 <> /ctl (rw, non-blocking); guest stdin from /in (held open via
  # fd 4 so its read blocks); amber output to /out, which the harness inspects.
  cat > $oo/init <<"INIT"
#!/bin/busybox sh
/bin/busybox --install -s /bin
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs dev /dev
echo ==PAUSE==
mkfifo /ctl /in
( AMBER_NET=none AMBER_VCPUS=1 AMBER_CONTROL_FD=3 RUST_LOG=info /amber boot /resin /pause-inner.gz 3<>/ctl < /in > /out 2>&1; echo "amber-rc=$?" >> /out ) &
exec 4>/in
i=0; while [ $i -lt 60 ]; do grep -qa PAUSE_READY /out 2>/dev/null && break; sleep 1; i=$((i+1)); done
printf '\x02' > /ctl
sleep 6
printf 'hello\n' >&4
sleep 8
echo "DURING_PAUSE_GOT=$(grep -ca 'GOT=' /out)"
printf '\x03' > /ctl
sleep 8
echo "AFTER_RESUME_GOT=$(grep -ca 'GOT=' /out)"
echo "GUEST_OUT:"; grep -aE 'PAUSE_READY|GOT=' /out || true
poweroff -f
INIT
  chmod +x $oo/init
  ( cd $oo && find . | cpio -o -H newc 2>/dev/null | gzip > /out/pause-initramfs.gz )'

echo "==> booting (QEMU TCG, EL2-emulated; slow)…"
log="$WORK/pause-run.log"
qemu-system-aarch64 -M virt,virtualization=on,gic-version=3 -cpu max -accel tcg -smp 2 -m 4G \
  -kernel "$WORK/outer-Image" -initrd "$WORK/pause-initramfs.gz" -append "console=ttyAMA0" \
  -nographic -no-reboot > "$log" 2>&1 &
qp=$!
for _ in $(seq 1 400); do
  grep -qaE "AFTER_RESUME_GOT=|Kernel panic|backend error" "$log" 2>/dev/null && break
  kill -0 $qp 2>/dev/null || break
  sleep 5
done
sleep 4; kill $qp 2>/dev/null; pkill -f qemu-system-aarch64 2>/dev/null || true

echo "--- markers ---"
grep -aE "==PAUSE==|DURING_PAUSE_GOT=|AFTER_RESUME_GOT=|GOT=|PAUSE_READY|panic|backend error" "$log" || true

if grep -qa "DURING_PAUSE_GOT=0" "$log" && grep -qa "AFTER_RESUME_GOT=1" "$log"; then
  echo ">>> PASS: vcpu froze under PAUSE and processed the queued input only after RESUME"
else
  echo ">>> FAIL (full log: $log)"; exit 1
fi
