#!/usr/bin/env python3
# Drive the agent over a socketpair (no VM/vsock): send a command, read the
# stdout/stderr/exit frames, check the protocol. Runs inside the Docker builder
# where the aarch64 agent binary executes natively.
import os, socket, struct, subprocess, sys

AGENT = sys.argv[1] if len(sys.argv) > 1 else \
    "/agent/target/aarch64-unknown-linux-musl/release/amber-agent"

host, child = socket.socketpair()
proc = subprocess.Popen(
    [AGENT],
    env={**os.environ, "AMBER_AGENT_FD": str(child.fileno()), "PATH": "/usr/bin:/bin"},
    pass_fds=[child.fileno()],
    stdout=subprocess.DEVNULL,
)
child.close()  # the agent holds the other end now

# command reads stdin (cat), then writes stdout + stderr + a known exit code.
cmd = b'cat; echo " end"; echo err-line 1>&2; exit 7'
blob = b'in-blob'
host.sendall(struct.pack('<I', len(cmd)) + cmd + struct.pack('<I', len(blob)) + blob)

host.settimeout(10)
buf = b''
def recv_exact(n):
    global buf
    while len(buf) < n:
        chunk = host.recv(65536)
        if not chunk:
            return None
        buf += chunk
    d, buf = buf[:n], buf[n:]
    return d

out, err, rc = b'', b'', None
while rc is None:
    hdr = recv_exact(5)
    if hdr is None:
        break
    tag, ln = hdr[0], struct.unpack('<I', hdr[1:5])[0]
    payload = recv_exact(ln) if ln else b''
    if tag == 1: out += payload
    elif tag == 2: err += payload
    elif tag == 3: rc = struct.unpack('<i', payload)[0]

proc.wait(timeout=5)
print("STDOUT", repr(out), "STDERR", repr(err), "RC", rc)
ok = out == b'in-blob end\n' and err == b'err-line\n' and rc == 7
print("AGENT_PROTOCOL_OK" if ok else "AGENT_PROTOCOL_FAIL")
sys.exit(0 if ok else 1)
