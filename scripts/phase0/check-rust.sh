#!/bin/sh
set -eu

export CARGO_TARGET_DIR=/tmp/rust/target
export TMPDIR
TMPDIR=$(mktemp -d /tmp/docs-mcp-rust.XXXXXXXX)
trap 'rmdir "$TMPDIR"' EXIT

cargo fmt --all -- --check
cargo clippy --workspace --all-features --all-targets --frozen -- -D warnings
cargo test --workspace --all-features --all-targets --frozen
cargo test --workspace --doc --frozen
