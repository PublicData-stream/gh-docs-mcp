# Contributing to docs-mcp

`docs-mcp` is a private, read-only MCP service for approved Git repository
exploration and Rustdoc Markdown. Use synthetic fixtures for qualification and
repository-relative paths in documentation, issues, and pull request notes.
Run commands from the repository root unless a procedure specifies another
directory.

## Repository Layout

| Path | Purpose | Change here when |
| --- | --- | --- |
| `Cargo.toml` and `rust-toolchain.toml` | Workspace metadata, shared dependencies, and pinned Rust tools. | Workspace membership, shared dependencies, or the toolchain changes. |
| `apps/` | Role entry points and MCP transport and tool handling. | Startup, role behavior, or the MCP contract changes. |
| `crates/core/`, `crates/git/`, and `crates/search/` | Shared configuration and the Git and search worker boundaries. | Common configuration, Git object access, worker confinement, or search behavior changes. |
| `crates/storage/`, `crates/sync/`, and `migrations/` | PostgreSQL access, synchronization, schema, and grants. | Database access, storage coordination, sync behavior, or role privileges change. |
| `crates/test-support/` and `scripts/` | Synthetic test support, offline checks, and qualification probes. | A check or fixture needs to cover new behavior. |
| `Dockerfile` and `compose.yml` | Role images and the Compose deployment. | Image contents, service wiring, secrets, networks, or mounts change. |
| `docs/` and `.github/workflows/` | Architecture decisions, qualification procedures and evidence, and hosted CI. | A design decision, operator procedure, observed gate result, or CI job changes. |

## Contribution Workflow

1. Identify the affected role, interface, or trust boundary before editing.
2. Make the smallest change that covers the intended behavior. Review protocol,
   configuration, and stored-data compatibility where they are affected.
3. Add or update focused regression coverage for behavior changes. Update the
   relevant document under `docs/` when a technical contract or procedure
   changes. Use `README.md` for project overview and discoverability.
4. Run the offline Rust checks and the phase probes that cover the change.
   State which checks ran and record observed results and blockers in the
   [implementation status](docs/implementation-status.md).
5. Verify that temporary fixtures and test resources have been cleaned up.

## Rust Module Organization

Name modules for the responsibility they own. Keep role entry points focused
on startup and wiring. Keep MCP transport handling, Git object access,
worker confinement, and database access in their respective modules. Keep
parsing and validation separate from filesystem, database, and network I/O
where the logic can run without those dependencies.

## Commit Messages

Use scoped Conventional Commits with this format.

```text
<type>(<scope>): <subject>
```

- Use one of `feat`, `fix`, `chore`, `docs`, `ci`, `refactor`, `security`,
  `tests`, or `perf` for the type.
- Use the affected area or responsibility for the scope, such as `mcp`,
  `storage`, or `compose`.
- Start the subject with an imperative verb. Use backticks for paths,
  identifiers, commands, and literal values in the title and body.

```text
fix(mcp): enforce the declared protocol revision
tests(storage): cover read-only role grants
docs(compose): clarify the qualification procedure
```

Keep commits focused and GPG-sign them. Add
`Assisted-by: Codex <codex@openai.com>` to each Codex-assisted commit. Obtain
separate authorization for remote delivery, image publication, and deployment.

## Tests and Temporary Data

Use the Rust toolchain declared in `rust-toolchain.toml`. Fetch the locked
inputs before the offline gate if the local cache needs them. The gate runs
formatting, Clippy with warnings denied, workspace tests, and doc tests.

```sh
export CARGO_TARGET_DIR=/tmp/rust/target
cargo fetch --locked
scripts/phase0/check-rust.sh
```

Run `python3 -B scripts/check-doc-links.py` to check relative links and heading
anchors in tracked Markdown. CI runs the same check in the workflow lint job.

Use synthetic fixtures. Generate disposable repositories, certificates, keys,
configuration, and probe output under `/tmp`. Remove generated data after the
run, including on failure where practical. Keep generated artifacts and
credentials out of Git. Preserve notices for reused material.

## Docker Compose Qualification

Run the [Compose qualification](docs/deployment-prerequisites.md) for changes
to images, runtime dependencies, or trust boundaries. Use the documented host
profile and harness so the role mounts, secrets, and network filtering are in
effect during the probes. Use the applicable phase probes for changes to the
MCP wire contract, database grants and TLS, filesystem and network isolation,
or worker confinement.

Scope cleanup to the test project and its containers, networks, volumes,
test-only images, firewall rules, and generated files. If cleanup fails,
report the retained resources. Record the tested revision and profile when
reporting a qualification result.

## Security Requirements

- Keep Git data in native bare object stores and PostgreSQL as the control
  plane. Follow the publication fences and visibility rules in the
  [storage decision](docs/decisions/0001-storage-identity-and-locking.md).
- Keep MCP private, stateless, and read-only. Serve only the declared MCP
  revision `2026-07-28` over Streamable HTTP. Treat request inputs as untrusted and
  preserve authority, origin, and forwarding checks. See the
  [MCP decision](docs/decisions/0002-private-mcp.md).
- Scope credentials, database privileges, filesystem access, and network
  access to each role. Keep migration authority separate and verify the
  database TLS chain and hostname. Fail closed when the Git or search
  worker cannot establish confinement. See the
  [role and worker decision](docs/decisions/0003-worker-and-role-boundaries.md).
- Keep live acquisition disabled until the source is approved for the
  intended refs, paths, history, and output forms. Use synthetic fixtures
  for qualification. See the
  [source approval decision](docs/decisions/0004-source-approval.md).
- Keep secrets in role-scoped runtime inputs. Keep credentials, private
  keys, and private endpoint values out of source, commit messages, and
  shared test output.

For security-sensitive changes, identify the affected boundary and untrusted
inputs. Describe the expected denial or failure behavior, cover it with a
focused regression check where practical, and report remaining risks and
compatibility concerns.

## Pull Request Checklist

- The change description identifies the affected areas and explains any
  behavior, configuration, or compatibility changes.
- Each commit uses the documented scope and imperative subject, carries a GPG
  signature, and includes the Codex trailer when applicable.
- Tests and technical documentation cover the changed behavior.
- Security-sensitive changes describe trust boundaries, untrusted inputs,
  failure behavior, and remaining risks.
- Offline checks and applicable phase probes are recorded with their observed
  results and any blockers.
- Temporary test data and project-scoped Compose resources have been cleaned
  up or any cleanup failure is reported.
