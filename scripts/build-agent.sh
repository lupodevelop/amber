#!/usr/bin/env bash
# Build the in-guest exec agent as a static aarch64-linux binary into
# assets/amber-agent. On Apple Silicon the Docker container is arm64, so this is
# a native musl build, not a cross-compile.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
command -v docker >/dev/null || { echo "need docker"; exit 1; }

echo "==> building amber-agent (aarch64-musl, static)"
docker run --rm -v "$ROOT/agent:/agent" -w /agent rust:bookworm bash -euc '
  rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1
  CARGO_TARGET_DIR=/agent/target cargo build --release --target aarch64-unknown-linux-musl 2>&1 \
    | grep -E "^error|Finished" | tail -1
  cp /agent/target/aarch64-unknown-linux-musl/release/amber-agent /agent/amber-agent.out'

mkdir -p "$ROOT/assets"
mv "$ROOT/agent/amber-agent.out" "$ROOT/assets/amber-agent"
size=$(wc -c < "$ROOT/assets/amber-agent")
file "$ROOT/assets/amber-agent" 2>/dev/null || true
echo "==> assets/amber-agent ($size bytes)"
