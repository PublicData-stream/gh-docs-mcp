# docs-mcp

docs-mcp provides a private, read-only Model Context Protocol endpoint for
documentation and other source material stored in Git. The native Git store
and confined reader support SHA-1 and SHA-256. Sync verifies private
candidates and supports atomic publication, rollback, and guarded
maintenance. MCP tools discover approved repositories, read pinned snapshots,
search source bytes, and inspect approved history. Markdown tools also serve
Rustdoc prose from approved Git builds and exact docs.rs releases.
[Markdown sources](docs/markdown.md)

Operators can onboard repositories from any host that satisfies the service's
HTTPS Git requirements. [Source onboarding](docs/synchronization.md#source-approval)

See the [storage and reader contract](docs/storage.md), [synchronization runbook](docs/synchronization.md), [publication and recovery](docs/publication.md), [scheduler and control runbook](docs/control.md), [private exploration contract](docs/exploration.md), [MCP search and history](docs/mcp.md), [deployment prerequisites](docs/deployment-prerequisites.md), [security and capacity qualification](docs/phase7-qualification.md), [architecture decisions](docs/decisions), and [observed results](docs/implementation-status.md).

For the private Compose profile, see [deployment](docs/deployment.md),
[release qualification](docs/release-readiness.md), and
[OCI artifacts](docs/ghcr-release.md).
