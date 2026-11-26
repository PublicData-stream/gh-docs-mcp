#!/usr/bin/env bash
# Run the existing Compose gates against one local fixture image tag.
set -euo pipefail
cd "$(dirname "$0")/../.."
tag=${1:?fixture image tag required}
evidence=${2:?evidence directory under /tmp required}
node=${3:?pinned Node executable required}
client=${4:?prepared SDK fixture required}
shift 4
if [[ $# == 0 ]]; then
    set -- {0..7}
fi
for phase do
    if [[ ! "$phase" =~ ^[0-7]$ ]]; then
        printf 'Invalid Compose phase: %s\n' "$phase" >&2
        exit 2
    fi
done
[[ $(realpath "$evidence") == /tmp/* ]]
mkdir -p "$evidence"

status=0
for phase do
    args=(--image-tag "$tag")
    case "$phase" in
        0) args+=(--skip-build --node "$node" --client-dir "$client") ;;
        1|5) args+=(--node "$node" --client-dir "$client") ;;
    esac
    printf 'Phase %s: %s\n' "$phase" "${args[*]}"
    # Keep the logs owned by the invoking user for artifact upload.
    # shellcheck disable=SC2024
    if sudo -n python3 -B "scripts/phase$phase/integration.py" "${args[@]}" \
            > "$evidence/phase$phase.log" 2>&1; then
        echo "Phase $phase passed"
    else
        status=1
        tail -60 "$evidence/phase$phase.log"
    fi
done
exit "$status"
