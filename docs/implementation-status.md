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

## Phase 2 qualification

Local tests used signed implementation commit
`53f500ce2f5fbda3b4e8a042b7be25e0c1946dfc` on 2026-11-24 UTC. The host
profile was Linux amd64, kernel `7.0.0-44-generic`, ext4-backed `/tmp`,
Docker Engine `29.10.0`, and Compose `5.8.0`. The tools were Rust
`1.100.0`, host Git `2.53.0`, image Git `2.54.1`, Python `3.14.4`,
Node `26.13.1`, and pnpm `12.21.0`.

| Gate | Observed | Result |
| --- | --- | --- |
| Offline Rust | Locked format, Clippy with warnings denied, workspace tests, and doc tests passed. The Phase 1 manifest golden vector still passed with the shared encoder. | Pass |
| A01–A04 | Rebuilt role images passed the pinned MCP wire and SDK probes, role and TLS checks, effective IPv4 and IPv6 network controls, mount isolation, and reader/search confinement. The host reported Landlock ABI 8. | Pass |
| A05–A06 | Fresh catalog migration, Phase 1 upgrade with fixture preservation, repeat migration, and scoped grants passed. The confined reader matched the object and raw byte oracle for both formats. | Pass |
| A07 | Synthetic plain HTTP and verified HTTPS produced full-closure SHA-1 and SHA-256 candidates. The probes checked ref policy, force pushes, tag changes, deletions, byte identity, reuse, no-ops, confinement, wrong CA, private destination denial, redirects, revocation, and concurrent admission. | Pass |
| A08, Phase 2 | Malformed and interrupted transfers, upstream restart, download and pack size, deadline, and private quota failures preserved usable candidates. Corrupt-baseline fallback reproduced the logical digest. Abrupt exit before and after the catalog commit exercised quarantine and retirement recovery. The active pointers and outbox stayed empty. | Pass |
| Minikube | Both the Phase 1 reader regression and Phase 2 HTTPS private candidate smoke passed on Minikube `1.39.0`, rootless Docker `29.10.0`, Kubernetes `1.37.0`, and containerd `2.3.4`. The Phase 2 Job verified both Git formats and denied worker network, credential, and unrelated filesystem access. | Pass |

The SHA-1 fixture received 395,441 bytes on the initial acquisition and
1,891 bytes on the subsequent update with a reverified baseline. The no-op
received 1,429 bytes and preserved the logical digest and generation. The
initial SHA-256 fixture received 395,907 bytes. These are synthetic transfer
measurements. Each initial candidate contained seven native objects.

### Images and commands

| Local image | Image ID |
| --- | --- |
| serve | `sha256:15ced0423cececd0c85c758acf4958b6f735b4d17fb192afc31ba8d291697a41` |
| sync | `sha256:751cdf8588b82ecf142c1265e25ae84620b9026c0400e6a7ab81b36197408c18` |
| feed | `sha256:9aedeb2da70facee1f078e867cce8b64347612600c3e8fa063ddd7a29fb20176` |
| Phase 1 fixture serve | `sha256:5b83053d5c75bf6de916a76f5988cf8b2d9120b85325ab7a50f202991c1ecf30` |
| Phase 1 fixture sync | `sha256:20e7df45c6566d4bd79207fd238138ddc4e2fac2a07db0e69244715d1474fd1c` |
| Phase 2 fixture sync | `sha256:70bb7f7fc2fc8bcaf3153e7068526122502cb0c0deecfaa20357090a7cc64d63` |
| Phase 2 fixture upstream | `sha256:edb35c73e6c47dfd59a387799eadfe43d03442fc97549567b164592699367d24` |
| PostgreSQL 19.0 | `sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f` |

The Compose runs began at 16:37:42, 16:39:35, and 16:37:46 UTC for Phases 0,
1, and 2, respectively. Buildx loaded each amd64 image with the
`phase2-qualification` tag. A separate `nft --check` validated syntax for
the generated IPv4 and IPv6 HTTPS allowances and scoped DNS allowance.

~~~sh
export CARGO_TARGET_DIR=/tmp/rust/target
cargo fetch --locked
scripts/phase0/check-rust.sh
client_dir=$(scripts/phase0/prepare-client.sh)
tag=phase2-qualification
sudo -n python3 -B scripts/phase0/integration.py --skip-build --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase1/integration.py --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase2/integration.py --image-tag "$tag"
python3 -B scripts/phase1/minikube.py --serve-image "gh-docs-phase1-serve:$tag" \
  --docker-context rootless --bootstrap-cache "$bootstrap_cache"
python3 -B scripts/phase2/minikube.py --sync-image "gh-docs-phase2-sync:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-phase2-postgres:$tag" --bootstrap-cache "$bootstrap_cache"
~~~

The local transcripts are
`/tmp/gh-docs-phase2-{rust,phase0,phase1,compose,minikube,phase1-minikube}-final.log`.
The disposable Compose projects, firewall tables, Minikube profiles, test
image tags, and generated fixtures were removed. Live acquisition, ARM64,
hosted CI, and physical power-loss durability were outside this qualification.

## Phase 3 qualification

The local Phase 3 gate passed on signed implementation revision
`77fb9a4815a7c25309a025b3aac0f50e7c0f161a` on 2026-11-24 UTC. Compose ran
on Linux amd64, kernel `7.0.0-44-generic`, rootful Docker Engine `29.10.0`,
Compose `5.8.0`, and ext4-backed `/tmp`. The tools were Rust `1.100.0`,
host Git `2.53.0`, image Git `2.54.1`, Python `3.14.4`, Node `26.13.1`,
and pnpm `12.21.0`.

| Gate | Observed | Result |
| --- | --- | --- |
| Offline Rust | Locked format, Clippy with warnings denied, workspace tests, and doc tests passed. | Pass |
| A01–A04 | Rebuilt images passed the pinned MCP wire and TypeScript client probes, scoped credentials and TLS, network and mount isolation, filesystem fences and fsync, and Git and search confinement. Landlock reported ABI 8. | Pass |
| A05–A06 | Fresh and repeated catalog migration, upgrade from Phase 0 and Phase 2, verified candidate and ref snapshot preservation, and role denials passed. The native reader matched both SHA-1 and SHA-256 fixture oracles. | Pass |
| A07 and Phase 2 A08 | Bounded private HTTPS acquisition, malformed and interrupted transfers, scoped refs, revocation of an in-flight acquisition, and same-repository and global admission limits passed. The catalog kept private candidates unreadable. | Pass |
| Phase 3 A08 | A malformed private pack failed preparation without changing the active pointer. A missing canonical pack blocked reads for its repository until exact bytes were restored and verified. Unknown paths and pack pairs were preserved for inspection. | Pass |
| A09 | Process interruption at installation, ref and manifest flush, ready state, publication commit, retirement, and repack boundaries recovered idempotently. SQL backend termination before and after outbox insertion preserved the prior pointer. A committed event remained discoverable after the response was lost. Fixture-only acknowledgment was durable and idempotent. | Pass |
| A10 | Concurrent active and retained generation reads, a second writer, a stale sequence, and rollback exercised the scope and fence checks. Publication and GC deferred while a reader held the repository fence. After PostgreSQL terminated the reader session, the filesystem fence remained held until the live Git child was reaped. | Pass |
| A11 | Force-pushed and deleted refs remained available through the earlier retained generation. Retirement denied the stale generation while bytes remained. The three-generation and 24-hour protections held, and guarded GC reclaimed aged discarded objects while preserving shared and young objects. | Pass |
| Minikube | A disposable rootless Docker profile used Minikube `1.39.0`, Kubernetes `1.37.0`, and containerd `2.3.4`. Both Git formats were published, verified by recovery, and read through the read-only canonical mount with serve credentials. | Pass |

The SHA-1 and SHA-256 initial upstream commits were
`eaeaffb1d30df005bad6f88f9051a2a96600785e` and
`9e2f6cbaa30976edb601b8b994b3769e870890c6cf38d164440ada674f27ef52`.
The Compose runs started at 17:35:53 UTC for Phase 0, 17:36:42 for Phase 1,
17:31:42 for Phase 2, and 17:30:55 for Phase 3.

### Rebuilt images and commands

| Local image | Image ID |
| --- | --- |
| serve | `sha256:bb22124f2de976b9ecc2468019b8fd4e1c74c50adf6f44683f5ef7c750412d35` |
| sync | `sha256:fca5c167c397bef2f927f533f5f15dea7dd7dcd64015c7068dad1b6fcba953bc` |
| feed | `sha256:0e566770decaffba365282b3d0733b4e9f96c23c5cb94dc2c4eed3da2c017c61` |
| Phase 1 fixture serve | `sha256:b02c04c19376e2987c3fa849e677529defdc25051ad08362d6e485182f93bffc` |
| Phase 1 fixture sync | `sha256:d170d97bffbb6b31937313062cbd5c1f3da0b99da52780a23c6a37b7db1ab688` |
| Phase 2 fixture sync | `sha256:c6185ecc5261570bc49dcdc1819ccab80b071949030f57721aabda2bd824cbd4` |
| Phase 2 fixture upstream | `sha256:c7bcb27b24852eb155ab14f9a5860ccda3d2a82b1c26af90fb9aa4b9a40cbeb9` |
| Phase 3 fixture serve | `sha256:944a4e4267145a4da69212643ebf25c812ae4da9f1f39a8030442f7b7ee68875` |
| Phase 3 fixture sync | `sha256:98fd1e9e1f34f8723bba2c9713823553c3b59ca1a76ed34892e7bb7cf26e74be` |
| PostgreSQL 19.0 | `sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f` |

The [qualification procedure](publication.md#qualification) documents the
Buildx targets. The final images were loaded locally with the tag below.

~~~sh
export CARGO_TARGET_DIR=/tmp/rust/target
cargo fetch --locked
scripts/phase0/check-rust.sh
client_dir=$(scripts/phase0/prepare-client.sh)
tag=phase3-verified-77fb9a4

sudo -n python3 -B scripts/phase0/integration.py --skip-build --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase1/integration.py --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase2/integration.py --image-tag "$tag"
sudo -n python3 -B scripts/phase3/integration.py --image-tag "$tag"
python3 -B scripts/phase3/minikube.py \
  --serve-image "gh-docs-phase3-serve:$tag" \
  --sync-image "gh-docs-phase3-sync:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-phase3-postgres:$tag" \
  --bootstrap-cache "$bootstrap_cache"
~~~

The local transcripts are `/tmp/ghdocs3-rust.log`,
`/tmp/ghdocs3-build-verified.log`, and
`/tmp/ghdocs3-verified-{phase0,phase1,phase2,phase3,minikube,cleanup}.log`.
Preliminary probes reproduced two issues. Pinned Git emitted a reverse-index
sidecar during repack, and the Phase 2 slot-saturation probe reached the
repository advisory-key lookup before slot admission. The final run used
explicit repack configuration and restored slot admission ordering. The
disposable Compose projects and firewall tables, Minikube profiles, test
image tags, generated fixtures, and SDK install directory were removed.

These results qualify synthetic process-interruption recovery on the
declared local profiles. Physical power-loss durability, live acquisition,
ARM64, hosted CI, and production deployment remain outside this run.

## Phase 4 qualification

The local Phase 4 gate passed on signed implementation revision
`2eefc311e4d95be5c97f82510bdf94094155b0a2` on 2026-11-25 UTC. The Compose
host was Linux amd64, kernel `7.0.0-44-generic`, with ext4-backed `/tmp`,
rootful Docker `29.10.1`, and Compose `5.8.0`. The tools were Rust
`1.100.0`, host Git `2.53.0`, image Git `2.54.1`, Python `3.14.4`,
Node `26.13.1`, and pnpm `12.21.0`.

| Gate | Observed | Result |
| --- | --- | --- |
| Offline Rust | Format, frozen Clippy with warnings denied, all-features workspace tests, and doc tests passed. Cursor tests checked HMAC tampering, argument and tool binding, and expiry without renewal. | Pass |
| A06 | The private JSON interface matched independent Git oracles for both object formats, including raw paths, modes, native OIDs, CRLF, NUL, invalid UTF-8, symlinks, gitlinks, empty objects, exact slices, and UTF-8 page boundaries. | Pass |
| A12 | The fixture denied hidden, unreachable, staging, ready, and cross-repository objects and returned the same outcome for denied and missing objects. It resolved nested approved tags and rejected invalid revision and path expressions. | Pass |
| A13 | The in-image request schema matched the snapshot. Returned contexts composed directly with other operations. Raw-path pagination was deterministic. Signed ref and tree pages stayed pinned through force push and rollback and failed after retirement. | Pass |
| A14 | The selective grant exposed only approved paths and their reviewed notices. Navigation ancestors omitted tree OIDs. Policy supersession and revocation denied subsequent reads. Acquisition-only approvals stayed outside the exploration projection. | Pass |
| Phase 4 limits and migration | The 50,000-entry fixture returned explicit incomplete coverage. Oversized blob reads and concurrent overload were bounded. Forced worker setup failure returned `sandbox_unavailable`. Migration from catalog version 4 with published records, repeat migration, and SELECT-only grants passed. | Pass |
| A01–A04 regression | The rebuilt Phase 0 stack passed the pinned MCP wire and TypeScript client `2.4.0` probes, database TLS and role grants, effective mounts and networks, and Git and search confinement. Landlock ABI 8 was observed. | Pass |
| A05–A06 regression | Phase 1 preserved prior catalog and fixture records across upgrade, kept reader lock inodes stable, and matched the native Git oracle in both object formats. | Pass |
| A07–A08 regression | Phase 2 passed bounded HTTPS acquisition, ref policy, interrupted and malformed transfers, recovery, resource limits, and concurrent admission. Private candidates stayed outside the published catalog. | Pass |
| A08–A11 regression | Phase 3 exercised pack rejection, interrupted publication and outbox writes, rollback, retention and GC. Database loss with a live Git child held the filesystem fence through reap. | Pass |
| Minikube | The disposable Minikube `1.39.0` profile used rootless Docker `29.10.0`, Kubernetes `1.37.0`, and containerd `2.3.4`. Both formats were published, recovered, and read byte-for-byte through the same Phase 4 image with serve credentials and a read-only store. | Pass |

The initial Phase 4 fixture commits were
`c0a821121825bf4d7262cf5dbb14182a08f716be` for SHA-1 and
`c1d671b3b2abf999220bc6fe3cf7b9addc974dc477fdadd594c35652722e4370` for SHA-256.
The preliminary Phase 4 Compose gate started at 02:23:14 UTC. On
`cdb32dd`, the Phase 3 regression stopped because its CLI fixture printed
`GenerationUnavailable` in place of the established `generation unavailable`
diagnostic. The corrected revision preserves that diagnostic and authorizes
paths before applying find patterns. The corrected Phase 3 and Phase 4
Compose runs started at 02:37:25 and 02:42:34 UTC, respectively. The Phase
0, 1, and 2 runs started at 02:45:43, 02:46:49, and 02:47:20 UTC.

### Rebuilt images and reproduction

The images were built with `docker --context default buildx build --network host
--platform linux/amd64 --load` and the targets in the existing
[qualification procedure](exploration.md#qualification).

| Local image | Image ID |
| --- | --- |
| gh-docs-phase4-serve | `sha256:5e5c1061fce95d549eda15500ca64da85321c9501c1499baeb82fa10ad0dee6a` |
| gh-docs-phase3-sync | `sha256:a745e408d248d6178ea1c307a3f29e2b4d12898b8cd1f69dbb053cd9d6767ea8` |
| gh-docs-phase2-upstream | `sha256:aa958c5b917fb306ca4a803a80ebc76c72be576b180fdf814fbd37d2a2827b06` |
| gh-docs-mcp-serve | `sha256:ba50019390191cb8e9061b1b28a0905bb710504c2b32f36a58d5f19e663839df` |
| gh-docs-mcp-sync | `sha256:d5777171e5b7934ef800948ba72cb53749c579770265c5c2aac87c6a6605f026` |
| gh-docs-mcp-feed | `sha256:d4e095cb2787edde7e5104ec25b636182d763fd76b88c6f6898f58bca7128ea1` |
| gh-docs-phase1-serve | `sha256:73a29002bf50a8ff8d125d011d6b24642365b6dde8dc55c6ed702bbc27cabdab` |
| gh-docs-phase1-sync | `sha256:a167874337d1c97f1a0915c4332b84358b87bae2e96aacc41beedbf3db0def28` |
| gh-docs-phase2-sync | `sha256:f355420d8918fe25600f2ee8aeb986b55aea30ba8b184391fd03879a59cee421` |
| gh-docs-phase3-serve | `sha256:0f4c12ac740d462e3e57ae56b698eb761132718f290c5cfaf8359f91e95424c6` |
| Pinned PostgreSQL | `sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f` |
| Pinned proxy and sink | `sha256:0272e4604ed93c1792f03695a033a6e8546840f86e0de20a884bb17d2c924883` |

~~~sh
export CARGO_TARGET_DIR=/tmp/rust/target
scripts/phase0/check-rust.sh
client_dir=$(scripts/phase0/prepare-client.sh)
tag=phase4-2eefc311e4d9

sudo -n python3 -B scripts/phase3/integration.py --image-tag "$tag"
sudo -n python3 -B scripts/phase4/integration.py --image-tag "$tag"
sudo -n python3 -B scripts/phase0/integration.py --skip-build --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase1/integration.py --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase2/integration.py --image-tag "$tag"
python3 -B scripts/phase4/minikube.py \
  --serve-image "gh-docs-phase4-serve:$tag" \
  --sync-image "gh-docs-phase3-sync:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-phase4-postgres:$tag" \
  --bootstrap-cache "$bootstrap_cache"
~~~

The final transcripts are in
`/tmp/gh-docs-phase4-final.LMPsRKXt`, including `rust.log`, `phase0.log`
through `phase4.log`, `minikube.log`, `cleanup.log`, and the image build logs. The
preliminary Phase 3 failure is in
`/tmp/gh-docs-phase4.khQi6fRz/phase3.log`.

Cleanup removed the disposable Compose projects, scoped firewall tables,
image tags, Minikube profiles, and generated SDK client fixtures. The
qualification used synthetic sources on local amd64 profiles.
