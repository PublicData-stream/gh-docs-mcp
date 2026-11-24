# AGENTS.md

## Project Overview

`gh-docs-mcp` is a Rust workspace for private, read-only Git repository
exploration over MCP. Phase 0 serves a synthetic fixture and qualifies the
deployment with Docker Compose. Git data belongs in native bare object stores.
PostgreSQL is the control plane. Keep the MCP revision pinned and scope
credentials and filesystem access to each role.

## Repository Structure

- `apps/` owns the `serve`, `sync`, and `feed` entry points and the MCP endpoint.
- `crates/` contains shared configuration, Git and search worker boundaries,
  PostgreSQL access, synchronization, and test support.
- `migrations/` contains PostgreSQL schema and privilege definitions.
- `Dockerfile` and `compose.yml` define the role images and local deployment.
- `scripts/phase0/` contains the offline Rust gate, synthetic fixtures, and
  Compose qualification probes. `.github/workflows/` contains hosted CI.
- `docs/` contains architecture decisions, deployment prerequisites, and
  observed qualification results.

## Contributor Guidance

Read `CONTRIBUTING.md` before editing. It is the source of truth for workflow,
security, testing, documentation, and commit requirements.

- [Contribution Workflow](CONTRIBUTING.md#contribution-workflow)
- [Commit Messages](CONTRIBUTING.md#commit-messages)
- [Security Requirements](CONTRIBUTING.md#security-requirements)
- [Pull Request Checklist](CONTRIBUTING.md#pull-request-checklist)

Run the offline checks and applicable phase probes before marking a gate
passed. Use `/tmp` for disposable test data and `/tmp/rust/target` for Cargo
artifacts. Record observed results and blockers in the
[implementation status](docs/implementation-status.md).

## Agent Commit Message Guidance

Write commit messages that readers can understand from the repository and
stable sources. Describe the behavior and the checks that ran using
repository-relative paths. Keep private details, credentials, session-specific
commands, and local artifact paths out of commit messages. Use the signed
commit and trailer requirements in [Commit Messages](CONTRIBUTING.md#commit-messages).
