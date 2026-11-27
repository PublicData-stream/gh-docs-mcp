#!/usr/bin/env bash
# Build disposable fixture roles and optionally the historical comparison images.
set -euo pipefail
cd "$(dirname "$0")/../.."
tag=${1:?fixture image tag required}
mode=${2:?comparison image tag or --current-only required}
roles=(serve sync feed postgres)
if [[ "$mode" == --current-only && ${3:-} == --smoke && $# == 3 ]]; then
    roles+=(phase2-upstream phase3-sync)
elif [[ $# == 2 && ( "$mode" != --* || "$mode" == --current-only ) ]]; then
    roles+=(phase1-serve phase1-sync phase2-sync phase2-upstream phase3-serve phase3-sync
            phase4-serve phase6-serve phase6-sync phase6-feed)
else
    echo "Usage: $0 TAG {PREVIOUS_TAG | --current-only [--smoke]}" >&2
    exit 2
fi
case "$(uname -m)" in
    x86_64) arch=amd64 ;;
    aarch64) arch=arm64 ;;
    *) exit 2 ;;
esac
args=()
if [[ -n ${DOCS_MCP_RELEASE_SEED:-} ]]; then
    args+=(--build-context "cargo-registry-seed=$DOCS_MCP_RELEASE_SEED")
fi
if [[ "$mode" != --current-only ]]; then
    work=$(mktemp -d /tmp/docsmcp8-fixture-build.XXXXXXXX)
    trap 'rm -rf -- "$work"' EXIT
    git archive 45db1afb5e034c5fb02d633bd0b6973ffab9be35 | tar -x -C "$work"
    for role in serve sync feed; do
        docker --context default buildx build --network host --allow network.host \
            --platform "linux/$arch" --load --target "$role" \
            -t "gh-docs-mcp-$role:$mode" "${args[@]}" "$work"
    done
fi
for role in "${roles[@]}"; do
    docker --context default buildx build --network host --allow network.host \
        --platform "linux/$arch" --load --target "$role" \
        -t "docs-mcp-$role:$tag" "${args[@]}" .
done
