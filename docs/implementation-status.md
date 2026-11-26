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

## Phase 5 qualification

The Phase 5 Compose gate passed on 2026-11-25 UTC with signed runtime
revision `03618b6bfc901d376d7ccedf4f7e90cda934b65c` and the Compose
probe at `e0c4b14d6d55377a1441e75f4bd782f76c26f0a3`. The host was Linux
amd64, kernel `7.0.0-44-generic`, with ext4-backed `/tmp`, rootful Docker
`29.10.1`, and Compose `5.8.0`. The probes used Rust `1.100.0`, host
Git `2.53.0`, container Git `2.54.1`, ripgrep `16.0.0`, Python
`3.14.4`, Node `26.13.1`, and pnpm `12.21.0`.

| Gate | Observed | Result |
| --- | --- | --- |
| Offline Rust | Formatting, frozen all-feature and all-target Clippy with warnings denied, unit tests, and doc tests passed. The generated registry matched the committed snapshot. | Pass |
| A01, A13 | Raw MCP discovery, listing, and calls through the loopback TLS proxy matched the exported schemas. Resolved contexts passed directly into read, search, and history. Search cursors rejected tampering, changed arguments, and a valid expired token. A retained cursor kept its original generation and failed after retirement. The pinned TypeScript SDK `2.4.0` called all twelve tools through the same proxy. | Pass |
| A14 | Migration from catalog version 5 preserved a published version 1 approval and SELECT-only access. Version 1 retained existing reads and denied both new output forms. Publication under a new approval enabled search and history. Supersession and revocation denied stale contexts and cursors. Selective path grants withheld hidden paths and bytes. | Pass |
| A15 | Both Git formats matched the pinned ripgrep oracle for CJK, CRLF, exact byte ranges, and repeated blob paths. A reversible raw path, case matching, binary skips, invalid regexes, and explicit oversized-blob omissions were checked. | Pass |
| A16 | Both Git formats matched Git oracles for graph membership, root and merge handling, patch bytes, and same-path first-parent blame. Binary metadata, rename boundaries, literal metacharacter paths, policy denial, and same-generation enforcement passed. | Pass |
| A17 | An empty incomplete search page resumed to a later hit. The wide control-byte blob reassembled across multiple responses within the 256 KiB wire cap. The framed worker denied inherited sockets, filesystem and executable access, and forced sandbox setup failed closed. Two held workers made a third request fail admission. Proxy disconnect and deadline each reaped a held worker, and a subsequent request obtained capacity. | Pass |
| A01–A04 regression | The rebuilt Phase 0 stack passed raw MCP and SDK, role grants and TLS, effective filesystem and egress isolation, and the original worker probes. Landlock ABI 8 was observed. | Pass |
| A05–A06 regression | Phase 1 passed catalog upgrades, scoped grants, stable reader locks, SHA-1 and SHA-256 native object oracles, and pinned fixture MCP calls. | Pass |
| A07–A08 regression | Phase 2 passed bounded synthetic HTTPS acquisition, ref policy, malformed and interrupted transfers, candidate recovery, concurrency and quota checks. | Pass |
| A08–A11 regression | Phase 3 passed malformed pack rejection, interrupted publication and outbox writes, rollback, retention, and maintenance. Database loss with a live reader held the filesystem fence through reap. | Pass |
| A06, A12–A14 regression | Phase 4 passed both raw-byte oracles, scoped disclosure and SELECT-only grants, generation denial, signed pages, rollback, retirement, supersession and revocation. | Pass |
| Minikube | Minikube `1.39.0`, rootless Docker `29.10.0`, Kubernetes `1.37.0`, and containerd `2.3.4` published and recovered both formats. The normal Compose serve image passed read, search, and history over pod loopback with serve credentials and the read-only store. | Pass |

The Compose run started at 04:45:33 UTC. The initial fixture commits were
`abb563788e9fb63dc4a86ef8eea26b5b98501785` for SHA-1 and
`6d8be56a114aa5e29b4708e3435e894847616712be33d3bc7d3df1b4a19213f4`
for SHA-256. Preliminary runs caught probe defects in HTTPS connection
teardown, plain proxy denial handling, and the assumption that an escaped
blob would fit in two pages. The corrected probe followed the full cursor
chain. The first Minikube attempt published both formats and stopped at
the older fixture's blob check. The corrected Minikube probe calls the
Phase 5 endpoint directly. The Phase 0–4 Compose regression runs started
at 04:44:16, 04:46:34, 04:49:02, 04:54:05, and 05:01:18 UTC,
respectively.

### Rebuilt images and reproduction

| Image target | Local image ID |
| --- | --- |
| Serve | `sha256:f6784a77c51fb468becbd8f11fff63bce56b68db1937766cc17997733f96e744` |
| Sync | `sha256:01a6a0c1432ceb69cd1db05135b62a7f8efe6266697a9c2b05ef254fe93004b0` |
| Feed | `sha256:742a91263ee522fa85510cc6460bfd70a754bcad7e18ebca5489c34991bf8433` |
| Phase 1 serve | `sha256:eab0616a57e7b6fa6edab13a54d52a1ae29fdb756fd10680bb36dfbab7eb0e08` |
| Phase 1 sync | `sha256:d58c4939b3962a215f6c8f79666a735b33e9e9c91fe8962ba92f649396a120a9` |
| Phase 2 sync | `sha256:1448d7a846f98fd1fe2500b83b6467eaf4bddd48cce21ad05e98af1c1b7d9be5` |
| Phase 2 upstream | `sha256:b2e1a717d29576ae74468e75e5d1c0f8517d191f3d8639b7c99e2ca5d0d9bc29` |
| Phase 3 serve | `sha256:f4cd10713b3d6a0fe28593d2755045e8f7ab481de274fb53374220138a0a74b3` |
| Phase 3 sync | `sha256:4367939bdc86e11eb5680c96deae31e64d97ec6a3ed5976ae7d9e65725bfb89c` |
| Phase 4 serve | `sha256:3bd7b7cb7d4ac0133bf09f37af8c55f21c4206f499cb9263668df111926e1b94` |
| Pinned PostgreSQL | `sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f` |
| Pinned proxy and sink | `sha256:0272e4604ed93c1792f03695a033a6e8546840f86e0de20a884bb17d2c924883` |

Buildx Bake built the pinned targets for `linux/amd64` with
`--allow=network.host` and Docker output. The reproduction commands below
use the locally built tag and the prepared client and Minikube caches.

~~~sh
export CARGO_TARGET_DIR=/tmp/rust/target
scripts/phase0/check-rust.sh
client_dir=$(scripts/phase0/prepare-client.sh)
tag=phase5-qualified
node=$(command -v node)

sudo -n python3 -B scripts/phase5/integration.py --image-tag "$tag" \
  --node "$node" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase0/integration.py --skip-build --image-tag "$tag" \
  --node "$node" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase1/integration.py --image-tag "$tag" \
  --node "$node" --client-dir "$client_dir"
for phase in 2 3 4; do
  sudo -n python3 -B "scripts/phase$phase/integration.py" --image-tag "$tag"
done
python3 -B scripts/phase5/minikube.py \
  --serve-image "gh-docs-mcp-serve:$tag" \
  --sync-image "gh-docs-phase3-sync:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-phase5-postgres:$tag" \
  --bootstrap-cache "$bootstrap_cache"
~~~

Local transcripts and the Bake inputs are in
`/tmp/gh-docs-phase5-qualified.4YSNiV7S`. The passing transcripts include
`phase5-final.log`, `phase0.log` through `phase4.log`, and
`minikube-serve.log`. Cleanup removed the disposable Compose projects and
firewall scopes, local test tags, generated SDK client, and Minikube
profiles. The evidence covers synthetic sources on local amd64 and the
pinned protocol clients.

## Phase 6 qualification

The local Phase 6 gate passed. The images were rebuilt from signed runtime
revision `4f0b5416eaba8b9aa025a984757bf0fdee276443`. The probes were based on
signed revision `b3b70d8e5d98f2c948d05d9a2d061724ee16975f` on
2026-11-25 UTC. The host was Linux amd64, kernel `7.0.0-44-generic`,
with ext4-backed `/tmp`, rootful Docker `29.10.1`, and Compose `5.8.0`.
The runs used Rust `1.100.0`, Git `2.53.0` on the host, Git `2.54.1`
and ripgrep `16.0.0` in the images, Python `3.14.4`, Node `26.13.1`,
pnpm `12.21.0`, and MCP TypeScript client `2.4.0`.

| Gate | Scope and observed result | Result |
| --- | --- | --- |
| Offline Rust | Formatting, frozen all-feature and all-target Clippy with warnings denied, workspace tests, and doc tests passed. | Pass |
| A18 | Request and response vectors matched the independently signed messages. Raw HTTP probes exercised framing, case-insensitive duplicate headers, MAC and audience rejection, clock skew, replay, receiver restart, and route isolation. Unit tests saturated the replay cache and checked epoch and expiry edges. The Compose receiver accepted both keys during rotation and denied the expired key. | Pass |
| A19 | The v6 upgrade preserved the existing pointer and outbox tuple. Compose exercised lost hint acknowledgment, restart after fetch, restart after pointer commit, and lost delivery acknowledgment. It also exercised reordered events, rollback with notifications unavailable, pause and resume with unchanged upstream, periodic reconciliation with feed idle, duplicate ownership, bounded reruns, session loss, transient retries, and actual feed-initiated publication. MCP resolved and read the resulting commits using the read-only role. | Pass |
| A01–A06 regression | Fresh and repeated migrations, v7 catalog upgrades, exact SHA-1/SHA-256 object reads, scoped DB grants and TLS, real Compose isolation and Landlock, and both pinned MCP probes. | Pass |
| A07–A17 regression | Synthetic HTTPS acquisition, interrupted publication and recovery, retention and rollback, visibility and revocation, raw bytes, cursor paging, search and history, and worker limits and cancellation. | Pass |
| Minikube | One disposable rootless Docker `29.10.0` profile with Minikube `1.39.0`, Kubernetes `1.37.0`, and containerd `2.3.4`. Periodic reconciliation was deferred so the SHA-1 publication required the real feed hint. The sync daemon observed authenticated delivery, and the normal serve image read the published generation over MCP pod loopback. | Pass |

The synthetic history fixtures started at
`abb563788e9fb63dc4a86ef8eea26b5b98501785` for SHA-1 and
`6d8be56a114aa5e29b4708e3435e894847616712be33d3bc7d3df1b4a19213f4`
for SHA-256. Preliminary Compose runs found stale workspace artifacts in
the Docker cache and a rotation probe that attached to the receiver.
A concurrent Phase 1 attempt hit the PostgreSQL startup health timeout.
The first signed-archive Phase 6 Compose attempt timed out waiting for
initial scheduled publication. The retry added an explicit scheduler
health check, a 140-second startup deadline and a queue-state dump on
failure. The corrected probes passed on the archive-built images.
The preliminary Minikube daemon ran as PID 1 and the Git worker
rejected that parent. The corrected smoke uses a supervising shell and
preserves the existing worker guard. The interruption probes exercised
process and database session loss. Production, live-source, cross-host
and physical power-loss qualification remain separate.

### Reproduction and artifacts

The build used Buildx Bake with host networking for the pinned input
downloads, local Docker output, and an unpacked archive of the tested
revision. The table lists the local image IDs for Phase 6.

| Image target | Local image ID |
| --- | --- |
| Serve | `sha256:fd6a0a1e27afbd7a7f2c0e934cdec5696416347a23112c8109af089fc5a10894` |
| Sync | `sha256:22e6887cb70820f2cb91cfc51e482390dcb2bcc88b44fe71fb767c97f535262c` |
| Feed | `sha256:40985a41b03bcc34057532469ef3525266d7085f9c28f6a086dc8d533decf8b8` |
| Phase 6 serve | `sha256:351afe5182d8a0bd76523a9be94b5cded86cf6e0b7538ed2ce0faa6f893dc076` |
| Phase 6 sync | `sha256:62ff5473b29b729e1df77ba0f2cce2851f3a5f1ddfdc6caafc052daf6e28a9be` |
| Phase 6 feed | `sha256:663f0a4e91f7f6eea483faae131c74889fde97fe3da1fe16652eec0cf1e162c9` |
| Synthetic upstream | `sha256:59e5b0590dd9877a1a09bdd6178e638611f0089c785fac47665202c7b95f87bf` |
| Pinned PostgreSQL | `sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f` |
| Pinned proxy and sink | `sha256:0272e4604ed93c1792f03695a033a6e8546840f86e0de20a884bb17d2c924883` |

Run the offline gate and Phase 6 probes as documented in the
[control runbook](control.md#qualification). The earlier Compose probes
used the same image tag and the pinned SDK fixture.

~~~sh
export CARGO_TARGET_DIR=/tmp/rust/target
scripts/phase0/check-rust.sh
client_dir=$(scripts/phase0/prepare-client.sh)
node=$(command -v node)
sudo -n python3 -B scripts/phase0/integration.py --skip-build --image-tag "$tag" \
  --node "$node" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase1/integration.py --image-tag "$tag" \
  --node "$node" --client-dir "$client_dir"
for phase in 2 3 4; do
  sudo -n python3 -B "scripts/phase$phase/integration.py" --image-tag "$tag"
done
sudo -n python3 -B scripts/phase5/integration.py --image-tag "$tag" \
  --node "$node" --client-dir "$client_dir"
~~~

The receipts for tag `p6-wn1jljb2-qual` are in
`/tmp/gh-docs-phase6.WN1jLjB2`. They include the Bake input, the exact
image IDs, `rust-qualified.log`, the six earlier Compose transcripts,
the successful `phase6-verified.log` retry, and
`minikube-qualified.log`.
The retry started at 05:59:27 UTC. Cleanup removed the disposable
Compose resources and firewall scopes, the Minikube profiles, the
generated SDK fixture and bootstrap files, and 36 local test image tags.
The transcripts and image IDs remain at the receipt path.

## Phase 7 qualification

The local qualification targets Linux amd64, kernel `7.0.0-44-generic`,
ext4-backed `/tmp`, rootful Docker `29.10.1`, and Compose `5.8.0`.
An earlier runtime archive was signed revision `6bee22d` and
its 1,024-ref positive probe failed. The final archive is signed revision
`c809e42`. The probes are based on signed revision `6d9e4c0`.
The inputs use Rust `1.100.0`, host Git `2.53.0`, image Git `2.54.1`,
ripgrep `16.0.0` with PCRE2 disabled, Rust MCP SDK `3.11.0`,
TypeScript MCP client `2.4.0`, Node `26.13.1`, and pnpm `12.21.0`.
Runtime qualification uses synthetic data and local endpoints.

### Findings and initial verification

- The serve process originally handled SIGINT but lacked coordinated
  SIGTERM shutdown. On revision `70ce268`, stopping the normal serve
  image with a live stopped search worker exited successfully in
  376 ms, released the filesystem fence, preserved its inode, and
  accepted a fresh MCP read after restart. An earlier probe attempted
  the read before the restarted server was ready. The qualifier now
  waits for health before checking the MCP endpoint.
- Git's routine per-ref fetch summaries exceeded the 64 KiB diagnostic
  cap at 1,024 refs. A disposable native Git comparison produced
  69,586 stderr bytes with ordinary fetch and zero with quiet fetch.
  Both runs wrote the same ref count. Revision `6bee22d` suppresses the
  routine output and keeps the diagnostic and source admission limits.
- The first rebuilt 1,024-ref run still failed. Native protocol tracing
  showed that Git generated six discovery prefixes per exact refspec,
  producing about 341,000 request bytes at that count. The confined
  broker permits at most 256 KiB per request and closed the connection.
  Revision `c809e42` now fetches bounded groups of exact approved refs
  into the private candidate. It checks the complete before-and-after
  ref snapshots and object closure before the candidate becomes usable.
  The broker cap and source approval limits remain in place.
- Buildx could not refresh the registry during the first rebuild. The
  later build used a read-only sparse index and crate archive seed
  populated from the locked host cache. The Dockerfile still runs the
  workspace compile frozen with networking disabled. The optional
  seed was also checked with an empty named context.
- The Phase 2 saturation probe initially lost a sync slot before its
  global admission assertion. The revised probe puts both repositories
  on the delayed fetch path with fresh objects and checks the global
  bound before the timed repository fence check.

### Exact-image adversarial gate

The Phase 7 Compose run on `c809e42` passed. Its normal serve, sync and
feed image IDs, and all fixture-target IDs, are preserved in the
`qualified-image-ids.txt` receipt. The suite used normal role mounts,
scoped credentials, local synthetic HTTPS, the pinned proxy and database,
and effective Landlock ABI 8 and seccomp confinement.

| Boundary | Observed result |
| --- | --- |
| Reader and search workers | Unrelated repositories, secrets, sockets, writes, inherited descriptors, and failed sandbox setup were denied. SHA-1 and SHA-256 reads matched the Git fixture oracle. |
| MCP request lifetime | A disconnected search request reaped its worker in 68 ms. A stopped worker was drained on service shutdown, its filesystem fence was released without inode replacement, and a subsequent request succeeded. `docker stop --timeout 8` took 8.555 s wall time on this run, so shutdown latency is variable. |
| Candidate isolation | Truncated indexes in both object formats, malformed tree and transport responses, partial transfer, and quota denial left the active publication usable. A corrected retry verified. |
| Search and traversal limits | A 130 MiB binary search advanced through a 120 MiB first page and completed on resume. Deep traversal returned `budget_exceeded`; the next approved read succeeded. |
| Transfer limits | A 7,371,760-byte pack passed the fixture's 8 MiB cap; a larger one failed. Exactly 1,024 approved refs verified; 1,025 failed `ref_policy`, preserving the published pointer. |

The probe did not exercise physical power loss, actual filesystem
exhaustion, an input approaching the 2 GiB global inflated-object cap,
or the maximum object and ref-name widths together. These remain outside
the qualified envelope.

The offline Rust gate passed after the runtime changes. The final
`qualified-compose-results` receipt records exit zero for every Phase 0
through 7 Compose probe on tag `p7-axd2tzaw-qualified`. Phase 0 and 1
covered the pinned MCP client, role mounts, database grants, TLS, and
both native object formats. Phases 2 through 6 covered acquisition and
recovery, publication and retention, visibility and revocation, search
and history, and the feed control path.

A separate Minikube `v1.39.0` and Kubernetes `v1.37.0` smoke ran the
same images in a rootless Docker profile with a Restricted namespace.
It passed feed-to-MCP publication, the authenticated acknowledgment,
search and history, Landlock ABI 8 and seccomp denials, and forced worker
setup failure. The disposable cluster and its home under `/tmp` were
deleted. Two focused repeats of the serve shutdown probe took 0.375 s
and 0.395 s and passed the fence and subsequent-read checks. The longer
8.555 s full-suite stop remains a measured outlier.

### Dependency and source review

The local source review followed the deployed request, sync acquisition,
worker, SQL visibility, and HMAC paths. The scope and assumptions are
recorded in the [Phase 7 threat model](phase7-qualification.md#threat-model).
The sync Git child clears inherited environment, fixes its executable
path and protocol allowlist, disables replacement objects and lazy fetch,
and asks the broker only for observed approved OIDs. Private and canonical
bare stores require service-owned config and reject unexpected entries,
including alternates; descriptor-relative opens reject symlinks. The
runtime probes above and the earlier gates exercised the corresponding
network, path, visibility, and worker denials.
The Codex Security workbench rejected the managed scan artifact path
because its parent ancestry is group-writable without the sticky bit.
The managed scan stopped at the threat model checkpoint. The source
review proceeded locally.

RustSec reported no matching vulnerability in the workspace or pinned
ripgrep lockfile. It marked `chrono 0.4.45` unmaintained. The pin review
also used upstream Git, ripgrep, Rust, and MCP SDK sources. Recursive
cloning is outside the configured Git acquisition path, and the
runtime uses the Rust SDK's pinned stateless server transport with
legacy sessions disabled. [RustSec RUSTSEC-2026-0543](https://rustsec.org/advisories/RUSTSEC-2026-0543.html) ·
[Git advisories](https://github.com/git/git/security) ·
[ripgrep advisories](https://github.com/BurntSushi/ripgrep/security/advisories) ·
[Rust release notes](https://doc.rust-lang.org/releases.html) ·
[Rust SDK advisories](https://github.com/modelcontextprotocol/rust-sdk/security/advisories) ·
[TypeScript SDK advisories](https://github.com/modelcontextprotocol/typescript-sdk/security/advisories)

Trivy reported critical package matches in the pinned proxy and PostgreSQL
images. The proxy contains the affected Brotli decoder. The tested Nginx
binary has no Brotli linkage, and the fixture configuration loads no
dynamic modules. The PostgreSQL match is in the bundled Go runtime of
`gosu`. The service starts as UID 70, which skips the root-only `gosu`
branch, and the Go advisory concerns TLS session resumption. PostgreSQL
also links the flagged libxml2 version, while app queries use fixed SQL
and have no XML input path. The exact image reports also contain findings
at lower severities. Treat the pinned infrastructure image findings as
release disposition items. [Brotli 1.2.1 release](https://github.com/google/brotli/releases/tag/v1.2.1) ·
[Go GO-2026-4337](https://pkg.go.dev/vuln/GO-2026-4337)

The final exact-image audit used Trivy `0.76.2` with a vulnerability
database updated at `2026-11-25T07:11:44Z` and downloaded at
`08:14:13Z`. It saved separate CycloneDX SBOMs and vulnerability JSON
for all six runtime images. RustSec used database revision
`10db6998b5d74ec438bcf0763125bd425a068a8f` and returned zero
matched vulnerabilities for the workspace and the Dockerfile-pinned
ripgrep lockfile, with the `chrono` warning above.

| Image | Critical | High | Medium | Low | Unknown |
| --- | ---: | ---: | ---: | ---: | ---: |
| Serve | 0 | 0 | 7 | 4 | 0 |
| Sync | 0 | 0 | 7 | 4 | 0 |
| Feed | 0 | 0 | 7 | 4 | 0 |
| PostgreSQL | 1 | 26 | 31 | 3 | 1 |
| Proxy | 1 | 57 | 91 | 100 | 2 |
| Initializer | 0 | 0 | 7 | 4 | 0 |

The critical proxy match is `CVE-2026-111497` in `brotli-libs`
version `1.2.0-r0`, with `1.2.1-r0` listed as fixed. The critical
PostgreSQL image match is `CVE-2025-68121` in the `gosu` Go standard library
`v1.24.6`. Static linkage and entrypoint checks narrow the tested
reachability as described above; they do not resolve the image findings.
Operator disposition or updated pinned infrastructure images are required
before production release. The managed Codex Security scan also remains
uncompleted because the workbench rejected its artifact directory.

### Measured capacity

The final exact-image run completed four deterministic profiles and retained
8,719 JSONL records: 8,540 request samples, 100 cell summaries, 40
reader comparisons, 33 publication stages, four inventories, and two
sync-concurrency results. The table describes the visible initial
generation. Compressed bytes are the canonical pack; allocated bytes
include filesystem allocation in its canonical store.

| Profile | Commits / tip files | Blobs / trees | Pack bytes | Inflated bytes | Allocated bytes / pack |
| --- | ---: | ---: | ---: | ---: | ---: |
| SHA-1 small | 32 / 32 | 48 / 65 | 80,914 | 169,151 | 1.57× |
| SHA-1 medium | 512 / 256 | 640 / 1,025 | 754,612 | 1,756,335 | 1.14× |
| SHA-1 large | 4,096 / 2,048 | 5,120 / 8,193 | 6,071,278 | 14,100,527 | 1.09× |
| SHA-256 medium | 512 / 256 | 640 / 1,025 | 797,380 | 1,802,379 | 1.16× |

These are client-observed TLS request latencies in milliseconds, at one
concurrent request, page limit 10, after three warm-up calls. Each cell
has 100 comparable samples except SHA-256 history, which offered 54
within the 20-second cell window. Entries are p50 / p95 / p99. A dash
means the required 100 comparable samples were unavailable.

| Profile | `git.read` | `git.rg` | `git.log` |
| --- | ---: | ---: | ---: |
| SHA-1 small | 38.4 / 41.0 / 43.8 | 42.1 / 44.0 / 48.1 | 33.2 / 35.0 / 37.0 |
| SHA-1 medium | 38.2 / 40.1 / 43.0 | 41.8 / 43.4 / 47.4 | 33.2 / 35.0 / 38.5 |
| SHA-1 large | 37.3 / 40.5 / 46.5 | 40.4 / 42.1 / 44.5 | 33.1 / 35.2 / 45.4 |
| SHA-256 medium | 36.9 / 39.2 / 40.6 | 40.3 / 42.6 / 45.6 | 375.8 / 384.6 / — |

The four profiles produced 1,675 complete results, 5,537 partial results,
1,328 structured `budget_exceeded` rejections, and no failed calls. The
partial search and history pages carried continuation. At one concurrent
request, reads admitted 26.0–27.0 calls per second; search admitted
23.7–24.6. At eight calls offered at about 47 per second to one repository,
the main read and search cells admitted about 27–30 per second and rejected
36–43 of each 100 calls. SHA-256 history admitted 2.7 per second at one
concurrent call, with 54 partial pages and no p99; its eight-call cell
admitted 12 of 100 and rejected 88. Subsequent cells completed, confirming
admission recovered after overload.

Twenty `git.read` pack-eviction samples per profile verified zero resident
pack and index pages before each call. Their p95 ranged from 39.0 to
42.3 ms; this only controls those files' page residency. The large
profile's cold cell recorded 131,317,760 cgroup read bytes across 20
requests, compared with zero cgroup read bytes during its 100 warm reads.
Five eight-object comparisons per profile gave median direct-reader time
of 77.2–77.6 ms and batched-reader time of 67.9–68.9 ms. The cgroup used
0.75–0.85 CPU seconds during each 100-call warm `git.read` cell. Sampled
process RSS summed in the serve container peaked at 18,372–20,532 KiB
across cells; the sync container's sum peaked at 21,020–45,572 KiB.
The 25 ms process sampler can miss short-lived workers, and summed RSS
counts shared pages more than once. Raw CPU, I/O, memory, PID, and
per-call counters are in the JSONL
receipt.

| Profile | Initial fetch | Prepare | Publish | Incremental fetch | GC |
| --- | ---: | ---: | ---: | ---: | ---: |
| SHA-1 small | 6.43 s | 0.23 s | 0.12 s | 6.57 s | 0.22 s |
| SHA-1 medium | 6.43 s | 0.27 s | 0.11 s | 6.60 s | 0.93 s |
| SHA-1 large | 8.56 s | 3.37 s | 0.19 s | 10.85 s | 1.26 s |
| SHA-256 medium | 6.49 s | 0.28 s | 0.11 s | 6.99 s | 1.76 s |

The large profile had 17,409 native objects. An update above its
17,412-object approval failed while the published generation remained
readable. One sync slot took 14.01 s for two manual jobs; two slots took
14.96 s, with peaks of one and two running jobs respectively and no
durable scheduler queue. Both jobs shared the paced local upstream host,
so this run does not demonstrate a throughput gain from the second slot.
An earlier local large-fixture run on the initial image observed a
13.37 s GC stage. The 1.26 s final-image observation is not a worst case.

### Limit proposal and qualification boundary

Keep the current defaults. The measurements support a bounded local
envelope for these synthetic repositories and expose overload as a
structured response. They do not justify increasing a global cap.

| Control | Current bound and measured evidence | Proposal |
| --- | --- | --- |
| Approved refs | 1,024 ordinary-length refs verified and 1,025 denied. Very long names can exhaust the existing 48-request budget. | Keep the 1,024 count and the 256 KiB broker request cap; qualify unusual ref-name distributions per source. |
| Per-source transfer | The fixture's 8 MiB pack cap admitted 7,371,760 bytes and rejected a larger pack. The global approval maxima remain 256 MiB pack, 384 MiB response, 500,000 objects, and 900 s deadline. | Keep source-specific caps; require inventory and an approved larger-source run before raising any source's values. |
| Inflation and object size | The largest measured closure had 14,100,527 inflated bytes. The verifier still caps one object at 128 MiB and the closure at 2 GiB. | Keep both caps; their near-boundary behavior is unqualified here. |
| Admission and deadlines | Readers have four global and two per-repository slots and a 15 s deadline. Sync defaults to two slots, with four as its configured maximum; the source fixture used a 90 s deadline. | Keep these values. The single-repository eight-call load demonstrated bounded rejections and later recovery. |
| Storage and retention | Staging defaults to an 8 GiB quota and reserves 384 MiB per job. Three inactive generations and those demoted within 24 hours are protected. One-generation allocated storage was 1.09–1.57 times pack bytes. | Keep the quota and retention rules; measure multi-generation pressure and physical ENOSPC before sizing a larger deployment. |

These timings cover one amd64 host, its tested kernel, local synthetic HTTPS,
and the exact images in the receipt. They do not qualify an official or
large real repository, another architecture, a remote production network,
physical power loss, or a full-size 2 GiB closure.

### Reproduction and evidence

Follow the [Phase 7 procedure](phase7-qualification.md#local-procedure).
The local receipts are in `/tmp/gh-docs-phase7.aXD2TZAw`. The earlier
attempts are kept alongside the final-image runs for comparison. The
final set includes `qualified-runtime-revision`, `qualified-image-ids.txt`,
`qualified-compose-results`, `qualified-phase7.log`,
`qualified-shutdown-repeat.log`, `qualified-capacity.jsonl`,
`qualified-minikube.log`, and `qualified-audit/` with image identities,
SBOMs, vulnerability results, lockfile hashes, and scanner versions.
All Phase 0–7 Compose and Minikube runs exited zero. The capacity and
audit commands also exited zero.

The test harnesses removed their Compose projects and Minikube profiles.
A scoped cleanup check found no Phase 7 containers, networks, volumes,
Minikube home, or `ghdocs` nftables tables. The 41 local Phase 7 image
tags, generated source archives,
Cargo seed, advisory checkout, and prepared SDK fixture were removed.
Receipt files and scanner output remain under `/tmp` for review.

## Phase 8 release qualification

**Release decision: no-go.** The declared profile is private synthetic
rootful Compose with OxiBelt on native Linux amd64 and arm64. The local
qualification host on 2026-11-25 UTC was Linux amd64, kernel
`7.0.0-44-generic`, with ext4-backed `/tmp`, Docker `29.10.1`, Compose
`5.8.0`, and Buildx `0.39.1`. Rust was `1.100.0`, Git in the images was
`2.54.1`, ripgrep was `16.0.0`, and the pinned TypeScript client runtime
used Node `26.13.1` and pnpm `12.21.0`.

### Supply chain

The native OCI export from signed runtime revision `1bd21e1` was imported
into a disposable loopback registry with preserved digests. Each runnable
descriptor matched the subject of its SPDX SBOM and SLSA provenance.
Independent builders produced identical runnable manifest digests for
all three roles. At `11e39a4`, the repeated sync build differed. Revision
`88d6948` switched to the CA bundle already present at the pinned version
in the Alpine base and passed the repeat check. Registry-only inspection
confirmed that the pinned arm64 base contains that CA package.

| Role | Native amd64 manifest |
| --- | --- |
| Serve | `sha256:c6066f0b4bf10de7094dfd6347cb04489e0b3e8a1fbfdd46b56fc37b75160e14` |
| Sync | `sha256:9773b6d54a24cfb625e9fffee05617723a46b3c380600faead165551e03cea3e` |
| Feed | `sha256:ac1a8f945cbd098dae6c95cd2ec41f9c5be5d8f2fec274108762150056792b2a` |

The exact amd64 image audit used Trivy `0.76.2` and its database updated
`2026-11-25T07:11:44Z`. Serve, sync, feed, and the Alpine initializer each
had seven medium and four low findings. PostgreSQL had one critical,
26 high, 31 medium, three low, and one unknown finding. The critical
match was `CVE-2025-68121` in the Go standard library bundled with
`gosu`. The PostgreSQL role starts as UID 70 and skips that root-only
branch. The official `19.0-alpine3.24` tag still resolved to the tested
index `sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f`.
Disposition of these findings remains open.

RustSec database revision `87025f60dbbb1f8820c2bd068cee9140244d4dc9` reported
zero matches in the workspace and pinned ripgrep lockfiles. The workspace
retains the unmaintained `chrono` warning. The shell-free OxiBelt image
produced an empty local package scan. GitHub verified the OxiBelt index
provenance and the native amd64 CycloneDX SBOM at source revision
`e879ac7e263b0af9a86280cd8799de44cbffc05c`. The signed SBOM identified
the proxy binary. The source lockfile audit reported 16 advisory matches.
That workspace lockfile spans multiple builds, so runtime disposition
requires review against the selected data plane and its enabled features.
The available stable OxiBelt tag was `0.10.0`. The audit command collected
all reports and exited 1 on the disposition gate. It listed 27 PostgreSQL
findings and an open OxiBelt lockfile item.

### Qualification evidence

Reproduction commands are in the [Phase 8 runbook](release-readiness.md).
Local receipts are under `/tmp/gh-docs-phase8.QFa4k9qY`. Native ARM64 and
multi-platform assembly require the authorized hosted workflow.
The synthetic TypeScript SDK and wire probes are the declared client scope.
A real-source live smoke, public deployment, product clients, and
power-loss durability require separate qualification.

The locked Rust gate passed on the runtime with the trusted-proxy adapter.
The original Phase 0 run expired during PostgreSQL initialization. The
retry with the startup grace period passed, as did the targeted Phase 0 and
5 rechecks on the exact release manifests. Each of the Phase 0–7 Compose
probes passed on the local amd64 host. The first Minikube run exceeded
the 180-second image import timeout. The isolated rootless retry with
the 420-second import bound passed on Minikube `1.39.0` and Kubernetes
`1.37.0`, including feed-to-MCP publication and Landlock/seccomp probes.
The disposable profile and its temporary home were removed.

The first OxiBelt wire probe exposed an upstream HTTP protocol mismatch.
The HTTP/1.1 backend pin resolved that error. The edge still normalized
`Host` to the hostname and carried the effective port in
`X-Forwarded-Port`. The release listener now validates its actual
OxiBelt peer and reconstructs the configured authority only after the
normalized hostname, TLS scheme and forwarded port match. The exact
release images passed verified TLS, HTTP/1.1 and HTTP/2 discovery, the
forwarded-header and private route checks, and the pinned SDK.

### Acceptance matrix

| Gate | Native amd64 local evidence | Native arm64 |
| --- | --- | --- |
| A01–A04 | Pass. Phase 0 and the release edge exercised pinned discovery, SQL grants and TLS, role and egress isolation, and worker confinement. | Not run |
| A05–A06 | Pass. Phase 1 and 4 exercised native identity, migration and raw byte/path oracles. | Not run |
| A07–A08 | Pass. Phase 2 and 3 exercised approved synthetic acquisition, private staging and candidate validation. | Not run |
| A09–A11 | Pass. Phase 3 exercised interrupted publication, reader and writer fences, retained history and retirement. | Not run |
| A12–A14 | Pass. Phase 4 and 5 exercised scoped visibility, cursors and policy revocation. | Not run |
| A15–A17 | Pass. Phase 5 exercised the search/history oracles, bounded results, overload and worker reap. | Not run |
| A18–A19 | Pass. Phase 6 exercised authenticated control, scheduling and durable outbox recovery. | Not run |
| A20–A21 | Pass for the synthetic workload. Phase 7 and the release drill exercised limits and bounded resource use. | Not run |
| A22 | Pass. Fenced capture, independent restore, corruption denial and repair, coordinated restart, and matched rollback all completed. | Not run |
| A23 | Fail on audit disposition. Local digest, platform, SBOM/provenance and exact proxy checks passed. | Not run |
| A24 | Private runbooks are present. The public source, exposure, privacy, retention and limits review remains open. | Not run |

During the uncoordinated restart attempt, OxiBelt returned a `502` on the
first read after serve restarted. The runbook closes the edge for a
planned serve restart and starts a fresh proxy after the service is healthy.
The coordinated rerun used qualification revision `2bcfd49` with runtime
`1bd21e1`. It restored active, retained and ready generations in both
object formats after removing the source volumes. It checked the exact
raw bytes and history, rotated cursors, recovered interrupted publication,
rejected damaged backups, and blocked a damaged restored repository until
verified repair. The sequence-checked rollback delivered its outbox event
and kept the retained bytes through GC and restart. A separate fresh
restore passed with the Phase 7 images and matching Nginx profile.

The bundle contained 186,155 bytes. Capture took 1.071 s and restore with
verification took 18.712 s, including 0.075 s for the initial digest and
inventory check. The first post-eviction search took 41.75 ms with zero
of eight pack pages resident. Twelve warm samples ranged from 40.76 to
42.94 ms. The eight-call burst returned six bounded admission denials and
the next regular request succeeded. Peak sampled aggregate serve process
RSS was 36,644 KiB. These measurements describe the synthetic fixture and
local host.

The key receipts are `oci-1bd21e1/receipt.json`, `restore-final.json`,
`restore-coordinated.log`, `regressions/`, `minikube-retry.log`, and
`audit-final/disposition.json`. The scoped cleanup removed 23 temporary
image tags and the SDK fixture. The cleanup receipt lists no remaining
Phase 8 projects, builders, Minikube profiles or firewall tables.
The release decision remains no-go while
both the audit disposition and native arm64 qualification are open.

### GHCR publishing workflow

The signed stable and beta release workflow uses the Phase 8 native gate
for both platforms. The full audit still gates assembly and publication.
`actionlint`, the locked offline Rust gate, and the release regression
suite passed. The source checker also accepted the existing canonical
`main` push CI at `40db6c8` through the GitHub API. The disposable registry
test simulated first-package discovery and exercised digest-only bootstrap,
fixed version tags, recovery after partial writes, digest conflicts,
anonymous downloads of two synthetic platforms with attestations, and
stable `:latest` promotion without downgrades.

The local native amd64 build used `d422f73` and the test version
`0.1.0-beta.1`. The first independent build timed out reading the Cargo
index inside BuildKit. A repeat build with the locked cache staged under
`/tmp` reproduced all three runnable digests and verified the release
labels and attestation subjects. Phases 0–7 passed under rootful Compose
with synthetic fixtures. Phase 1 passed after rerunning with lowercase
aliases for the same fixture images.

The exact-image Phase 8 restore and matched rollback rehearsal passed.
The supplemental rootless Minikube smoke passed on Minikube 1.39.0 and
Kubernetes 1.37.0. Cleanup removed the disposable cluster, fixture image
tags, OCI layouts, and local Cargo seed. No scoped Compose resources or
nftables tables remained.

The exact-image audit exited at the disposition gate. Trivy 0.76.2
with its database updated at 13:07 UTC on 2026-11-25 reported 27
critical or high PostgreSQL findings. RustSec revision `b9b8a3c2`
reported 17 OxiBelt workspace lockfile matches and zero workspace or
ripgrep matches. Verification of the pinned OxiBelt provenance and
native SBOM succeeded. The audit could not resolve the yank status of
some OxiBelt dependencies from the local crates.io cache.
Native arm64 qualification and live GHCR verification remain open.

### Release version stamping

The `1.0.1` publication stopped at source verification. Its signed tag
pointed to a commit with Cargo workspace version `0.1.0`. The tag signature
and exact-commit `main` CI passed independent checks. GitHub marks the
published release immutable, so the replacement target is `1.0.2`.

On 2026-11-25, local qualification of signed revision `fcc2451` used
Linux amd64, ext4-backed `/tmp`, rootful Docker `29.10.1`, Compose `5.8.0`,
Buildx `0.39.1`, Rust `1.100.0`, Node `26.13.1`, and pnpm `12.23.0`.
Workflow lint, the locked offline Rust gate, all nine release tests with
the disposable registry, the frozen pnpm install, and Phase 0 Compose
passed. Frozen Cargo metadata resolved the six production packages at
the full staged stable and beta versions and the test package at `0.0.0`.

Two independent builders reproduced all three runnable amd64 manifests
for `1.0.2`. The exact-image Phase 8 restore and matched rollback passed.
The restored MCP server reported `1.0.2` through the OxiBelt edge.

The exact-image audit reproduced both stamped Cargo input hashes and
completed frozen Cargo metadata resolution. Trivy `0.76.2` with its
database updated at 13:07 UTC found one critical and 26 high PostgreSQL
findings. RustSec revision `b9b8a3c2` found 17 matches in the OxiBelt
workspace lockfile and zero in the stamped workspace and ripgrep
lockfiles. OxiBelt provenance and native SBOM verification passed. The
findings still block release. Scoped Docker and firewall resources were
removed after the synthetic run. Native arm64, hosted qualification of
the replacement revision, and GHCR publication remain pending.

### PostgreSQL release image and proxy inventory

The hosted `1.1.0` amd64 and arm64 jobs passed the Rust, repeat build,
Compose, and restore gates. Both audits reported 26 Go standard-library
findings in PostgreSQL's `gosu`, one `libxml2` finding, and 17 RustSec
findings in the OxiBelt workspace lockfile. [amd64 audit evidence](https://github.com/PublicData-stream/gh-docs-mcp/actions/runs/45655773084/artifacts/15046946331) · [arm64 audit evidence](https://github.com/PublicData-stream/gh-docs-mcp/actions/runs/45655773084/artifacts/15046123302)

The PostgreSQL image at revision `e5d4359` keeps the official PostgreSQL
19.0 Alpine base, updates `libxml2` to `2.13.9-r3`, and removes `gosu`
from the runnable filesystem. A local Linux amd64 probe confirmed UID/GID
70, root startup rejection, and zero high or critical findings in Trivy
`0.76.2`. The candidate reopened a PostgreSQL 19 data directory initialized
by the previous image and read back a persisted row.

The first independent OCI builds reproduced the three application images.
The PostgreSQL layer comparison isolated the differing bytes to APK's
wall-clock transaction log. Revision `35033f0` keeps the transaction
output in the build log. The full native amd64 repeat build reproduced all
four runnable manifests. The PostgreSQL manifest was
`sha256:ee9dffa3bc2b184ee5f670c22c5a20924ffeec50aa522f2907f04c69103901b4`.
The exact-image audit used Trivy `0.76.2` with the 2026-11-26 07:04 UTC
database and RustSec revision `b9b8a3c2`. It reported zero high or critical
PostgreSQL findings and zero RustSec matches in the stamped workspace and
ripgrep locks. The only release blockers were the 17 OxiBelt workspace
matches and the incomplete signed proxy inventory.

The exact-image Phase 8 rehearsal passed on rootful Docker `29.10.1`
and ext4, with Compose `5.8.0` and the pinned Node and pnpm client. It
restored the matching backup, exercised the previous role binaries and
database image, then reopened that populated PostgreSQL 19 data directory
with the candidate image and verified MCP and catalog readback.
The restricted Phase 7 Minikube smoke on Kubernetes `v1.37.0` used the
exact amd64 Serve and PostgreSQL manifests. Feed-to-MCP publication and
framed worker denial checks passed, and the disposable profile was removed.

The offline Rust gate, workflow lint, and all 12 release tests passed,
including four-image verification and recovery in a loopback registry.
Compose phases 0, 1, and 3–7 passed in sequence. Phase 2 initially failed
the global-admission contention probe while the builds were running and
passed its full isolated retry. The scoped Compose, registry, image, and
firewall resources were removed. The sanitized reports remain under `/tmp`.

The new proxy gate verified the actual signed OxiBelt `0.10.0` index and
native SBOM attestations for both architectures and rejected both SBOMs
for missing compiled Cargo dependency graphs. `0.10.0` was still the
latest published release on 2026-11-26. Release requires an official
patched OxiBelt release with complete signed inventories and successful
native qualification on both architectures. [OxiBelt releases](https://github.com/OxiBelt/OxiBelt/releases)

## Routine GitHub Actions CI

[CI](../.github/workflows/ci.yml) runs the pinned Rust and release-helper
checks, workflow lint, and synthetic Docker Compose Phases 0 and 5 on every
push, pull request, and manual dispatch. The Compose jobs use native Linux
amd64 and arm64 runners. Select `full_compose` when dispatching the workflow
to run Phases 0–7 on both architectures. Failed Compose jobs attach the
available host, build, and phase logs for seven days.

Release preflight checks the entire canonical main push run and the `rust`
job for the exact tagged commit. Release candidate qualification exercises
the separate repeat build, restore, and audit gates.

On 2026-11-26, the CI changes based on `367c17ba4dac` passed the Rust
`1.100.0` offline gate, checksum-verified actionlint `1.17.1`, and the
offline release-helper suite. Eleven release-helper tests passed and the
opt-in loopback registry test was skipped. ShellCheck passed for both
Compose helpers.

The routine selection passed Phases 0 and 5, and the full selection
passed Phases 0–7, including the Phase 2 global-admission contention
probe. The local profile was Linux amd64, kernel `7.0.0-44-generic`,
rootful Docker `29.10.1`, Compose `5.8.0`, and ext4-backed `/tmp`.
The SDK fixture used Node `26.13.1` and pnpm `12.23.0`. A missing-image
run returned failure and produced diagnostics for both requested phases.
Fixture tags, Compose resources, firewall rules, and generated fixture
directories were removed. The new hosted CI matrix is pending its first run.
