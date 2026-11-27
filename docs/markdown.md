# Markdown sources

The private MCP endpoint serves ordinary Git Markdown, Markdown built from
approved Git ref tips with the docs.rs builder, and Markdown from exact
docs.rs releases. Rustdoc output preserves the public item's Markdown
docstring. The result carries the item name, source path, source line, and
reviewed notice separately.

## Find, read, and search

Use `markdown.sources` to discover published sources. A Git file entry
identifies a repository. Pass a context returned by `git.resolve` to select
one of its commits. Rustdoc entries return a source selector
containing the immutable snapshot digest. Git Rustdoc selectors also carry a
Git context. Hosted selectors identify independent crate releases.

Pass up to eight selectors to `markdown.find` or `markdown.search`. All
Git selectors in the call must share one context. Each result's `document`
is an exact locator for `markdown.read`. When reading a range or following
a continuation, retain the original selector and arguments.

`source_path_glob` matches the original source path. Git builds report a
repository-relative `.rs` path. Hosted builds report a crate-relative
`.rs` path. `path_glob` matches the generated Markdown path, for example
`rustdoc/widget/api/fn.open.md`. Both filters must match when supplied.
Both match the original Git path for regular and executable `.md` files.
They use the [anchored Git glob rules](exploration.md#output-approval).
Caller filters narrow the standing grants.

The following source selector is returned by discovery. This query selects
function documentation sourced from `src/` and nested directories.

~~~json
{
  "sources": [
    {
      "kind": "docsrs_rustdoc",
      "source_id": "35cf5ef7-85b9-4ee0-b006-65ad6a3bc4fa",
      "snapshot_digest": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }
  ],
  "source_path_glob": {"encoding": "utf8", "value": "src/**/*.rs"},
  "path_glob": {"encoding": "utf8", "value": "rustdoc/**/fn.*.md"}
}
~~~

For `markdown.search`, also supply the `pattern` and optional existing
search flags `literal`, `ignore_case`, and `context_lines`. Pages use source
ID, source kind, document path, and match offset ordering. An empty page
can have a continuation. Read each page until the cursor is absent and
check the coverage field. `markdown.read` uses zero-based half-open byte
ranges, with `utf8` and `base64` encodings.

## Approve and schedule sources

Apply migration `0009` with publication writers stopped. Register approvals
using the existing migration credential and a read-only file mount. Set
`DOCS_MCP_CONTROL_ENABLED=1` and `DOCS_MCP_RUSTDOC_ENABLED=1` on Sync. The
default configuration schedules no Rustdoc jobs. When configured for live
acquisition, install the project firewall with Sync DNS and HTTPS access
for only the reviewed endpoints.

An approval records a canonical UUIDv4 `source_id`, a positive
`policy_version`, `reviewer`, `license_evidence`, `notice`, `input`,
optional `search` permission, and one to 64 paired grants. Each grant has a
`source_path_glob` and a `path_glob`. One pair must match both paths.
Review the provenance and redistribution terms for all future matching
output. Re-register the same input with a higher version to change the
grants or notice. Register a new source ID for a different input.

A hosted input uses these exact coordinates.

~~~json
{
  "kind": "docsrs_rustdoc",
  "package": "widget",
  "version": "1.2.3",
  "target": "x86_64-unknown-linux-gnu",
  "format": 61
}
~~~

Sync checks the hosted artifact daily using the approved coordinates. A
missing or invalid response leaves the last verified snapshot available.
Changes to approved Markdown get a new snapshot digest. A prior hosted selector can
continue for 24 hours after replacement while approval is current.

A Git input names the repository and ref to follow. Set `workspace` to a
repository-relative directory or omit it to select the root.

~~~json
{
  "kind": "git_rustdoc",
  "repository_id": "11111111-2222-4333-8444-000000000001",
  "reference": "refs/heads/main",
  "workspace": "rust",
  "package": "widget",
  "target": "x86_64-unknown-linux-gnu"
}
~~~

The Git ref must appear in a published generation under a current Git
approval that enables exploration. Each generated snapshot is bound to the
generation and exact ref-tip commit. Git Rustdoc approval covers the entire
exported workspace and the dependency-influenced output for future ref
updates. A source span identifies an item location. Rustdoc Markdown can also
come from another approved file, a macro, or generated data.

Commit `Cargo.lock`. Sync prefetches checksum-bound registry dependencies
from `index.crates.io` and `static.crates.io`. The selected package must
package offline with the approved dependency identities. Git and private
registry dependencies, symlinks, submodules, and source-controlled Cargo
configuration are unsupported. Use a workspace containing the selected
package and its publishable manifest. The local runner qualifies a single
`x86_64-unknown-linux-gnu` target. The converter requires format 61 and a
usable source span for every emitted page.

~~~sh
docker compose --profile operator run --rm --no-deps \
  -v "$approval_file:/approval.json:ro" migrate \
  --register-markdown-source --approval-file /approval.json
docker compose exec -T sync docs-mcp-sync --markdown-status --source "$source_id"
docker compose exec -T sync docs-mcp-sync --markdown-retry --source "$source_id"
docker compose --profile operator run --rm --no-deps migrate \
  --revoke-markdown-source --source "$source_id"
~~~

The job status provides durable failure codes and retry information.
Revocation and supersession deny new reads and continuations immediately.
The scheduler cancels stale jobs and Sync rejects their results at publication.
Keep the migrator and all roles on the same revision. To roll back the schema,
restore a verified schema 8 backup into fresh volumes and start the previous
binaries. Schema 9 backups include the Markdown snapshot and document digests.

## Provision the dedicated Git runner

Use a dedicated Linux runner with QEMU and a disposable qcow2 guest. The
attached Docker daemon lives inside the guest. Provision the guest with
Python, Cargo and rustup launchers, a rootless Docker daemon, and a
digest-pinned docs.rs build image. Install the adapter from the pinned
upstream Git checkout with `scripts/rustdoc/prepare-builder.sh`. Preserve the
upstream notices with the installed binary. Record the image digest in
`/etc/docs-mcp-rustdoc-image`. Under the dedicated guest account, run
`docs-mcp-build --prepare` while provisioning to seed the Rustwide workspace
at `/opt/rustwide` with the pinned nightly and target. The adapter uses a checksum-verified Rustwide
patch so job startup checks the preinstalled tools without updating them.
It vendors the approved dependency archives offline before the builder
mounts Cargo home read-only. Give that account
ownership of the Rustwide workspace. Seal the guest image after preparation.

Copy `scripts/rustdoc/guest.py` and `wire.py` to `/opt/docs-mcp-rustdoc`
inside the guest. Enable the supplied `docsrs-builder.service` as a user
service alongside the rootless Docker user service. Give this account access
to the char device named `docs-mcp.rustdoc` through a udev rule for
`SUBSYSTEM=="virtio-ports"` and `ATTR{name}=="docs-mcp.rustdoc"`. Enable
linger for the guest user to start both services at boot. Leave the guest
image without any service credentials or runner-control keys.

Mount `rustdoc-server.crt`, `rustdoc-server.key`, and the dedicated
`rustdoc-client-ca.crt` as Sync-only secrets. Set
`DOCS_MCP_RUSTDOC_RUNNER_BIND` to the private Sync control address on port
8785. Install the project firewall with `--control --rustdoc-runner` to
permit the Compose host to reach this one listener. For a separate runner
host, forward the private listener through a restricted SSH tunnel to the
Compose host. Keep certificate hostname verification enabled through the
tunnel. Give the runner supervisor the client certificate and its key plus the
CA that signs the Sync certificate.

~~~sh
python3 -B scripts/rustdoc/runner.py \
  --host 127.0.0.1 --port "$tunnel_port" --server-name "$runner_tls_name" \
  --ca "$server_ca" --cert "$client_cert" --key "$client_key" --image "$builder_image"
~~~

The supervisor boots a fresh guest snapshot for each lease and sends the
bounded bundle and receives JSON over a virtio serial channel. The guest has
no NIC or shared host filesystem. The supervisor owns the link credentials.
Only Sync can convert the returned artifact and commit a published snapshot.

## Qualification

Use the [contributor workflow](../CONTRIBUTING.md) and the documented
[Compose and Minikube prerequisites](deployment-prerequisites.md). Build
the fixture images with `scripts/phase8/fixtures.sh TAG --current-only`.
Run the synthetic Rustdoc probe with
`sudo -n python3 -B scripts/rustdoc/integration.py --image-tag TAG`.
Pass `--builder-image /tmp/.../builder.qcow2` to run the real pinned builder
on the synthetic registry dependency. A locally built synthetic JSON artifact
can exercise publication with `--builder-json`. Run the [Phase 5 smoke](mcp.md#qualification)
to qualify Markdown reads on the normal MCP deployment under Minikube.
