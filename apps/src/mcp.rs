//! Adapted from rfc-mcp-docker bcb4181. See docs/RFC-MCP-DOCKER-LICENSE.
//! Stateless Streamable HTTP using the pinned official SDK.

use std::{borrow::Cow, sync::Arc};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::Request,
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, MetaObject, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
        ServerConfig, Tool, ToolAnnotations,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use serde_json::{Value, json};

const TOOL: &str = "phase0_read_fixture";
const BODY_LIMIT: usize = 64 * 1024;
static VERSIONS: [ProtocolVersion; 1] = [ProtocolVersion::V_2026_07_28];

#[derive(Clone, Copy)]
struct Probe;

fn identity() -> Implementation {
    Implementation::new("gh-docs-mcp", env!("CARGO_PKG_VERSION"))
}

fn response_meta() -> MetaObject {
    let value = serde_json::to_value(identity()).expect("implementation identity is serializable");
    let mut meta = MetaObject::default();
    meta.insert("io.modelcontextprotocol/serverInfo".to_owned(), value);
    meta
}

fn fixture_tool() -> Tool {
    let schema = json!({"type": "object", "properties": {}, "additionalProperties": false});
    Tool::new(
        TOOL,
        "Read a fixed synthetic Phase 0 value",
        schema
            .as_object()
            .expect("tool schema is an object")
            .clone(),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    )
}

impl ServerHandler for Probe {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(identity())
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&VERSIONS)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        (name == TOOL).then(fixture_tool)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut result = ListToolsResult::with_all_items(vec![fixture_tool()]);
        result.meta = Some(response_meta());
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name != TOOL {
            return Err(ErrorData::invalid_params("Unknown probe tool", None));
        }
        let mut result = if request
            .arguments
            .as_ref()
            .is_some_and(|args| !args.is_empty())
        {
            CallToolResult::error(vec![ContentBlock::text("Expected empty arguments")])
        } else {
            CallToolResult::success(vec![ContentBlock::text("phase-zero fixture")])
        };
        result.meta = Some(response_meta());
        Ok(result.into())
    }
}

pub fn router(host: String, origin: String) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_stateless_protocol_metadata_required(true)
        .with_json_response(true)
        .with_max_request_body_bytes(BODY_LIMIT)
        .with_allowed_origins(vec![origin])
        .with_allowed_hosts(vec![host])
        .enforce_origin_validation();
    let service = StreamableHttpService::new(
        || Ok(Probe),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .route_service("/mcp", service)
        .route_layer(middleware::from_fn(reject_legacy_requests))
}

// Keep malformed messages and legacy lifecycle requests out of the SDK dispatch.
async fn reject_legacy_requests(request: Request, next: Next) -> Response {
    if request.headers().get_all(header::ORIGIN).iter().count() > 1 {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.method() != Method::POST {
        return next.run(request).await;
    }
    if request
        .headers()
        .contains_key(axum::http::header::CONTENT_ENCODING)
        && (request
            .headers()
            .get_all(axum::http::header::CONTENT_ENCODING)
            .iter()
            .count()
            != 1
            || !request.headers()[axum::http::header::CONTENT_ENCODING]
                .as_bytes()
                .eq_ignore_ascii_case(b"identity"))
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        to_bytes(body, BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let json_body = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if json_body {
        let envelope: Value = match serde_json::from_slice(&bytes) {
            Ok(envelope) => envelope,
            Err(_) => {
                return rpc_error(
                    StatusCode::BAD_REQUEST,
                    &Value::Null,
                    ErrorData::parse_error("Invalid JSON", None),
                );
            }
        };
        let method = envelope["method"].as_str();
        if envelope["jsonrpc"] != "2.0"
            || method.is_none()
            || envelope
                .get("id")
                .is_some_and(|id| !id.is_i64() && !id.is_string())
        {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                &Value::Null,
                ErrorData::invalid_request("Expected one JSON-RPC 2.0 request", None),
            );
        }
        if ["mcp-protocol-version", "mcp-method", "mcp-name"]
            .iter()
            .any(|name| parts.headers.get_all(*name).iter().count() > 1)
        {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                &envelope["id"],
                ErrorData::header_mismatch("Duplicate MCP request header", None),
            );
        }
        if method == Some("notifications/initialized") {
            return StatusCode::BAD_REQUEST.into_response();
        }
        if method == Some("initialize") {
            let version = parts
                .headers
                .get("mcp-protocol-version")
                .and_then(|value| value.to_str().ok());
            let valid_method = parts
                .headers
                .get("mcp-method")
                .is_some_and(|value| value == "initialize");
            let (status, error) = match version {
                Some(version) if valid_method && version != VERSIONS[0].as_str() => (
                    StatusCode::BAD_REQUEST,
                    ErrorData::new(
                        rmcp::model::ErrorCode::UNSUPPORTED_PROTOCOL_VERSION,
                        "Unsupported protocol version",
                        Some(json!({"requested": version, "supported": VERSIONS})),
                    ),
                ),
                Some(_) if valid_method => (
                    StatusCode::NOT_FOUND,
                    ErrorData::new(
                        rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                        "initialize is unavailable; use MCP 2026-07-28",
                        None,
                    ),
                ),
                _ => (
                    StatusCode::BAD_REQUEST,
                    ErrorData::header_mismatch(
                        "Missing or mismatched MCP headers; use MCP 2026-07-28",
                        None,
                    ),
                ),
            };
            return rpc_error(status, &envelope["id"], error);
        }
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

fn rpc_error(status: StatusCode, id: &Value, error: ErrorData) -> Response {
    (
        status,
        Json(json!({"jsonrpc": "2.0", "id": id, "error": error})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_registry_is_read_only_and_modern_only() {
        assert_eq!(Probe.supported_protocol_versions().as_ref(), VERSIONS);
        let tool = Probe.get_tool(TOOL).expect("fixture was registered");
        assert_eq!(tool.name, TOOL);
        assert_eq!(tool.input_schema["additionalProperties"], false);
        assert_eq!(
            tool.annotations.expect("annotations").read_only_hint,
            Some(true)
        );
        assert!(Probe.get_tool("git.read").is_none());
    }
}
