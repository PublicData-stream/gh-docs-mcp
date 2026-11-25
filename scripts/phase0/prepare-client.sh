#!/bin/sh
set -eu
source_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/client" && pwd)
client_dir=$(mktemp -d /tmp/gh-docs-client.XXXXXXXX)
trap 'rm -rf -- "$client_dir"' EXIT
package_manager=$(node -p 'require(process.argv[1]).packageManager' "$source_dir/package.json")
actual_package_manager="pnpm@$(pnpm --version)"
if [ "$actual_package_manager" != "$package_manager" ]; then
    printf 'Expected %s, got %s\n' "$package_manager" "$actual_package_manager" >&2
    exit 1
fi
cp "$source_dir/package.json" "$source_dir/pnpm-lock.yaml" "$source_dir/probe.mjs" "$client_dir/"
pnpm --dir "$client_dir" install --frozen-lockfile --ignore-scripts >&2
trap - EXIT
printf '%s\n' "$client_dir"
