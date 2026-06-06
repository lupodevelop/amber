# Minimal test initramfs

A tiny initramfs to exercise a booted guest: it mounts the basics and execs an
interactive busybox `sh` on `ttyAMA0`. Boot it and type commands; `poweroff -f`
exits.

## Build

Needs an aarch64 busybox and the musl loader. The easy source is an Alpine
`initramfs-virt` (gzip cpio):

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
`/dev/null`, and `/dev/kmsg` device nodes, so it needs no root.

## Run

```sh
amber boot Image initramfs-amber.gz
```

The host terminal goes into raw mode; keystrokes reach the guest console and the
guest's tty echoes them. Piped stdin works too, but feed input after the boot
reaches the prompt — bytes delivered before the shell opens the console are
consumed by the kernel, not the shell.
