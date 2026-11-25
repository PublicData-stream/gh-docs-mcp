# Phase 0 prerequisites and qualification

Use Linux `amd64`, rootful Docker with Compose and Buildx, nftables, ext4-backed `/tmp`, Git, Python 3, OpenSSL, Node `26.13.1`, pnpm as pinned in the [client manifest](../scripts/phase0/client/package.json), and Rust `1.100.0`. The harness uses root for disposable bind-mount ownership and project-scoped firewall rules. It runs the TypeScript client as the invoking user. Container processes use their declared role UIDs.

Prefetch the locked inputs and run the qualification from the repository root.

```sh
export CARGO_TARGET_DIR=/tmp/rust/target
cargo fetch --locked
scripts/phase0/check-rust.sh

client_dir=$(scripts/phase0/prepare-client.sh)
docker --context default pull docker.io/library/alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf
docker --context default pull docker.io/library/postgres:19.0-alpine3.24@sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f
docker --context default pull docker.io/library/nginx:1.30.0-alpine@sha256:0272e4604ed93c1792f03695a033a6e8546840f86e0de20a884bb17d2c924883

set -o pipefail
sudo -n python3 -B scripts/phase0/integration.py --node "$(command -v node)" --client-dir "$client_dir" \
  2>&1 | tee /tmp/gh-docs-phase0-integration.log
```

The harness builds the three exact service images. Build-time downloads use host networking. Git, ripgrep, and workspace compilation run with networking disabled. Application runtime networks are created for one disposable Compose project, with temporary data, certificates, and secrets under `/tmp`. The harness verifies that every pinned image is present before it creates project resources. It installs nftables rules before starting services and stops the project before deleting its rules. Use the harness to start the Phase 0 stack so the egress policy is in effect.

The private topology uses separate ingress, database read and write, notification, feed-control, and sync/feed egress bridges. The positive egress cases are the synthetic sink on the approved addresses and port. The proxy publishes one port on host loopback. PostgreSQL binds both database interfaces and uses the private Unix socket for provisioning. Its certificate and private key are supplied separately from app and migration credentials.

Requalify network filtering, UID mapping, filesystem behavior, and the actual worker confinement on any new host profile. For rootless Docker, read the actual user namespace's `uid_map` and `gid_map` before provisioning bind mounts. Map each in-container UID and GID through those ranges and test the resulting effective ownership. On this host, the rootless UID and GID maps were both `0 1000 1` and `1 100000 65536`. That maps the three application UIDs and GIDs to `110000`, `110001`, and `110002` on the host. Rootless egress needs its own enforcement qualification. The Phase 0 fsync and flock tests exercise the running local filesystem. Power-loss durability and multi-host locks require separate evidence.

The serve and sync images use Git `2.54.1` built from its SHA-256-checked source archive with its bundled regex implementation for musl and the default collision-detecting SHA-1 implementation. Sync uses its restricted transport broker for HTTPS. The search worker uses ripgrep `16.0.0` at `669cb4c755db20b26c3af3d923c042ae224c7496`, with PCRE2 disabled. See the [RFC baseline notice](RFC-MCP-DOCKER-LICENSE) for adapted code. Runtime Git and ripgrep notices are installed in their consuming images. Qualify distribution obligations before publishing images.

[Git release archive](https://www.kernel.org/pub/software/scm/git/) · [Ripgrep release source](https://github.com/BurntSushi/ripgrep/commit/669cb4c755db20b26c3af3d923c042ae224c7496) · [Docker rootless mode](https://docs.docker.com/engine/security/rootless/)
