# Changelog

## Unreleased

- `amber fork` now assigns each forked VM a host-side vsock UDS base and returns
  it (second stdout line; `Reply::Started.vsock`). A host peer can reach a guest
  port with `CONNECT <port>\n`. Also fixes the device-set mismatch on restore.
