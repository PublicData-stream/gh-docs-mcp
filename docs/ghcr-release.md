# OCI release artifacts

The [release qualification workflow](../.github/workflows/release-candidate.yml)
runs read-only native amd64 and arm64 jobs. Both jobs must pass before they
produce a combined bundle. Every role has its own OCI index, with two
runnable platform manifests and their attached SPDX SBOM and SLSA provenance.
The build uses pinned BuildKit, locked source inputs, and the source commit
time. The repeat check compares runnable digests from independent builders.
Attestation generation time is outside that equality check.

The combined artifact includes `release.json` and `SHA256SUMS`. Keep both
native evidence artifacts with it. They contain the tested platform digests,
host and test evidence, Cargo inventories, image audits, and the exact Git
source archive needed for review of distribution obligations. Verify the
workflow revision and all checksums before trusting the bundle. Review the
project's license, upstream notices and corresponding-source obligations
before distribution.

## Authorized promotion

After operator approval, use the chosen signed source tag and approved
version with all three indexes from the same receipt. Check each version tag
in GHCR. An existing tag must match the receipt's digest. Use a new version
for a changed index. Transfer a missing role with digest preservation, for
example:

```sh
skopeo copy --all --preserve-digests "oci:$bundle/$role:release" \
  "docker://ghcr.io/publicdata-stream/gh-docs-mcp-$role:$version"
```

Inspect every registry index and compare its digest and platform mapping
with the receipt. After a partial transfer, repeat only the missing roles
after verifying existing ones. Use the three verified index digests together
in the [Compose deployment](deployment.md). The release-candidate workflow
exports artifacts and has read-only repository permissions. GHCR promotion,
tag pushes, and endpoint exposure each require separate approval.
