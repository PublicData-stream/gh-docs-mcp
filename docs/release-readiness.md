# Phase 8 release readiness

The intended profile is a private synthetic release candidate on native
Linux amd64 and arm64, using rootful Docker Compose with OxiBelt TLS.
The current go/no-go record is in [implementation status](implementation-status.md).
Minikube is a supplemental smoke profile. A hosted job or an emulated build
alone does not qualify a deployment.

## Reproduce the local gate

Use the [prerequisites](deployment-prerequisites.md) and install
`python3-jsonschema`, `skopeo`, and `cargo-audit` 0.22.2. Use native
architecture and retain logs in a fresh directory under `/tmp`.

```sh
set -o pipefail
export CARGO_TARGET_DIR=/tmp/rust/target TMPDIR=/tmp
evidence=$(mktemp -d /tmp/gh-docs-phase8.XXXXXXXX)
client=$(scripts/phase0/prepare-client.sh)
cargo fetch --locked
scripts/phase0/check-rust.sh
python3 -B scripts/phase8/images.py build --repeat "$evidence/oci"
scripts/phase8/fixtures.sh phase8-fixtures phase8-previous
while read -r image; do docker --context default pull "$image"; done \
  < <(python3 -B scripts/phase8/pins.py)
scripts/phase8/qualify.sh phase8-fixtures "$evidence" "$(command -v node)" "$client"
sudo -n python3 -B scripts/phase8/integration.py --bundle "$evidence/oci" \
  --fixture-tag phase8-fixtures --runtime-tag phase8-candidate --previous-tag phase8-previous \
  --node "$(command -v node)" --client-dir "$client" --evidence "$evidence/restore.json"
python3 -B scripts/phase8/audit.py "$evidence/oci" "$evidence/audit" --tag phase8-audit
```

On hosts with constrained registry access, seed the locked crates from a
private directory under `/tmp` containing Cargo's `index` and `cache`.
Pass that directory as `--seed` to `images.py` and as
`GH_DOCS_RELEASE_SEED` to `fixtures.sh`. Cargo still validates the lockfiles.

For Minikube, tag the pinned PostgreSQL image as a disposable local name and
run `scripts/phase7/minikube.py` with the prepared serve, Phase 6 sync/feed,
Phase 2 upstream, and PostgreSQL images. The harness gives its rootless
cluster a private kubeconfig and home under `/tmp` and deletes the profile
on exit. Follow the [Phase 7 command](phase7-qualification.md#local-procedure).
Remove only the temporary image tags and SDK fixture created for the run.
Keep the sanitized evidence.

## Gate and operator review

Release requires matched digests and passed native tests for both platforms,
a verified independent restore, a reviewed dependency and license
inventory, and disposition of critical and high audit findings. Assess
infra findings against the exact image and effective runtime profile.
A changed pin needs a new build and relevant probes. Record reports from
the pinned SDK, wire probe, and any other accessible clients individually.

Public release also requires reviewed privileges and host policy, approval
of the intended source scopes and notices, limits, retention and exposure,
and separate review of Terms, Privacy, and logging. Live smoke needs bounded
scope and separate approval. The release readiness record should include the
actual revision, test host and measured budgets, audit database versions,
and unresolved blockers. [Deployment procedures](deployment.md) ·
[OCI workflow and promotion](ghcr-release.md)
