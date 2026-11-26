# Private Git exploration

The Phase 4 Rust service in `gh_docs_search::exploration` provides a typed
request interface over approved SHA-1 and SHA-256 generations. Compose
qualifies it through the feature-gated `gh-docs-phase4-fixture` JSON driver
with serve credentials and the read-only published store. The normal HTTP
endpoint uses the same engine. See [MCP search and history](mcp.md).

## Output approval

Add an `exploration` member to the source approval document. Review the
source identity, ref names and targets, commit identities, and permitted
output over the complete approved history. Each notice is separate from the
source bytes and can contain up to 4 KiB of UTF-8 text.

~~~json
{
  "version": 1,
  "metadata_notice": "Reviewed attribution for this source and its refs",
  "scope": {
    "kind": "paths",
    "grants": [
      {
        "path": {"encoding": "utf8", "value": "docs"},
        "subtree": true,
        "notice": "Reviewed documentation notice"
      },
      {
        "path": {"encoding": "utf8", "value": "LICENSE"},
        "subtree": false,
        "notice": "Reviewed license file notice"
      }
    ]
  }
}
~~~

Use `{"kind":"whole","notice":"…"}` after reviewing the whole closure.
Path scope accepts up to 64 disjoint grants. A subtree grant requires a tree
at its anchor. An exact grant permits a regular or executable file, symlink,
or gitlink at that path. An ancestor containing an approved descendant in
the selected commit carries its directory name and metadata notice. Its
tree OID appears only under a whole-closure or applicable subtree grant.
Sources with acquisition-only approvals become discoverable after a
reviewed grant is registered and a matching generation is published.

Policy supersession and revocation invalidate access to previous policy
versions, including retained generations. A read admitted under the shared
fences can finish while its database session remains healthy. A database
disconnect cancels the read and the filesystem guards survive worker reap.

## Requests and results

The fixture driver reads a JSON request from stdin and produces a single
`{"result":…}` or `{"error":{"code":…,"guidance":…}}` envelope. For
qualification it also accepts an array of up to 16 requests using the same
service instance. `--schema` exports the [request schema](../scripts/phase4/request.schema.json).
Request validation also enforces canonical UUIDs, decimal generation IDs,
native OIDs, and raw byte encodings.

| Tool | Arguments and result |
| --- | --- |
| `repository.search` | Optional literal `query` and `page`. Lists active approved metadata in UUID order. Subsequent pages reflect current catalog state. |
| `repository.info` | Repository UUID. Returns reviewed source identity and notice, active generation, policy version, publication sequence, observation timestamp, and approved ref snapshot count. |
| `git.refs` | Repository UUID, optional `generation_id`, and `page`. Lists exact snapshot refs in raw byte order with native targets and peeled commits. |
| `git.resolve` | Repository UUID, optional `generation_id`, and `selector`. Use `{"kind":"default"}`, `{"kind":"ref","value":…}`, or `{"kind":"oid","value":"…"}`. Returns an authorized commit context and source metadata. |
| `git.ls_tree` | Returned `context`, optional `root` and `depth`, and `page`. Lists descendants with path, mode, type, and native OID where approved. Default depth is 1. |
| `git.find` | Same tree scope with `pattern` tagged `literal` or `glob`. Default depth is 64. |
| `git.read` | Returned `context`, exact `path`, optional `range`, `encoding`, and `cursor`. Returns source identity, path, mode, native OID, and notice, with the blob bytes and offsets when applicable. |

Pass the returned `context` unchanged into a subsequent request. This
example reads the binary file in the initial SHA-1 Compose fixture.

~~~json
{
  "tool": "git.read",
  "arguments": {
    "context": {
      "repository_id": "11111111-2222-4333-8444-000000000001",
      "generation_id": "0",
      "object_format": "sha1",
      "commit_oid": "c0a821121825bf4d7262cf5dbb14182a08f716be"
    },
    "path": {"encoding": "utf8", "value": "data/binary"},
    "range": {"start": 0, "end": 4},
    "encoding": "base64"
  }
}
~~~

The result includes the same context, source metadata, and these byte
coordinates.

~~~json
{
  "oid": "b9151552b879da3dbcc52667c3fef985b665a12b",
  "data": "DQoA/w==",
  "encoding": "base64",
  "total_bytes": 4,
  "requested_range": {"start": 0, "end": 4},
  "returned_range": {"start": 0, "end": 4},
  "notice": "Synthetic reviewed path bytes",
  "next_cursor": null
}
~~~

Gitlink responses carry their mode and commit OID as metadata. Symlink
responses carry their stored target bytes. Omitted tree root means the
commit root, and an approved empty tree returns an empty page.

Paths, ref names, and patterns use tagged `utf8` or padded canonical
`base64` when the underlying bytes cannot be represented in UTF-8. File
paths are literal tree coordinates. A literal find matches a case-sensitive
substring of the whole repository-relative byte path. Globs match the
whole path. `*` matches any bytes within a component, `?` matches one
byte, and a component of `**` matches zero or more components. Other
bytes match literally. There are no ignore rules or shell expansions.

Blob offsets are zero-based and half-open. Omitted range selects the whole
blob. The default `utf8` encoding requires the requested slice to be valid
UTF-8. A split code point returns `invalid_utf8_boundary`, and other
invalid bytes return `unsupported_encoding`. Use `base64` for arbitrary
bytes or offsets. Retain the original range and encoding on continuation.
Each page returns up to 32 KiB of original bytes. Empty blobs and empty
ranges are valid.

## Pagination and limits

Lists use `page.limit` of 20 by default and accept 1–100. Supply
`next_cursor` with the original arguments using `page.cursor`, or
`cursor` for a blob. The 30-minute limit applies to the whole sequence.
Ref, tree, and blob pages keep their initial generation and publication
sequence across a promotion or rollback. Every continuation rechecks
visibility and approval. A stale generation returns
`generation_unavailable` with guidance to resolve again.

Treat each cursor as opaque. The token encodes a versioned JSON payload
and its full HMAC-SHA-384 tag in unpadded base64url. The payload binds the
tool, key ID, keyed digest of normalized arguments, applicable generation
scope, next position, and expiry. Use separate MAC domains for the argument
digest and token. The resume position is a repository UUID, an approved
entry ordinal, or a blob byte offset. The fixture takes a
48-byte file through `EXPLORATION_CURSOR_KEY_FILE` and a key ID through
`EXPLORATION_CURSOR_KEY_ID`. Key rotation invalidates tokens issued with
the prior key.

Coverage describes the approved results in each page. A partial page
with `next_page` can continue. `narrow_scope` means the remaining scope
could not be covered within the request budget. This can occur on an empty
page. Tree continuations replay the ordered traversal to the authorized
resume position and charge that work to the new request.

Each service instance admits four requests globally and two fenced
requests per repository. Slots remain assigned through worker reap. The
request deadline is 15 seconds. The reader shares limits of 50,000
object and tree-entry visits, 128 MiB of worker output, 16 MiB of
tree-entry materialization, and 8 MiB per blob. The tree depth limit is
64 and the full MCP response budget is 256 KiB. These are initial
service limits. Host capacity and maximum safe concurrency require
separate qualification.

## Qualification

Use the [publication qualification](publication.md#qualification) to
prepare the pinned role images and Phase 0–3 fixtures. Build and run
the additional Phase 4 fixture with the same tag.

~~~sh
docker --context default buildx build --network host --platform linux/amd64 \
  --load --target phase4-serve -t "gh-docs-phase4-serve:$tag" .
sudo -n python3 -B scripts/phase4/integration.py --image-tag "$tag"
python3 -B scripts/phase4/minikube.py \
  --serve-image "gh-docs-phase4-serve:$tag" \
  --sync-image "gh-docs-phase3-sync:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-mcp-postgres:$tag" \
  --bootstrap-cache "$bootstrap_cache"
~~~

Use the same local PostgreSQL image for Minikube as in Phase 3. Use the
rootless Docker profile with its disposable state and kubeconfig under
`/tmp`. Compose exercises SQL grants, policy changes, object oracles,
and retention. The Minikube Job checks both Git formats with the same
read-only role image.
