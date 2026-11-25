# Private MCP tools

The normal serve entry point exposes stateless JSON over Streamable HTTP at
`/mcp` on revision `2026-07-28`. A client can discover, list, or directly
call tools using the pinned per-request metadata and headers. The TLS proxy
preserves the checked Host and Origin and strips forwarding identity headers.
The qualification proxy binds to loopback.

The seven [exploration tools](exploration.md) retain their argument shapes.
Reuse the exact four-field context from `git.resolve`. The
[runtime schema snapshot](../apps/schemas/tools.json) also includes the five
tools below. `gh-docs-serve --tool-schemas` exports the registry without
connecting to PostgreSQL.

Successful calls and tool errors carry structured JSON and equivalent JSON
text. Tool errors set `isError` and carry a stable `code` and optional
`guidance`. JSON-RPC and HTTP errors handle protocol and framing failures.
Hidden and missing objects both return `object_unavailable`. A denied output
form returns `policy_denied` for an otherwise readable generation.

## Approve output forms

Version 1 exploration approvals enable catalog, tree, and blob tools. Use a
new reviewed source policy version with a version 2 exploration approval to
enable search or history. Each permission defaults to false. Publish a
generation matching the reviewed version before using it.

~~~json
{
  "version": 2,
  "metadata_notice": "Reviewed source and history attribution",
  "scope": {"kind": "whole", "notice": "Reviewed source notice"},
  "outputs": {"search": true, "history": true}
}
~~~

For path grants, review all historical bytes, derived results, and commit
identity metadata in scope. History includes original author and committer
names, email addresses, signed Unix seconds as decimal strings, time zones,
native commit IDs, and native parent IDs. The service omits commit messages.
Source notices are separate from original bytes. Supersession, revocation and
retirement take effect on the next admission, including cursor use.

## Search

`git.rg` takes a `context`, UTF-8 `pattern`, and optional `literal`,
`ignore_case`, `root`, `path_glob`, `context_lines`, and `page`.
The default is a case-sensitive, single-line regex. Globs use the raw
repository-relative path semantics of `git.find`. Context defaults to
zero and allows up to three surrounding lines on each side. The worker uses
the pinned ripgrep regex engine with LF delimiters, no PCRE2, no multiline
mode, and no repository ignore or configuration rules.

Search visits regular and executable files and excludes symlinks and gitlinks.
A NUL anywhere in a blob classifies it as binary and reports it as a skip.
Non-UTF-8 names and excerpts use reversible tagged bytes. Lines are one-based.
Match, line, context, and returned excerpt byte ranges are zero-based and
half-open over the original blob. CR is retained. A bounded excerpt can be
shorter than its context or match range. Use `git.read` for an exact byte
slice. Hits sort by raw path and then byte offset. Two paths sharing a blob
produce separate hits.

## History

| Tool | Scope and semantics |
| --- | --- |
| `git.log` | A `context`, optional literal `path`, and `page`. Visit all parents in increasing graph distance, breaking ties by OID. A path filter or path-scoped approval compares the approved projection to the first parent. Root comparisons use the empty tree. |
| `git.show` | A `context`, optional one-based `parent`, literal `path`, `context_lines`, and `page`. Return reviewed metadata and the diff against parent one by default. A root compares to an empty tree. |
| `git.diff` | `base` and `head` contexts from one repository and generation, plus the optional show patch arguments `path`, `context_lines`, and `page`. Both commits must be authorized. |
| `git.blame` | A `context`, literal `path`, optional one-based half-open `line_range`, and `page`. Trace unchanged lines along first parents at that path. A missing path, type change, or binary predecessor is an attribution boundary. Report unresolved origins when the traversal budget runs out. |

Diffs are path-sorted, byte-preserving line comparisons with zero to three
context lines, defaulting to three. Treat renames as deletion and addition.
Regular text hunks include previous and resulting line and byte ranges,
retained line endings, and explicit excerpt truncation. Binary data and
special modes return metadata. A hunk or file that exceeds the comparison
budget carries an omission indicator. No Git diff drivers or text converters
are invoked.

## Limits, keys, and continuation

Set `EXPLORATION_CURSOR_KEY_FILE` to a serve-only file with exactly 48 bytes
and `EXPLORATION_CURSOR_KEY_ID` to its identifier. A restarted instance can
read outstanding cursors when the key and identifier stay the same. Rotation
invalidates the previous tokens. The 30-minute lifetime starts on the first
page. Resume with the same arguments and page limit. The server rechecks
approval and visibility on every request.

Initial limits are four concurrent tool operations and two per repository.
The HTTP listener admits up to 16 in-flight requests and the tool limiter
allows 64 calls per second with a burst of 64. A tool request has 15 seconds,
50,000 Git visits, up to 128 MiB of Git worker output, and up to 8 MiB per
blob. Search admits up to 32 candidate blobs per page and its confined
worker has two CPU seconds and a 64 MiB address-space limit. History caps
ancestry at 2,048 commits per request and bounds its comparison work and
memory independently. The complete JSON-RPC response stays within 256 KiB.

Coverage counters describe the current page. A partial page can have zero
hits. `next_cursor` provides a continuation when the scan can advance.
`narrow_scope` indicates an omission or a budget boundary that prevents full
coverage. These limits are starting settings. Measure host capacity in
Phase 7.

## Qualification

Build the pinned images following the [exploration qualification](exploration.md#qualification).
The Phase 5 Compose probe requires Python `jsonschema` in addition to the
[Phase 0 prerequisites](deployment-prerequisites.md). Run it against the
normal serve image. The supplemental Minikube smoke uses the Phase 4 fixture
target for publication diagnostics and runs its inherited serve binary over
pod loopback.

~~~sh
export CARGO_TARGET_DIR=/tmp/rust/target
scripts/phase0/check-rust.sh
client_dir=$(scripts/phase0/prepare-client.sh)
sudo -n python3 -B scripts/phase5/integration.py --image-tag "$tag" \
  --node "$(command -v node)" --client-dir "$client_dir"

python3 -B scripts/phase5/minikube.py \
  --serve-image "gh-docs-phase4-serve:$tag" \
  --sync-image "gh-docs-phase3-sync:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-phase5-postgres:$tag" \
  --bootstrap-cache "$bootstrap_cache"
~~~
