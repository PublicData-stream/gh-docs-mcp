# Repository guidance

Read [CONTRIBUTING.md](CONTRIBUTING.md) before editing. Keep Git data in native bare object stores and PostgreSQL as the control plane. Keep MCP read-only, pin the declared protocol revision, and qualify deployment with Docker Compose. Keep credentials and filesystem access scoped to each role.

Run offline checks and the applicable phase probes before marking a gate passed. Use `/tmp` for disposable test data and `/tmp/rust/target` for Cargo artifacts. Record observed results and blockers in [implementation status](docs/implementation-status.md).
