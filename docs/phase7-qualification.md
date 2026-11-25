# Phase 7 security and capacity qualification

## Threat model

The deployed entry point is the private TLS proxy. Anyone who can reach it
can issue untrusted MCP requests. The app checks the declared protocol,
authority and origin, limits body size and concurrent calls, and parses
requests into typed selectors. Source text, paths, author metadata and
notices remain untrusted content in tool results. Redistributing source
bytes or derived output requires the corresponding reviewed approval.
Every read checks current policy. Revocation also applies to retained
generations and cursor resumes.

Treat the upstream as capable of sending malformed Git protocol records,
objects, graphs, and compressed packs. Only sync can contact an approved
upstream. Explicit HTTPS identity, DNS and destination controls, the
transport broker, staged verification, and size and time limits bound that
authority. The Git process receives service-owned config and fixed
arguments. An object present on the filesystem becomes readable only through
an approved ref in a visible generation. A ready or quarantined candidate
has no public authority.

A compromised feed can send authenticated hints within its identity and
may consume its bounded share of scheduling capacity. It cannot authorize
a source, access the database or store, or publish data. A notification
only prompts a lookup of the SQL pointer. Keep the pairwise link keys,
cursor key, role credentials and migration authority separate. A
compromised worker can try to read or write files, reuse a socket, fork, or
launch a helper. The Git reader has read access to one validated bare
store. Search only consumes framed approved bytes. Both clear the
environment and inherited descriptors and apply kernel restrictions
before accepting data.

Database disconnect, interrupted storage writes, disk pressure and
corruption must preserve prior usable closures or visibly block the
affected repository. PostgreSQL authorizes visibility. Repository and
generation fences protect workers until reap, including if the advisory
connection disappears. Write-side recovery verifies the pointer, manifests,
objects and retention roots before reopening access. A compromised host or
database owner can change these controls. The deployment relies on
trusted host and database administration. Synthetic process interruption
tests exercise recovery on the tested filesystem and kernel.

## Local procedure

Build the normal serve, sync and feed images and the earlier fixture targets at
the same revision on linux/amd64. Buildx can take a read-only
`cargo-registry-seed` named context with the locked Cargo sparse index and
crate archives for a disconnected registry. Keep that input under
`/tmp`. The production compilation step still uses frozen inputs in a
networkless build step. Record the exact image IDs and check the embedded
MCP schemas against the tracked snapshot.

Use the [Phase 0 prerequisites](deployment-prerequisites.md) and the
[Phase 6 target list](control.md#qualification). Run the offline Rust
gate and the Phase 0 through 6 Compose probes serially. Then run:

~~~sh
sudo -n python3 -B scripts/phase7/integration.py --image-tag "$tag"
sudo -n python3 -B scripts/phase7/capacity.py --image-tag "$tag" --out "$receipt/capacity.jsonl"

python3 -B scripts/phase7/minikube.py \
  --serve-image "gh-docs-mcp-serve:$tag" \
  --sync-image "gh-docs-phase6-sync:$tag" \
  --feed-image "gh-docs-phase6-feed:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-phase7-postgres:$tag" \
  --bootstrap-cache "$bootstrap_cache"
~~~

Set `receipt` to a fresh directory under `/tmp`. Use a local alias of
the digest-pinned PostgreSQL image. The disposable Restricted smoke uses
pod loopback, proves feed-to-MCP publication and checks framed worker
denials. Compose supplies the network and role-boundary evidence.

Run `scripts/phase7/audit.sh "$tag" "$receipt/audit" "$trivy_archive" "$rustsec_db" "$ripgrep_lock"`
through the privileged command channel. Use the official Trivy 0.76.2
Linux amd64 archive, a snapshot of the RustSec database, and the root
`Cargo.lock` from the checksum-verified ripgrep source referenced in the
Dockerfile. The image audit captures the active database snapshot and
scans exact local image IDs. Review the source-built programs separately
because OS package inventory can miss them.
[Trivy image scanning](https://trivy.dev/docs/latest/target/container_image/) ·
[RustSec Advisory Database](https://rustsec.org/) ·
[Git advisories](https://github.com/git/git/security/advisories) ·
[ripgrep advisories](https://github.com/BurntSushi/ripgrep/security/advisories) ·
[Rust releases](https://doc.rust-lang.org/releases.html) ·
[Rust SDK advisories](https://github.com/modelcontextprotocol/rust-sdk/security/advisories) ·
[TypeScript SDK advisories](https://github.com/modelcontextprotocol/typescript-sdk/security/advisories)

## Reading the measurements

Capacity samples include TLS and HTTP request time. Warm requests follow
three warm-up calls and are offered at no more than 48 starts per second
per cell. Each warm cell stops offering new requests after 20 seconds.
Compare the recorded offered and completed rates. The
single-concurrency pack-eviction test calls `posix_fadvise` on the
canonical fixture pack and index and checks residency with `mincore`.
Only fully evicted samples contribute to those latency percentiles.
Database, process and filesystem metadata caches are outside that
definition. The cgroup snapshots report cumulative counters and lifetime
peaks. Worker counts are sampled every 25 milliseconds and can miss
short-lived processes. Summed process RSS can count shared pages more
than once. The lifecycle run measures full and incremental
fetch individually. A separate two-repository run compares one and two
sync slots through paced synthetic HTTPS. It samples the
running jobs and separately reports the durable scheduler queue count.

Treat search and history continuation as partial work. The report counts
admitted complete and partial results separately from budget rejection.
Use a minimum of 100 comparable samples for p99. A single fixture host
can establish a bounded operating envelope for those workloads. Release
qualification of other repositories, hosts and architectures needs
additional evidence. See the [observed results](implementation-status.md).
