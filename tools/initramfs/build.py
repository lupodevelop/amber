import os, struct, sys
root="amberfs"; out="initramfs-amber.gz"
buf=bytearray(); ino=[0]
def rec(name, mode, data=b"", rdev=(0,0), nlink=1):
    ino[0]+=1
    name_b=name.encode()+b"\0"
    hdr="070701"+"".join("%08x"%v for v in [
        ino[0], mode, 0,0, nlink, 0, len(data),
        0,0, rdev[0], rdev[1], len(name_b), 0])
    b=hdr.encode()+name_b
    b+=b"\0"*((-len(b))%4)
    b+=data
    b+=b"\0"*((-len(data))%4)
    buf.extend(b)
S_IFDIR=0o040000; S_IFREG=0o100000; S_IFLNK=0o120000; S_IFCHR=0o020000
# dirs
for d in ["bin","lib","dev","proc","sys"]:
    rec(d, S_IFDIR|0o755)
# files
for f,perm in [("bin/busybox",0o755),("lib/ld-musl-aarch64.so.1",0o755),("init",0o755)]:
    with open(os.path.join(root,f),"rb") as fh: data=fh.read()
    rec(f, S_IFREG|perm, data)
# symlink libc -> loader
rec("lib/libc.musl-aarch64.so.1", S_IFLNK|0o777, b"ld-musl-aarch64.so.1")
# device nodes
rec("dev/console", S_IFCHR|0o600, rdev=(5,1))
rec("dev/null",    S_IFCHR|0o666, rdev=(1,3))
rec("dev/kmsg",    S_IFCHR|0o644, rdev=(1,11))
# trailer
name_b=b"TRAILER!!!\0"
hdr="070701"+"".join("%08x"%v for v in [0,0,0,0,1,0,0,0,0,0,0,len(name_b),0])
b=hdr.encode()+name_b; b+=b"\0"*((-len(b))%4); buf.extend(b)
import gzip
with gzip.open(out,"wb") as g: g.write(buf)
print("wrote",out,len(buf),"bytes cpio")
