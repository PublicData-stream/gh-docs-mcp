# gh-docs-mcp

gh-docs-mcp provides a private, read-only Model Context Protocol endpoint. The native Git store and confined reader support SHA-1 and SHA-256. Sync verifies private candidates from operator-approved sources and supports atomic publication, rollback, and guarded maintenance. MCP tools discover approved repositories, read pinned snapshots, search source bytes, and inspect approved history.

See the [storage and reader contract](docs/storage.md), [synchronization runbook](docs/synchronization.md), [publication and recovery](docs/publication.md), [scheduler and control runbook](docs/control.md), [private exploration contract](docs/exploration.md), [MCP search and history](docs/mcp.md), [qualification procedure](docs/deployment-prerequisites.md), [architecture decisions](docs/decisions), and [observed results](docs/implementation-status.md).
