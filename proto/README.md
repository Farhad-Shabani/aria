# Protocols

`aria/daemon/v1` groups daemon services and their messages by capability.
`aria/setup/v1` carries length-delimited setup messages over stdio.
`grpc/health/v1` retains the upstream health contract.

The daemon serves gRPC, health, and reflection (v1 and v1alpha) over a
per-user Unix socket protected by filesystem permissions. Setup messages are
excluded from daemon reflection.

Paired phones reach relay, preferences, and health over TLS with a pinned
client certificate. Pairing exchanges a one-time code for that pin. LAN
preference writes are restricted to relay settings; other daemon services
stay local. The inference host has a separate TLS service and local admin
socket, described in `inference.proto`.

Run `just proto-check` for formatting and lint, `just proto-breaking` to
compare with `origin/main`, and `just proto-swift` to regenerate Swift clients.
Rust generates its bindings during the build.
