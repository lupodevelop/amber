# Networking backends

How amber does guest networking, and the backends we may add. The model is
Firecracker's: a virtio-net device pumping Ethernet frames across a pluggable
host-side seam, adapted to macOS (no TAP) by swapping the backend. This doc is the
reference for implementing the not-yet-built backends.

## The seam

- `NetBackend` trait (`crates/amber-core/src/net.rs`): the only integration point.
  - `send(&mut self, frame: &[u8])`: a frame the guest transmitted (raw Ethernet).
  - `poll(&mut self) -> Option<Vec<u8>>`: next frame to hand the guest (non-blocking).
  - `mac(&self) -> [u8; 6]`: the MAC the device advertises.
  - `wants_poll(&self) -> bool`: true while async host-side work is pending, so the
    run loop polls eagerly (caps the idle park at about 1 ms) instead of waiting out
    the 50 ms backstop.
- virtio-net `NetDevice` (same file, device id 1): rx queue 0, tx queue 1. tx drains
  readable descriptors, strips the 12-byte virtio-net header, calls `send`. rx is
  filled asynchronously: `process` does not consume the rx queue on notify; the
  run loop calls `pump_rx` each iteration, which pulls a posted buffer and writes
  one `poll`-returned frame (prepending a zeroed header). See `virtio.rs`.
- Backends live in the `amber-net` crate, each behind a cargo feature so its deps
  are opt-in. `amber_net::backend(kind) -> Option<Box<dyn NetBackend>>`.
- Selection (`crates/amber/src/main.rs`, `cmd_vm`): the `AMBER_NET` env var.
  - unset → `smoltcp` (default on),
  - `none`/`off` → no device,
  - `capture` → the bring-up logger (`CaptureBackend` in amber-core),
  - anything else → `amber_net::backend(<kind>)`.
- Snapshot/fork: `meta.net` records whether a net device was present; `restore_from`
  recreates it with a fresh backend (host state is not snapshotted) when `meta.net`
  is set, keeping the device set aligned with the template. `amber template` builds
  with net so exec'd and forked sandboxes have it.
- Auto-config: the bootstrap init brings up `eth0` (static `10.0.0.2/24`, gateway
  `10.0.0.1`, `nameserver 10.0.0.1`) when a net device is present (gated on
  `/sys/class/net/eth0`). Addresses must match the backend.

A new backend only implements `NetBackend`; nothing above the seam changes.

## smoltcp: the default (built, rootless, in-process)

`crates/amber-net/src/lib.rs`, feature `smoltcp-backend` (default). amber is the
gateway `10.0.0.1`; the guest is static `10.0.0.2`. A smoltcp `phy::Device` bridges
two frame queues to an `Interface`.

- **ARP/ICMP**: answered by the interface for the gateway address (no socket).
- **Outbound TCP**: destination NAT. On a guest SYN to `ext:port`, dial a host
  `TcpStream` (on a thread, so the vcpu never blocks) and open a smoltcp socket
  listening on the gateway's own IP plus a fresh ephemeral port; rewrite
  guest→ext frames' dst to `gateway:eph` (IP and TCP checksums refilled) before
  smoltcp sees them, and rewrite the reply src back. This is needed because
  smoltcp's `any_ip` will not accept SYNs to a foreign destination on a listening
  socket.
- **DNS**: the gateway is the resolver. A smoltcp UDP socket on `10.0.0.1:53`
  forwards queries to a host resolver (`1.1.1.1:53`) and relays replies. No NAT.
- **Inbound / port-forward**: `AMBER_PORTS="hostport:guestport,..."`. A host
  `TcpListener` per mapping hands accepted streams to the netstack, which dials the
  guest and bridges. (Currently the boot path; the daemon would need to pass
  `AMBER_PORTS` to exec/fork workers.)
- **Latency**: `wants_poll()` is true while flows, queries, or listeners exist, so
  replies reach the guest within milliseconds.

**Limits (where the other backends win):** outbound is TCP and DNS only (no general
UDP NAT, no ICMP to external hosts, no IPv6); the TCP/poll path is hand-rolled
(about 300 lines) and basic; performance is fine for agent tasks, not tuned for
heavy throughput. Good enough for the common case: outbound HTTP/HTTPS/DNS plus an
inbound forward, self-contained and rootless.

## gvproxy: fuller netstack via a helper (not built)

[gvisor-tap-vsock](https://github.com/containers/gvisor-tap-vsock); the binary is
`gvproxy`. gVisor's production netstack does the NAT; amber is just a frame bridge.

- **Benefits**: complete NAT for any TCP/UDP/ICMP and IPv6; mature; better
  performance under load. Covers what smoltcp does not (arbitrary UDP,
  ICMP-to-external, IPv6).
- **Costs**: an external binary (breaks single-binary self-containment) and a
  gvproxy process per VM (or shared).
- **Implementation** (small, about 50 to 80 lines): a `GvproxyBackend` that
  connects to the unix socket gvproxy serves and speaks its vfkit/qemu framing,
  each Ethernet frame prefixed by a length (vfkit uses a 4-byte big-endian length;
  confirm against the gvproxy version). `send` writes `len ++ frame`; a reader
  thread reads `len ++ frame` and queues them for `poll`; the reader unparks and
  sets the poll flag so frames reach the guest promptly. Start gvproxy as a child
  (or require it running) with `--listen-vfkit unixgram://...` (check the exact
  flag and transport per release).
- **Testing**: install `gvproxy` (Go build or a release download), run it, point
  the backend at its socket, verify the guest reaches the internet (including a
  UDP or IPv6 case smoltcp cannot do).
- **Verdict**: add as an opt-in for users who need IPv6, arbitrary UDP, or
  throughput and accept the binary. Marginal for the core agent-sandbox case.

## vmnet: kernel NAT (not built; needs root or a helper)

Apple's `vmnet` framework (what Virtualization.framework, Docker Desktop, and lima
use).

- **Benefits**: NAT accelerated by the host kernel (faster than userspace), a real
  NIC's worth of protocols (TCP/UDP/ICMP/IPv6), and bridged mode (the guest gets
  its own IP on the real LAN). DHCP is provided by vmnet.
- **Costs/blockers**: vmnet shared/NAT mode requires root or the restricted
  entitlement `com.apple.vm.networking` (not grantable to an ad-hoc-signed binary).
  amber's model is an unprivileged daemon spawning one process per VM, so:
  - **(a) amber as root**: opens vmnet directly. Simple, but breaks the
    per-VM-unprivileged security model. Not recommended.
  - **(b) a `socket_vmnet`-style root helper** (the lima/colima pattern): a small
    setuid/root helper holds the vmnet interface and passes packets over a unix
    socket; amber connects unprivileged. Clean, but it is an external privileged
    component to ship and audit.
- **Implementation**: link the vmnet framework (`vmnet_start_interface` in
  `VMNET_NAT` or `VMNET_BRIDGED` mode; read/write via the dispatch-callback API or
  `vmnet_read`/`vmnet_write`). A `VmnetBackend` bridges those to `NetBackend`. With
  the helper approach, the backend instead speaks a unix-socket frame protocol to
  the helper (like gvproxy).
- **Testing**: requires `sudo` (mode a) or building the helper (mode b). Verify the
  guest gets a vmnet DHCP lease and full connectivity; test bridged mode
  separately.
- **Verdict**: supportable. Worth it for users who want kernel-speed, full
  protocols, or bridged-onto-LAN, and accept root or a helper. smoltcp already
  covers the rootless common case better for amber's purpose.

## TAP: the Linux/KVM backend (not built)

`/dev/net/tun`. This is Firecracker's actual backend and the natural companion to
the KVM backend on arm64 Linux. Not applicable on macOS.

- **Benefits**: kernel-fast L2; the standard, well-understood path; the host does
  NAT via iptables or a bridge (operator-configured, like Firecracker).
- **Costs**: Linux only; the host networking (NAT/routing) is set up outside amber
  (or via a CNI-style helper); typically needs `CAP_NET_ADMIN` to open the tap.
- **Implementation**: a `TapBackend` that opens a pre-created tap fd (or creates
  one), then reads and writes raw Ethernet frames, a near-trivial `NetBackend` (the
  fd is the L2 wire). The same `pump_rx`/`poll` plumbing applies.
- **Testing**: needs arm64 Linux hardware with `/dev/kvm` and `/dev/net/tun`. Do it
  alongside hardware validation of the KVM backend.
- **Verdict**: implement together with the KVM path on real hardware. The seam is
  already shaped for it.

## Decision matrix

| backend | when it wins | cost | testable here |
|---|---|---|---|
| **smoltcp** (default) | outbound HTTP/DNS plus inbound forward; the agent-sandbox case | none (in-process, rootless) | yes ✓ |
| **gvproxy** | IPv6, arbitrary UDP, ICMP-to-external, throughput | external binary | only if `gvproxy` installed |
| **vmnet** | kernel-speed, all protocols, bridged-onto-LAN | root or a root helper | only with `sudo` or a helper |
| **TAP** | Linux/KVM production | Linux plus `CAP_NET_ADMIN`; host NAT config | no (needs arm64 Linux) |

**Stance:** on macOS, smoltcp is the best fit for amber's goal (self-contained,
rootless, fast spawn) and covers the common case. gvproxy and vmnet are opt-in
options for specific needs (IPv6/UDP/throughput; kernel-speed/bridged), to add when
we can install and validate them. TAP lands with the KVM backend on real hardware.
None should displace smoltcp as the default; all plug into the unchanged
`NetBackend` seam.
