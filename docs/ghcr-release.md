# OCI release artifacts and GHCR publication

The [release-candidate workflow](../.github/workflows/release-candidate.yml)
can be run manually to produce a read-only qualification bundle. The
[publishing workflow](../.github/workflows/publish-images.yml) calls the
same native amd64 and arm64 jobs at the signed release commit. Both jobs
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

The native image build applies the full release tag in a disposable archive
of the verified commit. It changes the root Cargo workspace version and
the production package entries in the copy of `Cargo.lock`. The MCP server,
OCI labels, and build receipt use the same version. Audit evidence records
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
   `python3 -B scripts/phase8/release_source.py preflight 1.0.2`. It checks
   the exact signed tag, canonical `main` CI, and version stamping.
4. Publish a GitHub Release for that existing tag. Mark beta releases as
   prereleases. Publishing the release authorizes the workflow to write the
   versioned indexes after qualification. A tag push or a draft release
   alone does not start publication.

The workflow links the three packages to this repository through their OCI
source label. New packages start private. A package admin makes each
package public, then reruns the failed anonymous verification job. If a
package already exists, its Actions access must allow this repository to
publish. Public verification downloads both platforms and their
attestations with an empty credential file. A stable release also sets
`:latest` after all three anonymous downloads pass. [GitHub package visibility and Actions access](https://docs.github.com/en/packages/learn-github-packages/configuring-a-packages-access-control-and-visibility)

| Package in `ghcr.io/publicdata-stream` | Compose variable |
| --- | --- |
| `gh-docs-mcp-serve` | `GH_DOCS_SERVE_IMAGE` |
| `gh-docs-mcp-sync` | `GH_DOCS_SYNC_IMAGE` |
| `gh-docs-mcp-feed` | `GH_DOCS_FEED_IMAGE` |

Use all three digest-pinned references from the workflow summary or the
`ghcr-release-digests.json` artifact together. The receipt also records the
source revision, release version, and runnable manifest digest for each
platform. See the [Compose deployment procedure](deployment.md).

## Recover a partial publication

If source verification failed for a published immutable release, use a new
eligible version for the corrected commit and run preflight before publishing.

Rerun failed jobs while the original qualified bundle is still available.
This reuses the exact artifact ID. Each version tag is accepted only if
its digest already matches, and only missing version tags are written.
Treat the signed Git tag and each GHCR version tag as fixed. If the original
bundle cannot be recovered or a tag points to different bytes, use a new
eligible release version for a changed index. Keep package tag writes
within the publishing workflow.

GHCR updates the three `:latest` aliases individually. After a partial
promotion, rerun promotion for the highest successfully verified stable
release. The workflow fills or updates aliases still on older versions and
skips the entire promotion if any alias is already on a higher stable
version. A conflicting digest for the same version requires investigation.

## Test the workflow helpers

~~~sh
actionlint
python3 -B -m unittest discover -s scripts/phase8 -p test_release.py
GH_DOCS_TEST_LOOPBACK=1 python3 -B -m unittest discover -s scripts/phase8 -p test_release.py
~~~

The loopback test requires Docker and Skopeo and uses synthetic OCI layouts.
Run the [native release gate](release-readiness.md#reproduce-the-local-gate)
for runtime and audit evidence.
