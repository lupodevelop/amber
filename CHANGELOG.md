# Changelog

## 0.1.0

- `amber fork` now assigns each forked VM a host-side vsock UDS base and returns
  it (a second stdout line; `Reply::Started.vsock`). A host peer reaches a guest
  port by connecting that socket and sending `CONNECT <port>\n`. This also fixes
  the device-set mismatch when restoring a snapshot that carries a vsock device.
- `amber pull` and `amber run` accept a local `docker save` tar path, not only a
  registry reference: amber resolves, flattens, and packs it offline with no
  registry. The layer reader sniffs gzip, so uncompressed tar layers work too.
- vsock host-to-guest connections use monotonic ephemeral ports instead of
  restarting at 1024 every time. Immediate reuse collided with the just-closed
  connection the guest was still tearing down, which made every other sequential
  connect to a fork fail.
