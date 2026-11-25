#!/usr/bin/env bash
# Build disposable fixture roles and the Phase 7 comparison images.
set -euo pipefail
cd "$(dirname "$0")/../.."
tag=${1:?fixture image tag required}
previous=${2:?comparison image tag required}
case "$(uname -m)" in
    x86_64) arch=amd64 ;;
    aarch64) arch=arm64 ;;
    *) exit 2 ;;
esac
work=$(mktemp -d /tmp/ghdocs8-fixture-build.XXXXXXXX)
cleanup() { rm -rf -- "$work"; }
trap cleanup EXIT
previous_revision=45db1afb5e034c5fb02d633bd0b6973ffab9be35
mkdir "$work/previous"
git archive "$previous_revision" | tar -x -C "$work/previous"

args=()
if [[ -n ${GH_DOCS_RELEASE_SEED:-} ]]; then
    args+=(--build-context "cargo-registry-seed=$GH_DOCS_RELEASE_SEED")
fi
for role in serve sync feed; do
    docker --context default buildx build --network host --allow network.host \
        --platform "linux/$arch" --load --target "$role" \
        -t "gh-docs-mcp-$role:$previous" "${args[@]}" "$work/previous"
done
for role in serve sync feed phase1-serve phase1-sync phase2-sync phase2-upstream \
            phase3-serve phase3-sync phase4-serve phase6-serve phase6-sync phase6-feed; do
    prefix=gh-docs
    case "$role" in serve|sync|feed) prefix=gh-docs-mcp ;; esac
    docker --context default buildx build --network host --allow network.host \
        --platform "linux/$arch" --load --target "$role" \
        -t "$prefix-$role:$tag" "${args[@]}" .
done
