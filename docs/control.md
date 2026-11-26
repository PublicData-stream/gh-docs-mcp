# Durable scheduling and private control

Phase 6 adds automatic reconciliation and publication for an operator-managed
roster. Source approval still uses the separate migration role. The Git
object store and PostgreSQL are authoritative for publication and reads.

## Start and stop

Apply the catalog migration during a maintenance window with publication
writers stopped. Use the existing role UID, mount, and TLS configuration.
Set `GH_DOCS_CONTROL_ENABLED=1` to enable the control plane in Compose. With
control disabled the roles provide their previous services and manual sync
commands. Enabling control with an empty roster starts outbox delivery and
schedules no repositories.

Set `GH_DOCS_SYNC_ROSTER_FILE` to an absolute path in sync's read-only inputs.
The file is a JSON array of up to 128 canonical repository UUIDs. Register
each source using the existing approval workflow. Every currently approved
HTTPS repository on this roster can publish automatically. Remove a
repository and restart sync to stop scheduling it. The durable pause survives
a restart or removal and later re-enrollment.

Start the optional `feed` profile to reduce update latency. Set
`GH_DOCS_FEED_ROSTER_FILE` to a JSON array of the reviewed source approval
documents for the subset to observe. Mount the file read-only on feed.
Feed uses bounded Git Smart HTTPS ref discovery, preserves native SHA-1 or
SHA-256 identity, and sends only the observation digest and identifiers.
Configure `FEED_DNS` and feed-specific host filtering only for reviewed
sources. Sync independently fetches and verifies against the current
PostgreSQL approval. Changes to roster files take effect on role restart.

Install the host firewall before starting the roles. The existing
`scripts/phase2/egress.py install` command accepts `--control` for the two
private links and separate `--feed-resolver` and `--feed-https` allowlists.
Keep the DB listeners, sync port 8783, serve notification port 8784, and
loopback health listeners off published ports and outside the MCP proxy.
The qualified single-host profile uses private HTTP/1.1 with HMAC.
Use TLS for control links before any cross-host deployment. Keep Compose's
`init: true` for sync so its Git workers can distinguish their daemon parent
from an orphan reparented to PID 1.

After installing the firewall and provisioning each role's configuration
and keys, migrate and start the selected roles.

~~~sh
docker compose --profile feed stop feed sync
docker compose --profile operator run --rm --no-deps migrate --migrate
GH_DOCS_CONTROL_ENABLED=1 docker compose up -d serve sync
GH_DOCS_CONTROL_ENABLED=1 docker compose --profile feed up -d feed
~~~

Stop both feed and sync before a maintenance operation that requires
quiesced ingestion. Stop feed alone to keep periodic reconciliation and
outbox delivery active.

## Queue, ownership, and rollback

One database session owns the scheduler through advisory lock `-3`. If
ownership is lost, the daemon exits after closing its worker lifelines and
reaping the workers. A restart reacquires the lock and resumes durable work.
Manual and automatic writers share the existing repository fences and slot
limits. A repository can have one active job and one queued rerun. Repeated
observations share the queued work. Each repository reconciles on enrollment
and approximately daily, including when feed is stopped. Feed normally polls
every 15 minutes plus jitter. It retains its unacknowledged event across
network retries in memory. The sync database deduplicates accepted event IDs
for 24 hours and caps receipts at 1,024 per repository.

Use the following commands inside the sync service.

~~~sh
gh-docs-sync --schedule-status --repository "$repository_uuid"
gh-docs-sync --pause --repository "$repository_uuid"
gh-docs-sync --rollback --repository "$repository_uuid" --generation "$retained" --expect-sequence "$current"
gh-docs-sync --resume --repository "$repository_uuid" --expect-sequence "$after_rollback"
~~~

An operator pause prevents new claims and automatic promotion. A fetch that
already holds a writer fence may finish before the operator can acquire that
fence. A successful rollback pauses an enrolled repository in the same
commit as the pointer change. Resume uses the current sequence and starts
fresh reconciliation. An unchanged upstream snapshot that was rolled back
leaves the rollback active. A new logical snapshot may publish. If a writer
cannot obtain a fence, inspect status and retry after the current writer exits.
Manual reconciliation still ends at a private candidate.

Inspect the scheduled job state, attempt count, expected sequence, and
failure code before retrying. Transient failures can receive two scheduler
retries. Verify source approval and host routing for fetch failures. Check
the repository block and use the existing fenced recovery command for
interrupted canonical work. Capture an incident before modifying a corrupt
store. For an ambiguous publication result, inspect the event at the intended
sequence and preserve the committed pointer. The matching event can still
be inspected after a later publication. [Publication and recovery](publication.md)

## Keys and rotation

Provision two independent 48-byte raw random keys and a separate cursor key.
Give feed and sync copies of `feed-sync.key`. Give sync and serve copies of
`sync-serve.key`. Use read-only regular files with mode 0400 or 0440 and
role-specific ownership. Key IDs accept 1–32 ASCII letters, digits,
underscores, and hyphens. Keep private addresses, key bytes, and signed
requests out of logs and configuration history.

Generate each link's key under a private umask and install the role copies
with the appropriate owner and permissions.

~~~sh
umask 077
openssl rand -out "$private_dir/feed-sync.key" 48
openssl rand -out "$private_dir/sync-serve.key" 48
~~~

Rotate one link at a time. On its receiver, install the new active key and
ID. Set the link's `_PREVIOUS_KEY_ID`, `_PREVIOUS_KEY_FILE`, and
`_PREVIOUS_KEY_EXPIRES` values for the old key. The expiry is an absolute Unix
second at most 24 hours ahead. Recreate the receiver before changing the
sender. Override the sender's key ID for that service only when recreating the
receiver. Recreate the sender with the new key, verify delivery, and remove
the previous key from the receiver after the overlap. The receiver signs
the acknowledgment with the key that authenticated the request.

Synchronize the host clock. Check routing, clock offset, key ID, expiry, and
receiver health when delivery is pending. Delivery attempts and next due time
remain in PostgreSQL across restarts. Sync retries with the same event ID,
a fresh nonce, and fresh epoch discovery after failure. MCP reads continue
from PostgreSQL even while delivery is pending.

## Protocol v1

Requests use HTTP/1.1 POST, a literal path, `Content-Type: application/json`,
and a canonical decimal `Content-Length`. The key ID, caller, audience,
epoch, timestamp, nonce, and signature are carried in corresponding
`X-Git-MCP-*` headers. The caller and receiver role strings are
`gh-docs-feed`, `gh-docs-sync`, and `gh-docs-serve`. Nonces and epochs are
32 bytes encoded as unpadded base64url. Signatures use the full 48-byte
HMAC-SHA-384 tag in the same encoding. Each connection carries one message
and response with bounded headers, body, and deadlines.

| Route | Link | Body | Successful response |
| --- | --- | --- | --- |
| `/internal/v1/identify` | Either | Exact bytes `{}` and epoch `bootstrap` | Fresh receiver `epoch` |
| `/internal/v1/sync-requested` | Feed to sync | `event_id`, `repository_id`, `observation_digest` (64 lowercase hex), `observed_at` (Unix seconds), `reason` (`feed_refs`) | Same `event_id` and `accepted` outcome |
| `/internal/v1/generation-committed` | Sync to serve | `event_id`, `repository_id`, `generation_id`, `publication_sequence` (both canonical decimal strings) | Same `event_id` and `observed` or `superseded` outcome |

The exact request prefix is below. Terminate every line, including the empty
line, with LF. Append the original body bytes without a trailing separator.

~~~text
GITMCP-HMAC-SHA384-V1
{key_id}
{caller}
{audience}
{receiver_epoch}
{unix_seconds}
{nonce}
POST
{literal_path}
application/json
{body_length}

~~~

The response carries `X-Git-MCP-Request-Nonce`, `X-Git-MCP-Receiver` and
`X-Git-MCP-Response-Signature`. The response prefix uses the same line and
raw-byte rules.

~~~text
GITMCP-HMAC-SHA384-RESPONSE-V1
{key_id}
{receiver}
{caller}
{request_nonce}
{http_status}
application/json
{body_length}

~~~

Both independently calculated test vectors use key bytes 0 through 47,
ID `vector`, the feed-to-sync role pair, nonce
`AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8`, and body
`{ "probe": true }`. The request uses identify, epoch `bootstrap` and
timestamp `1700000000`. The response uses HTTP status 200.

| Message | Tag |
| --- | --- |
| Request | `TwqQA24aORdLimU3HxHLpRSEyeKlMW6e0K-vjdOq31e7rqzORkGci6wkHfDjfu0-` |
| Response | `QGtgEmSi2yEfhZGRZriNZSgTHfSXjo47FgkJElw__ytonIkcUUGwYzukaaVffroo` |

Receivers reject duplicate headers, ambiguous framing, wrong link identity,
and requests outside the 60-second clock window. Once verified, a nonce is
recorded atomically for at least 150 seconds. An exhausted replay cache
rejects requests while all 8,192 entries remain live. An unsigned error does
not acknowledge an event. Serve checks the exact event tuple and the active
publication sequence in one read-only lookup.

## Qualification

Use the pinned role and fixture images from the same revision. For the
Phase 6 Compose probe, build `phase6-serve`, `phase6-sync`, `phase6-feed`,
and `phase2-upstream` alongside the regular roles. The fixture builds
accept synthetic HTTPS endpoints and contain the interruption hooks. Use
the regular images for deployment.

~~~sh
export CARGO_TARGET_DIR=/tmp/rust/target
scripts/phase0/check-rust.sh
sudo -n python3 -B scripts/phase6/integration.py --image-tag "$tag"
python3 -B scripts/phase6/minikube.py \
  --serve-image "gh-docs-mcp-serve:$tag" \
  --sync-image "gh-docs-phase6-sync:$tag" \
  --feed-image "gh-docs-phase6-feed:$tag" \
  --upstream-image "gh-docs-phase2-upstream:$tag" \
  --postgres-image "gh-docs-mcp-postgres:$tag" \
  --bootstrap-cache "$bootstrap_cache"
~~~

Minikube uses the same local PostgreSQL image as Compose. The Minikube
smoke delays scheduled reconciliation so the first publication must follow
an authenticated feed hint. It exercises delivery and MCP reading on pod
loopback. Compose qualifies effective role networking and fault recovery.

[Deployment prerequisites](deployment-prerequisites.md) ·
[Observed qualification results](implementation-status.md)
