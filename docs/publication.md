# Git publication and recovery

Each approved repository has a canonical bare Git object store. PostgreSQL
holds its active generation pointer, the publication sequence, and an outbox
event for each committed pointer change. A generation has an immutable,
approved ref snapshot. The SQL pointer and policy determine visibility.

## Operator workflow

Run these commands in the sync role with its scoped database credentials and
writable store. Pass `--repository UUID` immediately after the command. Use
canonical decimal generation and sequence values. Source registration and
revocation use the separate migration role.

| Command | Effect |
| --- | --- |
| `--reconcile --repository UUID` | Fetch and verify a private candidate, or record a no-change observation. |
| `--prepare --repository UUID --generation N` | Reverify a usable candidate, install its canonical pack and retention roots, and record it as ready. |
| `--publish --repository UUID --generation N --expect-sequence S` | Activate a ready generation. Use sequence `0` for the first publication. |
| `--rollback --repository UUID --generation N --expect-sequence S` | Reactivate a retained generation at a new publication sequence. |
| `--status --repository UUID [--sequence S]` | Inspect the pointer, health, pending work, and the requested or current outbox event. |
| `--recover --repository UUID` | Reconcile pending work, check the canonical inventory and protected closure, and reopen a verified repository. |
| `--discard --repository UUID --generation N` | Discard a verified or ready candidate and detach any installed roots. |
| `--gc --repository UUID` | Recover pending operations, retire eligible generations, and run guarded Git repack and expiry. |

A sequence conflict is safe to report as a failed operation. After losing a
publish or rollback response, inspect the event at the attempted next sequence
with `--status --sequence`. The event identifies the generation that actually
committed even if the pointer has advanced again. Phase 6 retries pending events. Fresh reads use the catalog.
[Scheduler and delivery runbook](control.md)

## Fences and durability

A writer takes the filesystem coordinator, its PostgreSQL session advisory
lock, the repository filesystem fence, and its session advisory lock. Canonical
commands then take the sync-private storage admission lock and the needed
generation fences. For multiple generations, acquire filesystem and advisory
fences in pairs in ascending generation ID order. Reconcile takes the filesystem coordinator before checking slot capacity.
It checks approval and pairs that coordinator with its advisory lock before
starting a job. It holds both during fetch and uses short SQL statements to
seal a candidate.
The policy writer takes only the repository advisory fence before changing
approval rows. Its database role has no store mount.

A reader takes shared repository and generation fences in the corresponding
order. It checks approval and visibility in a fresh read-only READ COMMITTED
statement after acquiring the generation fence. Its worker can access only
the selected bare ODB. The worker owner keeps the filesystem fences until
the child is reaped, including after loss of the database session. A policy
change waits for the database read fence and prevents subsequent admission
under the old policy. An accepted operation with a healthy session can
finish under its original approval.

The catalog allocates unique advisory keys beginning at 1024. Migration and
foundation probes reserve keys 1 and 2. The filesystem lock inodes remain
stable for the life of the store. Writer waits are bounded and a busy
operation can be retried. The production commands require the ext4-backed
local filesystem profile.

Prepare revalidates the private snapshot, copies validated pack and index
bytes into the canonical filesystem, and installs them without overwriting
existing names. Retention roots use `refs/gh-docs/retention/<generation>/<index>`,
where the index follows the byte order of approved ref names. Prepare creates
them with a Git transaction in a confined scratch ODB and moves the complete
directory into place. Annotated tags are retained by native tag OID. The
versioned manifest stays at `manifests/<repository>/<generation>.json`.
Files and containing directories are flushed before committing ready state.
Promotion compares the expected sequence and commits demotion, activation,
pointer and outbox together with synchronous commit enabled.

## Retention and repair

Active and ready generations keep their roots. The three most recently
demoted generations and all generations demoted within 24 hours remain
protected. GC commits unreadable tombstones before detaching roots. A pin to
a retired generation fails even when another generation or the grace period
keeps its objects present. Revoked or superseded approvals deny reads
independently of retention.

Explicit GC copies the registered pack inventory into bounded scratch space.
It repacks with cruft retention and a 14-day Git object-age cutoff, verifies
the protected refs and native closure there, installs checked replacements,
and removes only the recorded obsolete files. The repository stays blocked
during replacement and while interrupted maintenance needs recovery. Store
admission caps a repository at 1 GiB of pack components and reserves at least
128 MiB of filesystem headroom. Scratch usage is included in admission.
Automatic Git maintenance is disabled.

On corruption or unrecognized canonical data, keep the repository blocked and
retain evidence. Use status and the incident record to identify the affected
repository and restore missing or corrupt bytes from verified storage.
Recovery rechecks pack checksums, manifests, roots, pointer and outbox, and
Git object closure before clearing the block. Shared corruption can affect
multiple generations. Recovery retains ambiguous files for operator
inspection and does not fetch replacement content. Staging used for
canonical work is sync-owned and inaccessible to serve.

## Backup and restore

Use the rootful Compose maintenance runner on a trusted single-host
deployment. Export the same private Compose variables used to start the
project and pass the same project name and Compose files. Use the
PostgreSQL Unix socket through Compose and keep its host ports private.

1. Close the ingress proxy, stop feed, and drain and stop serve and sync.
   Exclude other operators and one-shot jobs for the maintenance window.
   Recover interrupted publication and settle uninstalled candidates by
   preparing or discarding them. Stop the recovery containers before capture.
   Busy fences or unsettled catalog state abort the backup.
2. Invoke `backup.py backup` with a fresh private bundle directory. It takes
   the migration advisory lock and existing repository fence pairs, freezes
   catalog DML while allowing `pg_dump`, and checks the inventory and pointer
   again before releasing fences. The bundle holds `catalog.dump`, `store/`,
   and `manifest.json`. Save the reported manifest SHA-256 through an
   independent trusted channel. Secure and encrypt the backup and preserve
   numeric ownership on the independent copy. Keep secrets, private staging
   and quarantine separately. Retain or remove any failed partial bundle
   as incident material under the operator's policy.
3. Provision a fresh destination with the same scoped roles, a fresh
   PostgreSQL instance, an initialized empty canonical mount, and the
   private ingress closed. Provision independent TLS, DB and HMAC secrets
   and a new cursor key and key ID. Start only PostgreSQL.
4. Verify the copied bundle using the independently held manifest digest.
   Run `restore` with that digest. It restores into the un-migrated database
   with object owners and grants, recreates lock files in the empty store and
   runs fenced recovery including strict fsck, manifests, retention roots
   and pointers. On failure, retain the closed destination for investigation
   and retry on fresh volumes after repair.
5. Reapply revocations made after capture, apply any compatible migration,
   and confirm role grants and readiness. Through the private proxy check
   active and retained read bytes and history, check ready generations stay
   private and verify notification delivery after starting sync. Old cursors
   expire with key rotation. Fresh cursors should continue against the same
   approved generation. Open approved ingress only after these checks.

```sh
sudo -E python3 -B scripts/phase8/backup.py backup "$bundle" \
  --project "$project" --compose-file compose.yml --compose-file deploy/compose.release.yml
python3 -B scripts/phase8/backup.py verify "$bundle" --manifest-sha256 "$backup_digest"
sudo -E python3 -B scripts/phase8/backup.py restore "$bundle" --manifest-sha256 "$backup_digest" \
  --project "$restore_project" --compose-file compose.yml --compose-file deploy/compose.release.yml
```

Recover damaged content from independently verified bytes. Rollback may
encounter the same damage when generations share a pack. The restore
rehearsal measures fixture size and elapsed time. Process interruption and
ext4 tests leave power-loss durability and production recovery objectives
unqualified. [Phase 8 rehearsal](release-readiness.md)

## Qualification

Build the Phase 0–2 images from the current revision as described in the
[storage](storage.md#reproduce-the-qualification) and
[synchronization](synchronization.md#qualification) guides. Build the
additional fixture images with the same local tag, then run the Phase 3
Compose and Minikube probes. Alias the pinned Compose PostgreSQL image
locally as `gh-docs-phase3-postgres:$tag` before Minikube loads it.

~~~sh
for role in serve sync; do
  docker --context default buildx build --network host --platform linux/amd64 \
    --load --target "phase3-$role" -t "gh-docs-phase3-$role:$tag" .
done

sudo -n python3 -B scripts/phase3/integration.py --image-tag "$tag"
python3 -B scripts/phase3/minikube.py \
  --serve-image "gh-docs-phase3-serve:$tag" \
  --sync-image "gh-docs-phase3-sync:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-phase3-postgres:$tag" \
  --bootstrap-cache "$bootstrap_cache"
~~~

The Compose probe injects faults with synthetic inputs and checks A08–A11.
The Minikube Job smoke-tests both Git formats and the read-only serve mount
using a disposable rootless profile. See [observed results](implementation-status.md).
