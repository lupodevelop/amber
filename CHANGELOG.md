# Changelog

## Unreleased

- vsock: host→guest connections now use monotonic ephemeral ports instead of
  restarting at 1024 each time. Immediate reuse collided with the just-closed
  connection the guest was still tearing down, which made every *other*
  sequential connect to a fork fail.

- `amber fork` now assigns each forked VM a host-side vsock UDS base and returns
  it (second stdout line; `Reply::Started.vsock`). A host peer can reach a guest
  port with `CONNECT <port>\n`. Also fixes the device-set mismatch on restore.
- `amber pull`/`run` accept a local `docker save` tar path (not just a registry
  reference): resolve, flatten, and pack it offline, no registry. Layer reader
  now sniffs gzip so plain (uncompressed) tar layers work too.
