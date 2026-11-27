#!/bin/sh
# Build the adapter within the pinned docs.rs workspace and its upstream lockfile.
set -eu

revision=f2201305fdb6da6a980bbe4ff5176d14e5030bc4
if [ "$#" -ne 2 ]; then
    echo "Usage: $0 PINNED_DOCS_RS_CHECKOUT OUTPUT_BINARY" >&2
    exit 1
fi
checkout=$(realpath "$1")
output=$(realpath -m "$2")
script=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
test "$(git -C "$checkout" rev-parse HEAD)" = "$revision"
work=$(mktemp -d /tmp/docs-mcp-builder.XXXXXXXX)
trap 'rm -rf -- "$work"' EXIT
git -C "$checkout" archive --format=tar HEAD > "$work/upstream.tar"
tar -xf "$work/upstream.tar" -C "$work"
mkdir -p "$work/crates/bin/docs_rs_build/src/bin"
cp "$script/build.rs" "$work/crates/bin/docs_rs_build/src/bin/docs-mcp-build.rs"
rustwide_version=0.29.0
rustwide_checksum=810d71fef848733737482293c26e124f9d9bf96a9ca6b4e81155571d0a9c180f
curl --proto '=https' --fail --silent --show-error --max-time 120 \
    "https://static.crates.io/crates/rustwide/rustwide-$rustwide_version.crate" -o "$work/rustwide.crate"
test "$(sha256sum "$work/rustwide.crate" | cut -d ' ' -f 1)" = "$rustwide_checksum"
mkdir "$work/docs-mcp-rustwide"
tar -xf "$work/rustwide.crate" --strip-components=1 -C "$work/docs-mcp-rustwide"
patch --directory "$work/docs-mcp-rustwide" --strip=1 --fuzz=0 < "$script/rustwide-offline.patch"
# Leave every upstream dependency at the locked version and checksum.
python3 - "$work" "$rustwide_version" "$rustwide_checksum" <<'PYLOCK'
from pathlib import Path
import sys

workspace, version, checksum = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
manifest = workspace / "Cargo.toml"
with manifest.open("a") as file:
    file.write('\n[patch.crates-io]\nrustwide = { path = "docs-mcp-rustwide" }\n')
lockfile = workspace / "Cargo.lock"
lock = lockfile.read_text()
package = f'[[package]]\nname = "rustwide"\nversion = "{version}"\n'
original = package + 'source = "registry+https://github.com/rust-lang/crates.io-index"\n' + f'checksum = "{checksum}"\n'
if lock.count(original) != 1:
    raise SystemExit("rustwide lock entry differs from the pinned upstream checksum")
lockfile.write_text(lock.replace(original, package))
PYLOCK
(
    cd "$work"
    export CARGO_TARGET_DIR=/tmp/rust/target
    export GIT_SHA="$revision"
    export TMPDIR="$work"
    cargo build --locked --release --package docs_rs_build --bin docs-mcp-build
)
install -m 0755 /tmp/rust/target/release/docs-mcp-build "$output"
