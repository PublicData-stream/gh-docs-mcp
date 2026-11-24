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

## Contributor documentation verification

On 2026-11-24, `git diff --check` passed, all 12 local links and their anchors
resolved, and the documented Rust commands matched the checked-in gate and
CI. The offline Rust gate passed formatting, Clippy, workspace tests, and doc
tests. The Phase 0 entry above records the separate Compose qualification and
its tested revision.

## Phase 1 qualification

The local Phase 1 gate passed for signed implementation commit
`48871f007344da456901731571f0cee103d8f941`. The Phase 0 regression began at
2026-11-24 14:52:05 UTC and the Phase 1 Compose suite began at 14:53:18 UTC.
The profile was Linux `amd64`, kernel `7.0.0-44-generic`, ext4-backed `/tmp`,
rootful Docker Engine `29.10.0`, and Compose `v5.8.0`. Rust was `1.100.0`,
fixture Git `2.53.0`, image Git `2.54.1`, Python `3.14.4`, Node `26.13.1`,
and pnpm `12.21.0`.

| Gate | Observed | Result |
| --- | --- | --- |
| Offline Rust | Locked format, Clippy with warnings denied, workspace tests, and doc tests passed. The focused cases covered native identity, raw paths and ranges, malformed batch framing, payload delimiters, process deadline and reap, and the manifest golden vector. | Pass |
| A01–A04 | Rebuilt role images passed the Phase 0 raw HTTP and TypeScript client probes, TLS and role separation, effective network and mount isolation, and confinement. Landlock reported ABI 8. | Pass |
| A05 | Fresh catalog migration, upgrade from `0001`, explicit rollback, repeated migration, initial fixture preservation, native UUID/OID checks, repository-scoped FKs, and role denials passed. A shared fence from the serve mount blocked an exclusive sync fence. Reinitialization preserved the lock inode. | Pass |
| A06 | The confined reader matched trusted Git for both hash formats, including loose and packed objects, raw path and blob bytes, modes, peeled tags, and merge reachability. It rejected oversized blobs, disallowed store entries, and a forced sandbox-setup failure. | Pass |
| Candidate visibility | Two generations per repository reached `verified`, and the old ref snapshot survived the synthetic force-push. The active pointers and outbox remained empty. The MCP discovery, list, and call probes exercised the Phase 0 fixture after candidate verification. | Pass |
| Minikube reader smoke | Minikube `v1.39.0` with rootless Docker `29.10.0`, Kubernetes `v1.37.0`, and containerd `2.3.4` ran the hardened Job. Both hash formats passed its object, tree, reachability, and forced sandbox-failure probes. The preliminary rootful Minikube attempt failed during bootstrap because the Docker default bridge `docker0` was missing. | Pass on rootless Docker |

### Rebuilt images and fixture identities

| Local image | Image ID |
| --- | --- |
| serve | `sha256:8531f8a1dab6e64df1395795b5f2716484ae30d7027e451a3ea310fd97244f32` |
| sync | `sha256:1cc7cc6cd83877db303984475d0df678ef2f0991721dbe454a9619d429712dba` |
| feed | `sha256:155435ec0dc08052a46a83a50c0a4de15b07a8c0830db8578d242696aa03c2ad` |
| Phase 1 fixture serve | `sha256:27a3c35d1e1c38bdf2a17184db393a202456781d3ca473ac1965a67372cbd2e1` |
| Phase 1 fixture sync | `sha256:852d65223736721c63a5f148e0091c88f706f45803b9ad6163ee518de8b52e08` |

The synthetic merge commits were `0575d9c2a1ba73c78fa364e4449c26b6ef8d4e50`
for SHA-1 and `b425c9828fcae5284c1b93419244e48d6f3ed2bf1d98188cb7df9305fb1c2fb3`
for SHA-256. The production role images and their fixture targets used the
Phase 0 toolchain and base image pins. The Minikube Job used the same fixture
serve image as Compose.

### Commands and cleanup

The [storage qualification procedure](storage.md#reproduce-the-qualification)
lists the five Buildx commands. The run used image tag `phase1-48871f007344`,
the previously prepared pinned Phase 0 client, and Minikube bootstrap inputs
copied into the disposable profile.

```sh
export CARGO_TARGET_DIR=/tmp/rust/target
cargo fetch --locked
scripts/phase0/check-rust.sh
sudo -n python3 -B scripts/phase0/integration.py --skip-build --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase1/integration.py --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
python3 -B scripts/phase1/minikube.py --serve-image "gh-docs-phase1-serve:$tag" \
  --docker-context rootless --bootstrap-cache "$bootstrap_cache"
```

The four local transcripts are
`/tmp/gh-docs-phase1-48871f007344-{rust,phase0,compose,minikube}.log`.
The disposable Compose projects, their firewall tables, Minikube profiles,
fixture directories, and test image tags were removed. SHA-256 fetch and
live acquisition remain later-phase gates. Hosted CI, ARM64, and production
deployment were outside this run.
