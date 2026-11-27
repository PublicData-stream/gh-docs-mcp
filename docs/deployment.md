# Private Compose deployment

Phase 8 targets rootful Docker Compose on local ext4 with effective nftables,
Landlock, and seccomp. The release overlay needs Compose support for
`!reset` and `!override`. The pinned OxiBelt data plane requires x86-64-v3 on
amd64. See the [release gate](release-readiness.md) for qualification status.

## Provision and start

1. Use four matching image digests from the release receipt. Set
   `DOCS_MCP_SERVE_IMAGE`, `DOCS_MCP_SYNC_IMAGE`, `DOCS_MCP_FEED_IMAGE`, and
   `DOCS_MCP_POSTGRES_IMAGE` to the full registry references with
   `@sha256:<index digest>`. Set
   `DOCS_MCP_AUTHORITY` to the TLS name and port used by clients.
   The overlay trusts only the dedicated OxiBelt ingress IP. Serve verifies
   the normalized hostname, effective TLS port and scheme from that peer
   before reconstructing the configured Host for MCP validation.
2. Supply the rootful Compose address and bridge variables and the absolute
   `FIXTURES` bind root through a private environment file. Despite its name,
   `FIXTURES` holds the deployment's state and role directories. Use dedicated,
   non-overlapping subnets. Keep the Compose port on host loopback. Install
   the qualified bridge and routed nftables rules before starting roles.
   The qualification profile uses synthetic endpoints. For live sources,
   provision egress for each approved Git host using the
   [source onboarding procedure](synchronization.md#source-approval).
3. Provision `store`, `staging`, and `pgdata` as distinct bind directories on
   the qualified filesystem. Run the networkless `volume-admin` once on fresh
   volumes. PostgreSQL starts as UID/GID 70 and requires the data and socket
   directories to be writable by that identity. Provision database SCRAM passwords, TLS materials with the
   `postgres.phase0.test` server SAN, and the role-scoped secrets described in
   [deployment prerequisites](deployment-prerequisites.md). Give the
   OxiBelt certificate and key to UID 10001 in the separate proxy directory.
   Mount each credential only into its intended role. For HMAC and cursor
   keys use distinct 48-byte random values and record their key IDs.
4. Render OxiBelt TOML with the real proxy and serve ingress addresses. Pass
   the certificate DNS name without the port. Place the output in
   `$FIXTURES/proxy/oxibelt.toml` and supply `server.crt` and `server.key` there.
   Verify the Compose configuration, the exact pinned proxy's `--check`,
   PostgreSQL health and the schema migration before starting app roles.
   Keep proxy ingress closed until the private checks succeed.
5. Start the proxy on loopback. Verify the certificate, pinned MCP discovery
   and a known synthetic read. Check that the listener exposes only `/mcp`,
   and that wrong authority or Origin, direct role access, and serve egress
   are denied. Enable the optional feed only after both HMAC receivers and
   the scheduler are healthy.

```sh
python3 -B scripts/phase8/edge.py --host "$tls_name" \
  --proxy-ip "$PROXY_IP" --serve-ip "$SERVE_INGRESS_IP" > "$FIXTURES/proxy/oxibelt.toml"

docker --context default compose --env-file "$private_env" -p "$project" \
  -f compose.yml -f deploy/compose.release.yml config --quiet
```

Use the same project arguments for all Compose operations. With firewall
rules installed, run `run --rm --no-deps volume-admin`, then
`up -d --no-build --wait postgres`, and then `run --rm --no-deps migrate`.
Start `serve` and `sync` with `up -d --no-build`. Add `proxy` after checking
the application and storage. For a restore, start PostgreSQL and run
the restore before any migration or application start.

## Readiness and operations

- Check app `/healthz` inside each container. The proxy keeps `/live`,
  `/ready`, and metrics on its private loopback sockets. Use verified TLS and
  an MCP smoke call through the edge to check end-to-end readiness. A
  catalog publication block keeps that repository unavailable.
- Monitor project-scoped Docker logs, proxy latency/errors, bounded
  admissions, filesystem free bytes/inodes, job backlog and oldest pending
  outbox age. Query control state through the private PostgreSQL socket and
  existing sync status commands. Alert below twice the 128 MiB canonical
  admission floor and investigate sustained queue growth or notification lag.
  Keep source bytes, arguments, private paths and auth material out of logs
  and metrics.
- Keep nightly coordinated backups and a backup before an upgrade, policy
  change or maintenance window. Keep an encrypted independent copy with the
  verified digest in separate protected storage. Rehearse recovery at least
  quarterly and after changing the DB/image or storage profile. Set service
  recovery objectives after measuring the real workload.
- For takedown, revoke the source with the migrator and verify new reads,
  retained contexts and cursors fail. Preserve incident evidence privately.
  Review retention and backup deletion separately from the online revocation.
- For a suspected compromise or canonical damage, close ingress, stop new
  feed/job activity, preserve the store and logs, rotate affected credentials,
  and use the [recovery procedure](publication.md#backup-and-restore).
  Review new advisories against the deployed digests and rerun impacted
  native gates before adopting a new pin.

## Upgrade and rollback

For planned serve restarts, close the proxy first and reopen it after serve
is ready. This clears upstream connections held by the old proxy process.

Drain writers and take a verified matching backup. Keep the old role and
infrastructure digests, proxy configuration, and backup digest together. Bring up the new
database/image combination with ingress closed. Run the migration as the
migrator, verify schema, grants, filesystem and pinned MCP behavior, and
start the proxy only after readiness. An image-only rollback is available
when verified against the current catalog and store format. For an
incompatible schema or failed store migration, provision fresh volumes and
restore the matched pre-upgrade backup, previous images and proxy configuration. Reconcile
revocations and credential changes made after the backup before reopening.

### Move from gh-docs-mcp to docs-mcp

The `2.0.0` cutover reuses the existing Compose project and data. Keep the
absolute `FIXTURES` path, the `store`, `staging`, and `pgdata` bind mounts,
the `gh_docs` database and its roles, and the network configuration and
firewall. Keep the TLS hostname and role secrets including cursor and
control keys. Save the previous Compose files, private environment, and
four image digests for rollback.

1. Stage a new private environment file with the four qualified
   `DOCS_MCP_*_IMAGE` values. Replace each configured `GH_DOCS_*` setting
   with its `DOCS_MCP_*` counterpart. Use `DOCS_MCP_AUTHORITY` for the
   old `GH_DOCS_MCP_AUTHORITY`. Update automation to call
   `docs-mcp-serve`, `docs-mcp-sync`, or `docs-mcp-feed`, and update any
   client filters that expect the old MCP server name. Render the new
   Compose configuration against the existing project and check the
   images, bind mounts, and configured control mode. Pull the four pinned
   images before closing ingress.
2. Stop the proxy and feed, settle active Sync and publication work, and stop
   Sync and Serve. With PostgreSQL still running, capture and verify the
   [coordinated backup](publication.md#backup-and-restore).
3. Stop PostgreSQL. Using the new files and environment with the same
   `-p "$project"`, bring up PostgreSQL with
   `up -d --no-build --wait postgres` and run the migrator. Start Serve
   and Sync, and start Feed if it was enabled. Check health, grants, and the
   active generation and sequence. Check control delivery when enabled.
   Start the proxy on loopback and verify TLS, MCP discovery, and a known
   read before restoring client access.
4. To roll back on the existing state, close ingress, quiesce the roles,
   then stop PostgreSQL and repeat the startup checks with the saved old
   Compose files, environment, and image digests. Use the verified backup
   and the [restore procedure](publication.md#backup-and-restore) if the
   existing state cannot be reopened.

The [cutover rehearsal](release-readiness.md#rehearse-the-rename) exercises
both directions on a populated synthetic deployment.
