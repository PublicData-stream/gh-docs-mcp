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
committed even if the pointer has advanced again. Events stay pending until
the authenticated delivery work in Phase 6. Fresh reads use the catalog.

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

## Backup contract

Quiesce approval and sync writers, finish or recover pending canonical
operations, and capture a matching PostgreSQL dump and canonical store
inventory under exclusive repository fences. Include pack files and
sidecars, immutable manifests, retention refs, the outbox and policy
records. Back up private staging and quarantine separately if required.
Restore to fresh volumes with ingress closed. Recreate lock inodes only
with readers and writers stopped, then verify catalog health, all protected
closures, role grants and a pinned read before restoring ingress. Phase 8
will rehearse the procedure. Phase 3 process interruptions do not qualify
power-loss behavior.

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
