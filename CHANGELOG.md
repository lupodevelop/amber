# Changelog

## Unreleased

- `amber fork` now assigns each forked VM a host-side vsock UDS base and returns
  it (second stdout line; `Reply::Started.vsock`). A host peer can reach a guest
  port with `CONNECT <port>\n`. Also fixes the device-set mismatch on restore.
- `amber pull`/`run` accept a local `docker save` tar path (not just a registry
  reference): resolve, flatten, and pack it offline, no registry. Layer reader
  now sniffs gzip so plain (uncompressed) tar layers work too.
