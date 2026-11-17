#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
if [ -z "${RUST_GATEWAY_BIN:-}" ]; then
    cargo build --locked --manifest-path ../Cargo.toml -p ictl-gateway --bin iphone-control-gateway
    task_target="${CARGO_TARGET_DIR:-../target}"
    RUST_GATEWAY_BIN="$(cd "$task_target/debug" && pwd)/iphone-control-gateway"
    export RUST_GATEWAY_BIN
fi
if [ ! -x "$RUST_GATEWAY_BIN" ]; then
    echo "RUST_GATEWAY_BIN must point to an executable Rust gateway" >&2
    exit 1
fi
exec node --test tests/rust-gateway-*.test.mjs
