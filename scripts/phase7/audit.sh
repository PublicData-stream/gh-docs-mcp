#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
export CARGO_TARGET_DIR=/tmp/rust/target TMPDIR=/tmp
umask 077

tag=${1:?Pass the qualification image tag}
out=$(realpath -m "${2:?Pass a fresh evidence directory under /tmp}")
archive=${3:?Pass the Trivy 0.76.2 Linux amd64 archive}
rustsec=${4:?Pass a RustSec advisory database checkout}
ripgrep_lock=${5:?Pass Cargo.lock from the Dockerfile-pinned ripgrep source archive}
test "${out#/tmp/}" != "$out"
test "$(uname -m)" = x86_64
mkdir "$out"
work=$(mktemp -d /tmp/gh-docs-phase7-audit.XXXXXXXX)
trap 'rm -rf -- "$work"' EXIT

checksum=bc0fc726e017b8927a62b2d3531d7a84ee01769daf5c99ed866dced54089fdbf
printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --strict
tar -xzf "$archive" -C "$work" trivy

git rev-parse HEAD > "$out/revision.txt"
git -C "$rustsec" log -1 --format='%H %cI' > "$out/rustsec.txt"
cargo audit --version >> "$out/rustsec.txt"
sha256sum Cargo.lock "$ripgrep_lock" > "$out/locks.sha256"

for name in workspace ripgrep; do
    lock=Cargo.lock
    if [[ $name == ripgrep ]]; then
        lock=$ripgrep_lock
    fi
    exit_code=0
    cargo audit --db "$rustsec" --no-fetch --file "$lock" --json \
        > "$out/$name-rustsec.json" || exit_code=$?
    printf '%s %s\n' "$name" "$exit_code" >> "$out/rustsec-exit.txt"
    python3 -B -c 'import json, sys; assert "vulnerabilities" in json.load(open(sys.argv[1]))' \
        "$out/$name-rustsec.json"
done

docker=(docker --context default)
docker_host=$("${docker[@]}" context inspect default --format '{{.Endpoints.docker.Host}}')
cache="$work/cache"
if [[ -n ${PHASE7_TRIVY_DB:-} ]]; then
    mkdir "$cache"
    cp -a -- "${PHASE7_TRIVY_DB:?}/db" "$cache/db"
fi
scan() {
    DOCKER_HOST="$docker_host" TRIVY_SKIP_VERSION_CHECK=true \
        "$work/trivy" --config '' --cache-dir "$cache" "$@"
}
if [[ -z ${PHASE7_TRIVY_DB:-} ]]; then
    scan image --download-db-only
fi
scan version --format json > "$out/trivy-version.json"

proxy=$(sed -n 's/^    image: &proxy-image \(docker\.io\/library\/nginx:.*\)$/\1/p' compose.yml)
initializer=$(sed -n 's/^    image: \(docker\.io\/library\/alpine:.*\)$/\1/p' compose.yml)
for role in serve sync feed postgres proxy initializer; do
    case "$role" in
        proxy) image=$proxy ;;
        initializer) image=$initializer ;;
        *) image=gh-docs-mcp-$role:$tag ;;
    esac
    id=$("${docker[@]}" image inspect --format '{{.Id}}' "$image")
    printf '%s %s %s\n' "$role" "$image" "$id" >> "$out/images.txt"
    scan image --image-src docker --skip-db-update --format cyclonedx \
        --output "$out/$role.cdx.json" "$id"
    scan image --image-src docker --skip-db-update --scanners vuln --format json \
        --output "$out/$role-vulnerabilities.json" "$id"
done
