# Contributing

Use the Rust toolchain declared in `rust-toolchain.toml`. Fetch locked dependencies with `cargo fetch --locked` and run `scripts/phase0/check-rust.sh`. Run the [Compose qualification](docs/deployment-prerequisites.md) when changing images, runtime dependencies, or trust boundaries.

Keep commits focused and GPG-sign them. Add `Assisted-by: Codex <codex@openai.com>` to each Codex-assisted commit. Treat remote delivery, image publication, and deployment as separate operations. Use synthetic fixtures and preserve notices for reused material.
