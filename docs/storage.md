# Native Git storage and catalog

## Layout and ownership

```text
/git-store/repositories/<repository UUIDv4>/
/git-store/manifests/<repository UUIDv4>/
/git-store/locks/<repository UUIDv4>/coordinator.lock
/git-store/locks/<repository UUIDv4>/repository.lock
/git-store/locks/<repository UUIDv4>/generation-<decimal BIGINT>.lock
/git-sync-private/incoming/<job UUIDv7>/
/git-sync-private/quarantine/
```

The networkless volume initializer owns mount-root provisioning. Sync UID/GID
10002 initializes the subdirectories and owns store changes. Serve UID/GID
10001 mounts the canonical store read-only. Incoming and quarantine have mode
0700 under a separate sync-only mount. Feed has neither mount. Directories
use canonical lowercase UUID text. Filesystem locks live outside the affected
Git directories. Repeated lock initialization validates the inode and syncs
its parent. Runtime operations keep these inodes in place.

The reader accepts a service-owned bare configuration and an inert
`HEAD` pointing to `refs/heads/main`. Catalog ref snapshots supply the logical
default ref and exact approved roots. The reader inspects the selected ODB
using directory descriptors and requires expected ownership, safe modes,
single-link regular files, and allowed directory entries. Its Linux host needs
`openat2`, the existing worker confinement prerequisites, and the qualified
mount semantics. Alternates, grafts, shallow state, promisor objects, symlinks,
and configuration includes cause admission to fail.

The canonical reader holds a shared repository lock through worker reap.
Incoming reads run under the owning job or fixture process. The advisory-key
registry reserves numbers below 1024 for other uses and allocates one key per
typed lock scope. Future generation-aware admission, publication, and
maintenance follow the order from the storage decision.
[Storage and locking decision](decisions/0001-storage-identity-and-locking.md)

## Migration and visibility

Run the existing one-shot `migrate` Compose service with its dedicated
migrator credential. The runner applies the original foundation migration
and then adds `git_catalog` in a separate transaction under the same
migration advisory key. The catalog and its version row commit together.
Reapplying the command preserves existing generations and ref snapshots.
Back up the database before the upgrade. Application rollback can use the
Phase 0 binaries with the additive schema in place. Use a coordinated restore
to return to the prior database state.

Sync has scoped catalog reads and three fixture routines for registration,
candidate creation, and final disposition. Registration requires a synthetic
URN and an exact reviewed ref scope. Candidate IDs are allocated atomically
per repository. A finished snapshot is immutable through the runtime role.
Serve can query the visibility projection. Phase 1 fixture transactions leave
the active generation, jobs, and outbox empty. Create later transitions and
write grants with their owning phases.

## Reader contract

The catalog stores native OIDs as binary SHA-1 or SHA-256 digests and ties the
format to the repository and generation. Input OIDs use exact full lowercase
hex. The reader validates object headers and tree mode/type pairs. Blob reads
preserve bytes, including line endings and embedded NUL. Byte ranges are
zero-based with an exclusive end. Paths use tagged UTF-8 text when valid
and canonical padded base64 otherwise. A symlink returns blob bytes. A
gitlink supplies its native commit pointer as metadata.

The worker uses pinned static Git with binary output framing and an ODB-scoped
Landlock rule. The parent supervises and reaps it under a 15-second reader
deadline. Initial limits are 8 MiB per blob, 1 MiB per non-blob body, 128 MiB
of object payload per batch, 50,000 object probes, and 16 annotated tag hops.
Store inspection has independent entry and depth limits. Corrupt framing or
an unexpected object type fails the operation. Exhausted ancestry budgets
produce an indeterminate reachability result. Read qualification considers
both native hash formats. HTTPS fetch and strict closure validation have
their own gate.

## Proposed manifest version 1

Encode compact UTF-8 JSON with no trailing newline. All structured fields are
ordered arrays. Use lowercase UUIDs and full lowercase hexadecimal OIDs,
positive policy version as a decimal string, and canonical padded RFC 4648
base64 for raw ref bytes. Sort ref entries by raw ref bytes and reject
duplicates. The default ref must name an entry in the map.

```text
envelope = ["gh-docs-manifest", 1, content, observation]
content = [repository_uuid, source_uuid, object_format, policy_version,
           default_ref_base64, refs]
refs = [[ref_base64, target_type, native_oid_hex, peeled_commit_hex_or_null], ...]
observation = ["verified", "fixture_checks", observed_utc]
```

Render `observed_utc` in UTC with six fractional digits followed by `Z`.
Hash canonical content with SHA-256 after the prefix
`gh-docs-manifest-content-v1` and a NUL byte. Hash the canonical envelope with
SHA-256 after the prefix `gh-docs-manifest-observation-v1` and a NUL byte.
The logical digest is stable across observation times and object repacks.
The fixture driver records the envelope digest on verification and tests
the version 1 encoding against a golden vector. Manifest file installation
belongs to publication.

## Reproduce the qualification

Prepare the pinned dependencies and Phase 0 TypeScript client using the
[deployment prerequisites](deployment-prerequisites.md). Build local images
with one unique tag, then run the rootful Compose probes and supplemental
Minikube reader smoke from the repository root.

```sh
export CARGO_TARGET_DIR=/tmp/rust/target
cargo fetch --locked
scripts/phase0/check-rust.sh
client_dir=$(scripts/phase0/prepare-client.sh)
tag=phase1-local

for target in serve sync feed phase1-serve phase1-sync; do
  case "$target" in
    phase1-*) image="gh-docs-$target" ;;
    *) image="gh-docs-mcp-$target" ;;
  esac
  docker --context default buildx build --network host --platform linux/amd64 \
    --load --target "$target" -t "$image:$tag" .
done

sudo -n python3 -B scripts/phase0/integration.py --skip-build --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
sudo -n python3 -B scripts/phase1/integration.py --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"
python3 -B scripts/phase1/minikube.py --serve-image "gh-docs-phase1-serve:$tag"
```

Use a disposable Minikube profile on a Docker engine with a working default bridge. Pass
`--docker-context rootless` to use the rootless engine. The smoke
creates a hardened Job with locally loaded images, no service account token,
and a read-only fixture mount for the reader. Its Kubernetes version defaults
to `v1.37.0` and bootstrap files can be copied from an existing cache with
`--bootstrap-cache`. The Job tests reader confinement only. Compose exercises
database roles, network isolation, and migration. Remove the local fixture
client files and images created by the run after collecting evidence.
