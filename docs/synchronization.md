# Private Git synchronization

Phase 2 prepares verified candidates on a single Linux host. The operator
invokes one reconciliation per repository. PostgreSQL records the source
approval, job outcome, and private artifact. Phase 3 owns installation and
publication. The production allowlist starts empty. Qualification uses
synthetic sources only.

## Source approval

Use the migration credential to register and revoke approvals. Mount approval
documents read-only into the operator service. Each registration for the same
repository and canonical URL advances the policy version. A revoked approval
can be replaced by a newly reviewed version. Repository and source are UUIDv4.
Phase 2 accepts one canonical HTTPS source per repository and an exact DNS
hostname on port 443.

~~~json
{
  "repository": "11111111-2222-4333-8444-000000000001",
  "source": "11111111-2222-4333-8444-000000000011",
  "format": "sha256",
  "version": 1,
  "url": "https://git.example.test/organization/repository.git",
  "hosts": ["git.example.test"],
  "required_refs": ["refs/heads/main"],
  "optional_refs": ["refs/tags/v1"],
  "prefixes": ["refs/heads/releases/"],
  "reviewer": "Example operator",
  "reviewed_at": "2002-02-02T02:02:02Z",
  "full_closure_approved": true,
  "output_scope": "Synthetic original bytes and commit metadata",
  "license_evidence": "Operator-owned test fixture",
  "notice_evidence": "Fixture notice reviewed",
  "limits": {"pack_bytes": 268435456, "response_bytes": 402653184, "objects": 500000, "deadline_seconds": 900}
}
~~~

Exact refs and prefixes also accept an object with a `base64` field containing
canonical padded base64 of the original ref bytes. Prefixes end with a slash
and match the branch or tag namespace. An absent optional ref is a deletion
when it was present in the previous observation. An absent required ref, an
unapproved default branch, or a noncommit root fails reconciliation. Annotated
tags support up to 16 peel steps. Full-closure approval covers every Git object
reachable under these refs, including the history and full tree.

~~~sh
docker compose --profile operator run --rm --no-deps migrate --migrate
docker compose --profile operator run --rm --no-deps \
  -v "$approval_file:/approval.json:ro" migrate --register-source --approval-file /approval.json
docker compose exec -T sync gh-docs-sync --reconcile --repository "$repository_uuid"
docker compose --profile operator run --rm --no-deps \
  migrate --revoke-source --source "$source_uuid"
~~~

Install the project firewall before starting the role containers. The default
profile closes live sync egress. For an approved host, provision a trusted
resolver through `SYNC_DNS` and install rules using
`scripts/phase2/egress.py` with the Compose network file, resolver IP, and
reviewed public HTTPS ranges. The broker checks every returned destination
before connecting and verifies the original TLS hostname. It accepts only
read-side Git Smart HTTP v2 requests and rejects redirects. Update the host
firewall when an approved host changes addresses.

## Verification and recovery

Sync holds the repository coordinator through worker teardown and takes one
global slot. Set `GH_DOCS_SYNC_CONCURRENCY` consistently in the single Compose
sync service. The default is 2 and the range is 1–4. Provision host headroom
and set `SYNC_MEMORY_LIMIT=8g` before raising concurrency to four. Each slot
has a 1 GiB tmpfs and a separate process group. The storage quota defaults to
8 GiB. Lower `GH_DOCS_SYNC_QUOTA_BYTES` only with capacity for the candidate
reservation of each configured slot. Admission checks free space and inodes.

Git uses a fixed executable, scoped scratch access, a syscall allowlist, and
capped diagnostics. Each source has bounded request pacing, response bytes,
fetch attempts, and a deadline. Strict verification uses the pinned Git
defaults with zero per-message overrides. The candidate also checks pack and
index contents, native object IDs, object closure and counts, inflated sizes,
delta depth, tree depth, and tag peeling. Tree and tag verification has a
96 MiB framed metadata cap.

Artifacts live in `/git-sync-private/incoming/<job UUIDv7>/`. A bare `odb/`
and its sibling `manifest.json` form the artifact. The manifest uses the v1
canonical encoding and the `https_git_checks` method. PostgreSQL stores the
logical, observation, and pack digests separately. A logical no-op records an
observation without allocating another generation. Sync revalidates the
newest matching baseline, negotiates using copied packs, and writes a fresh
pack containing the current approved closure. Confirmed input corruption
triggers one fresh attempt. An unconfirmed integrity check preserves the
existing baseline and fails the job.

Sync keeps at most two usable private artifacts per repository. The database
marks older artifacts retired before disk cleanup. A later reconciliation
examines the job outcomes and resolves interrupted seals under the repository
fence. Quarantine holds suspect artifacts and bounded transfer samples in
`/git-sync-private/quarantine/`, up to 1 GiB within the staging quota. It
removes its oldest diagnostics when needed. Use the JSON job UUID and reason
code to correlate outcomes with PostgreSQL, which is authoritative when a
commit response is lost.

Use the Phase 2 migrator after applying migration `0003`. Roll back the
application binaries with the additive schema in place. Restore a coordinated
database and private-store snapshot to roll back the schema.

## Qualification

Build uniquely tagged local images for `serve`, `sync`, `feed`,
`phase1-serve`, `phase1-sync`, `phase2-sync`, and `phase2-upstream`. Run the
offline gate and Phase 0 and Phase 1 probes, then use the same local tag for
the Phase 2 suites. [Deployment prerequisites](deployment-prerequisites.md) ·
[Storage qualification](storage.md#reproduce-the-qualification)

~~~sh
sudo -n python3 -B scripts/phase2/integration.py --image-tag "$tag"
python3 -B scripts/phase2/minikube.py \
  --sync-image "gh-docs-phase2-sync:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-phase2-postgres:$tag" \
  --bootstrap-cache "$bootstrap_cache"
~~~

The PostgreSQL alias must refer to the exact Compose base image. The Minikube
Job uses locally loaded images and a disposable rootless profile. Both
harnesses keep fixture inputs under `/tmp`.

[Source approval decision](decisions/0004-source-approval.md)
