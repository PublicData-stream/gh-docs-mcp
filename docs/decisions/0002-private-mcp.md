# 0002 — Private stateless MCP

Use solely the published MCP revision `2026-07-28` over Streamable HTTP. Pin `rmcp` to `3.11.0`. The `/mcp` endpoint uses stateless POST and JSON for the fixture, with a bounded request body. It accepts discovery, tool list, and direct call in any order. Phase 5 wires the [exploration registry](../mcp.md) into the serving path. The synthetic Phase 0 tool is available in the explicit qualification mode. Negotiate protocol features through the standard per-request metadata. Validate standard HTTP headers and ignore obsolete session and resumption identifiers as specified.

Bind the service MCP listener to the ingress address. Only the fixture TLS proxy can initiate traffic to it. Validate the original Host against the configured authority and validate Origin when present. The proxy strips forwarding identity headers supplied by clients. Health is bound to each service's loopback. Keep the fixture proxy on host loopback.

Require a raw HTTP probe and separately pinned TypeScript SDK client `2.4.0`. Product UI interoperability is outside the Phase 0 gate.

[Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http) · [Versioning and compatibility](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning) · [Rust SDK](https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v3.11.0) · [TypeScript SDK](https://github.com/modelcontextprotocol/typescript-sdk/releases/tag/v2.4.0)
