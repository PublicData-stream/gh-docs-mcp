#!/bin/sh
set -eu
source_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/client" && pwd)
client_dir=$(mktemp -d /tmp/gh-docs-client.XXXXXXXX)
trap 'rm -rf -- "$client_dir"' EXIT
test "$(pnpm --version)" = 12.21.0
cp "$source_dir/package.json" "$source_dir/pnpm-lock.yaml" "$source_dir/probe.mjs" "$client_dir/"
pnpm --dir "$client_dir" install --frozen-lockfile --ignore-scripts >&2
trap - EXIT
printf '%s\n' "$client_dir"
