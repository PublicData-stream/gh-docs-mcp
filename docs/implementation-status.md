# Implementation status

## Phase 0 qualification

The local Phase 0 gate ran on 2026-11-24 at 14:04:40 UTC against signed implementation commit `0e6a524a1c1cd3ba5e84d8bbb419373e22a2c13f`. The declared profile was Linux `amd64`, kernel `7.0.0-44-generic`, rootful Docker Engine `29.10.0`, Compose `v5.8.0`, and ext4-backed `/tmp`. Rust was `1.100.0`, Python `3.14.4`, Node `26.13.1`, and pnpm `12.21.0`.

| Gate | Expected | Observed | Result |
| --- | --- | --- | --- |
| A01 | Exact stateless MCP `2026-07-28` discovery, list, and call through the private TLS boundary | Raw HTTP and TypeScript client `2.4.0` completed all three. Rejection cases covered unsupported revisions, lifecycle requests, malformed framing, standard headers, Host and Origin, and forwarded host spoofing. | Pass |
| A02 | Separate migration authority, SELECT-only serving, scoped sync DML, and verified DB TLS | DDL, temp tables and private relations were denied to runtime roles. An explicitly writable serve transaction still lacked DML. Sync DML and read-only advisory locking passed. All three app-side roles used SCRAM. Wrong CA, wrong hostname, and plaintext were rejected. Stored and actual ICU `und` collation versions both reported `153.136`. | Pass |
| A03 | Effective role, secret, mount, ingress, and egress isolation | The rebuilt stack enforced the configured UIDs, caps, read-only roots and mounts, private listeners, secrets, and the proxy-only ingress path. Synthetic IPv4 and IPv6 allowances and denials passed, including routed and host attempts. A shared lock on serve's read-only mount blocked sync's exclusive lock. File and directory fsync succeeded. | Pass |
| A04 | Confine the Git and search workers before they process input | Both real tools completed permitted reads. The tests denied unrelated paths, secrets, file creation, inherited sockets, new sockets, process creation, and unapproved execution. Both profiles failed closed on an injected Landlock kernel error. The effective Landlock ABI was 8. A separate synthetic bare SHA-256 read passed. | Pass |
| Offline Rust | Format, Clippy with warnings denied, workspace tests, and doc tests | The locked Rust checks and fixture registry test passed. | Pass |

The selected SHA-1 fixture commit was `e6493d41a3705cebe0c2d6659c1b7242dc99d998`. The SHA-256 fixture commit was `e935251f5b6c110125b57fdf8e2684244e89feeb76c36a1ee1964a46863e4f95`. All inputs and credentials were synthetic. The missing-image preflight rejected unavailable images before creating any containers or networks.

## Exact inputs and local evidence

| Input | Pin or observed image digest |
| --- | --- |
| Rust builder | `rust:1.100.0-alpine3.24@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33` |
| Alpine runtime and initializer | `alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf` |
| PostgreSQL | `postgres:19.0-alpine3.24@sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f` |
| TLS proxy and sink | `nginx:1.30.0-alpine@sha256:0272e4604ed93c1792f03695a033a6e8546840f86e0de20a884bb17d2c924883` |
| Rebuilt serve | `sha256:e3ff84c955ac7faaa923fa50d5d2f96ee41874cd3569d2d5af916d1344be80e0` |
| Rebuilt sync | `sha256:155f8b4977618b44d6ba097fac440a57d693178e95e0203f36bd8484aa1a0fed` |
| Rebuilt feed | `sha256:af6237fa535aeb06ac6314ce9b42b7fbfafdcbb2d8b059825d2b013be00b22ab` |
| Git and CA bundle | Git `2.54.1`, with `SHA1_DC` and `SHA256_BLK` verified in the static reader. Alpine Git package `2.54.1-r0` and CA bundle `20261020-r0`. |
| Search | ripgrep `16.0.0`, source commit `669cb4c755db20b26c3af3d923c042ae224c7496`, verified with PCRE2 disabled. |
| MCP clients and server | `rmcp =3.11.0`, `@modelcontextprotocol/client =2.4.0`, published spec revision `2026-07-28`. |
| RFC reference | `bcb4181cb044fcc0c48c1ebb055cedc7cb19a681` in the sibling RFC checkout. Attribution is in [the preserved notice](RFC-MCP-DOCKER-LICENSE). |

```sh
export CARGO_TARGET_DIR=/tmp/rust/target
cargo fetch --locked
scripts/phase0/check-rust.sh > /tmp/gh-docs-phase0-rust-0e6a524.log 2>&1
client_dir=/tmp/gh-docs-client.E6sCIu6Q
sudo -n python3 -B scripts/phase0/integration.py --node /home/code/.nvm/versions/node/v26.13.1/bin/node \
  --client-dir "$client_dir" > /tmp/gh-docs-phase0-0e6a524.log 2>&1
```

The Compose transcript includes the exact Buildx commands. The missing-image check is recorded at `/tmp/gh-docs-phase0-missing-image.log`. The rootless UID/GID mapping observation is at `/tmp/gh-docs-phase0-rootless-map.log`.

The Phase 0 gate passed on the declared local profile. Hosted CI, ARM64, rootless end-to-end confinement, and product UI interoperability were outside this run. The filesystem probes cover active-kernel behavior. Power-loss durability and SHA-256 ingestion and fetch remain separate qualification gates. The live allowlist is empty, and `github/docs` is recorded as a candidate for review. Production release requires the later phase gates.
