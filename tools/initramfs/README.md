# Minimal test initramfs

A tiny initramfs to exercise a booted guest: it runs busybox `sh`, prints
`uname`/`date`/uptime, sleeps 2s to prove the timer, and powers off.

Output is routed to `/dev/kmsg` because the emulated PL011 is earlycon-only for
now (no full `ttyAMA0` tty / RX), so writes to `/dev/console` are lost.

## Build

Needs a static-ish aarch64 busybox and the musl loader. The easy source is an
Alpine `initramfs-virt` (gzip cpio):

```sh
mkdir extract && (cd extract && gunzip -c /path/to/initramfs-virt | cpio -idm)

mkdir -p amberfs/bin amberfs/lib
cp extract/bin/busybox               amberfs/bin/
cp extract/lib/ld-musl-aarch64.so.1  amberfs/lib/
cp tools/initramfs/init              amberfs/init
chmod +x amberfs/init amberfs/bin/busybox

python3 tools/initramfs/build.py     # writes initramfs-amber.gz
```

`build.py` writes the cpio directly (newc), including `/dev/console`,
`/dev/null`, and `/dev/kmsg` device nodes — so it needs no root.

## Run

```sh
amber boot Image initramfs-amber.gz
```
