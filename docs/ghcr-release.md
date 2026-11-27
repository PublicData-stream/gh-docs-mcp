# OCI release artifacts and GHCR publication

The [release-candidate workflow](../.github/workflows/release-candidate.yml)
can be run manually to produce a read-only qualification bundle. The
[publishing workflow](../.github/workflows/publish-images.yml) calls the
same native amd64 and arm64 jobs at the signed tag's commit. Both jobs
must pass the repeat build, Compose probes, restore rehearsal, and the full
Phase 8 audit before assembly. Each role has one OCI index containing both
runnable platform manifests and their attached SPDX SBOM and SLSA provenance.
The repeat check compares runnable manifests from independent builders.

The combined artifact includes `release.json` and `SHA256SUMS`. Retain
both native evidence artifacts with the bundle. They include host and test
evidence, the stamped Cargo inputs and inventory, exact image audits, and
the pinned Git source archive. The combined bundle is retained for 14 days,
native evidence for 30 days, and the publication receipt for 90 days.

## Cargo versions

Keep `workspace.package.version` and the committed first-party Cargo lock
entries at `0.0.0`. Production packages inherit the workspace version and
the test-support package declares `0.0.0` explicitly. Unstamped builds
report `0.0.0-dev.archive` over MCP.

The native image build applies the full publishing tag in a disposable archive
of the verified commit. It changes the root Cargo workspace version and
the production package entries in the copy of `Cargo.lock`. A build tag with
an all-digit SHA prefix that starts with zero uses `X.Y.Z-build+<sha8>` in
Cargo to satisfy SemVer. The MCP server, OCI labels, and build receipt use
the original tag in all cases. Audit evidence records
the stamped files and verifies their hashes against the native build receipt.
To rebuild, pass the same revision and full version to
`scripts/phase8/images.py build`. Manual qualification defaults to
`0.0.0-rc.1`. The bare `0.0.0` version is reserved for development.

## Publish a release

1. Use a commit on `main` whose newest exact-commit branch-push CI run
   passed. Complete the [release readiness review](release-readiness.md),
   including applicable license, notice, and corresponding-source
   arrangements before public distribution.
2. Choose `X.Y.Z` for a stable release or `X.Y.Z-beta.N` for a beta.
   Create an annotated GPG-signed tag with `git tag -s`, verify it with
   `git tag -v`, and push that tag. GitHub must be able to verify the signing
   key.
3. Refresh `origin/main` and run preflight with an authenticated `gh` that
   can read the repository, Actions runs, and checks. For example, run
   `python3 -B scripts/phase8/release_source.py preflight 2.0.0`. It checks
   the exact signed tag, canonical `main` CI, and version stamping.
4. Publish a GitHub Release for that existing tag. Mark beta releases as
   prereleases. Publishing the release authorizes the workflow to write the
   stable or beta indexes after qualification.

## Publish a build tag

1. Complete the same release readiness review. Select a commit containing
   the build-tag workflow whose newest exact-commit `main` push CI run
   passed, and refresh `origin/main`.
2. Create a new GPG-signed annotated tag using the first eight lowercase
   hex characters of the full commit SHA. Run preflight before pushing.

   ~~~sh
   revision=$(git rev-parse --verify 'HEAD^{commit}')
   tag="2.0.0-build.$(printf '%.8s' "$revision")"
   git tag -s "$tag" "$revision" -m "$tag"
   python3 -B scripts/phase8/release_source.py preflight "$tag"
   git push origin "refs/tags/$tag"
   ~~~

3. Creating the remote tag authorizes the same public GHCR publication
   and anonymous verification as a release. The workflow checks GitHub's
   verification of the annotated tag and uses the exact build tag on all
   four indexes. If the source gate fails because CI is pending, rerun the
   failed jobs after CI passes. Push each build tag individually.

## Verify public package access

The workflow links the four packages to this repository through their OCI
source label. New packages start private. A package admin makes each
package public, then reruns the failed anonymous verification job. If a
package already exists, its Actions access must allow this repository to
publish. Public verification downloads both platforms and their
attestations with an empty credential file. A stable release also sets
`:latest` after all four anonymous verifications pass. [GitHub package visibility and Actions access](https://docs.github.com/en/packages/learn-github-packages/configuring-a-packages-access-control-and-visibility)

| Package in `ghcr.io/publicdata-stream` | Compose variable |
| --- | --- |
| `docs-mcp-serve` | `DOCS_MCP_SERVE_IMAGE` |
| `docs-mcp-sync` | `DOCS_MCP_SYNC_IMAGE` |
| `docs-mcp-feed` | `DOCS_MCP_FEED_IMAGE` |
| `docs-mcp-postgres` | `DOCS_MCP_POSTGRES_IMAGE` |

The first planned stable release under these package names is `2.0.0`. Existing
release tags and the `gh-docs-mcp-*` package references remain part of their
original release lineage. Retain the previous digest-pinned references for
the [deployment cutover](deployment.md#upgrade-and-rollback).

Use all four digest-pinned references from the workflow summary or the
`ghcr-release-digests.json` artifact together. The receipt also records the
source revision, release version, and runnable manifest digest for each
platform. See the [Compose deployment procedure](deployment.md).

The PostgreSQL image derives from the official PostgreSQL 19.0 Alpine 3.24
image, updates `libxml2` from signed Alpine packages, and runs as UID/GID 70.
Include the upstream base layers and their notices in the release's license
and corresponding-source review. [Docker Official Image source](https://github.com/docker-library/postgres/tree/8672300fdf4f4ce8a8eac7511031fd08b6d2c886/19/alpine3.24) · [PostgreSQL license](https://www.postgresql.org/about/licence/) · [Alpine libxml2 source and license](https://pkgs.alpinelinux.org/package/v3.24/main/x86_64/libxml2)

## Recover a partial publication

If source verification or qualification requires a change to a published
immutable release's source, use a new eligible version for the corrected
commit and run preflight before publishing.

For releases and build tags, rerun failed jobs while the original qualified
bundle is still available.
This reuses the exact artifact ID. Each version tag is accepted only if
its digest already matches, and only missing version tags are written.
Treat the signed Git tag and each GHCR version tag as fixed. If the original
bundle cannot be recovered or a tag points to different bytes, use a new
eligible release version for a changed index. Keep package tag writes
within the publishing workflow.

GHCR updates the four `:latest` aliases individually. After a partial
promotion, rerun promotion for the highest successfully verified stable
release. The workflow fills or updates aliases still on older versions and
skips the entire promotion if any alias is already on a higher stable
version. A conflicting digest for the same version requires investigation.

## Test the workflow helpers

~~~sh
actionlint
python3 -B -m unittest discover -s scripts/phase8 -p test_release.py
DOCS_MCP_TEST_LOOPBACK=1 python3 -B -m unittest discover -s scripts/phase8 -p test_release.py
~~~

The loopback test requires Docker and Skopeo and uses synthetic OCI layouts.
Run the [native release gate](release-readiness.md#reproduce-the-local-gate)
for runtime and audit evidence.
