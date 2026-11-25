//! Adapted from rfc-mcp-docker bcb4181. See docs/RFC-MCP-DOCKER-LICENSE.
//! Stateless Streamable HTTP using the pinned official SDK.

use std::{
    borrow::Cow,
    sync::{Arc, LazyLock, Mutex},
    time::Instant,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::Request,
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use gh_docs_search::exploration::{self, Error, Explorer};
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
use schemars::JsonSchema;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

const TOOL: &str = "phase0_read_fixture";
const BODY_LIMIT: usize = 64 * 1024;
static VERSIONS: [ProtocolVersion; 1] = [ProtocolVersion::V_2026_07_28];

#[derive(Clone)]
enum Handler {
    Fixture,
    Exploration {
        explorer: Arc<Explorer>,
        rate: Arc<Mutex<Rate>>,
    },
}

struct Rate {
    tokens: f64,
    at: Instant,
}

impl Rate {
    const CALLS_PER_SECOND: f64 = 64.0;

    fn new() -> Self {
        Self {
            tokens: Self::CALLS_PER_SECOND,
            at: Instant::now(),
        }
    }

    fn admit(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens
            + now.duration_since(self.at).as_secs_f64() * Self::CALLS_PER_SECOND)
            .min(Self::CALLS_PER_SECOND);
        self.at = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

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

fn definition<T: JsonSchema>(name: &str, description: &str, required: &[&str]) -> Tool {
    let input = serde_json::to_value(schemars::schema_for!(T)).expect("generated input schema");
    let mut properties = serde_json::Map::new();
    for field in required {
        let field_schema = match *field {
            "context" | "base" => json!({"$ref":"#/$defs/Context"}),
            "source" | "coverage" | "commit" => json!({"type":"object"}),
            "entries" | "matches" => json!({"type":"array","items":{"type":"object"}}),
            "next_cursor" | "parent" => json!({"type":["string","null"]}),
            "scan_complete" => json!({"type":"boolean"}),
            _ => json!({"type":"string"}),
        };
        properties.insert((*field).to_owned(), field_schema);
    }
    let context = schemars::schema_for!(exploration::Context);
    let schema = json!({
        "type":"object",
        "$defs":{"Context":context},
        "oneOf":[
            {"required":required,"properties":properties,"not":{"required":["code"]}},
            {"required":["code","guidance"],"properties":{
                "code":{"type":"string"},"guidance":{"type":["string","null"]}
            },"additionalProperties":false}
        ]
    });
    let mut tool = Tool::new(
        name.to_owned(),
        description.to_owned(),
        input.as_object().expect("object input schema").clone(),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    );
    tool.output_schema = Some(Arc::new(
        schema.as_object().expect("object output schema").clone(),
    ));
    tool
}

static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| {
    vec![
        definition::<exploration::RepositorySearch>(
            "repository.search",
            "List reviewed active repository metadata in UUID order. Pages reflect the live catalog.",
            &["entries", "coverage", "next_cursor"],
        ),
        definition::<exploration::RepositoryInfo>(
            "repository.info",
            "Read approved repository identity, policy, notice and active generation.",
            &["repository_id", "generation_id", "metadata_notice"],
        ),
        definition::<exploration::Refs>(
            "git.refs",
            "List the approved ref snapshot of a readable generation in raw-byte order.",
            &["entries", "source", "coverage", "next_cursor"],
        ),
        definition::<exploration::Resolve>(
            "git.resolve",
            "Resolve the default, an exact ref or a full OID to an immutable context reusable by other tools.",
            &["context", "source"],
        ),
        definition::<exploration::ListTree>(
            "git.ls_tree",
            "List approved tree paths, modes and object identities from a resolved context.",
            &["context", "source", "entries", "coverage", "next_cursor"],
        ),
        definition::<exploration::Find>(
            "git.find",
            "Match approved raw paths using a literal substring or an anchored byte glob.",
            &["context", "source", "entries", "coverage", "next_cursor"],
        ),
        definition::<exploration::Read>(
            "git.read",
            "Read a literal approved blob path. Byte ranges are zero-based and half-open. Use base64 for arbitrary byte slices.",
            &["context", "source", "oid", "coverage"],
        ),
        definition::<exploration::Rg>(
            "git.rg",
            "Search approved blobs at a resolved commit. Lines are one-based and original byte ranges are zero-based and half-open. A partial empty page leaves results unknown.",
            &[
                "context",
                "source",
                "matches",
                "coverage",
                "scan_complete",
                "next_cursor",
            ],
        ),
        definition::<exploration::Log>(
            "git.log",
            "List approved commit metadata by graph distance and then native object ID. A literal path filters changes against the first parent.",
            &["context", "source", "entries", "coverage", "next_cursor"],
        ),
        definition::<exploration::Show>(
            "git.show",
            "Read approved commit metadata and changes against the first or selected parent. Root commits use the empty tree.",
            &[
                "context",
                "source",
                "commit",
                "parent",
                "entries",
                "coverage",
                "next_cursor",
            ],
        ),
        definition::<exploration::Diff>(
            "git.diff",
            "Compare two approved commits in one repository generation. Returns approved paths and bounded hunks or binary metadata.",
            &[
                "context",
                "source",
                "base",
                "entries",
                "coverage",
                "next_cursor",
            ],
        ),
        definition::<exploration::Blame>(
            "git.blame",
            "Attribute approved lines at one literal path along first parents. Line ranges are one-based and half-open. Coverage identifies unresolved lines.",
            &["context", "source", "entries", "coverage", "next_cursor"],
        ),
    ]
});

pub fn tool_snapshot() -> Value {
    json!(&*TOOLS)
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(identity())
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&VERSIONS)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match self {
            Self::Fixture => (name == TOOL).then(fixture_tool),
            Self::Exploration { .. } => TOOLS.iter().find(|tool| tool.name == name).cloned(),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = match self {
            Self::Fixture => vec![fixture_tool()],
            Self::Exploration { .. } => TOOLS.clone(),
        };
        let mut result = ListToolsResult::with_all_items(tools);
        result.meta = Some(response_meta());
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.get_tool(&request.name).is_none() {
            return Err(ErrorData::invalid_params("Unknown tool", None));
        }
        let result = match self {
            Self::Fixture => {
                if request
                    .arguments
                    .as_ref()
                    .is_some_and(|args| !args.is_empty())
                {
                    CallToolResult::error(vec![ContentBlock::text("Expected empty arguments")])
                } else {
                    CallToolResult::success(vec![ContentBlock::text("phase-zero fixture")])
                }
            }
            Self::Exploration { explorer, rate } => {
                let admitted = rate
                    .lock()
                    .map_err(|_| ErrorData::internal_error("Rate limiter unavailable", None))?
                    .admit();
                let parsed = serde_json::from_value::<exploration::Request>(json!({
                    "tool":request.name, "arguments":request.arguments.unwrap_or_default()
                }));
                let result = match (admitted, parsed) {
                    (false, _) => Err(Error::Budget),
                    (_, Err(_)) => Err(Error::InvalidArgument),
                    (true, Ok(request)) => tokio::select! {
                        () = context.ct.cancelled() => Err(Error::Budget),
                        result = explorer.execute(request) => result,
                    },
                };
                match result {
                    Ok(result) => CallToolResult::structured(result),
                    Err(error) => CallToolResult::structured_error(json!(error)),
                }
            }
        };
        Ok(result.with_meta(Some(response_meta())).into())
    }
}

pub fn router(
    host: String,
    origin: String,
    explorer: Option<Arc<Explorer>>,
    config: StreamableHttpServerConfig,
) -> Router {
    let handler = match explorer {
        Some(explorer) => Handler::Exploration {
            explorer,
            rate: Arc::new(Mutex::new(Rate::new())),
        },
        None => Handler::Fixture,
    };
    let admission = Arc::new(Semaphore::new(16));
    let config = config
        .with_legacy_session_mode(false)
        .with_stateless_protocol_metadata_required(true)
        .with_json_response(true)
        .with_max_request_body_bytes(BODY_LIMIT)
        .with_allowed_origins(vec![origin])
        .with_allowed_hosts(vec![host])
        .enforce_origin_validation();
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .route_service("/mcp", service)
        .route_layer(middleware::from_fn(reject_legacy_requests))
        .route_layer(middleware::from_fn(move |request: Request, next: Next| {
            let admission = Arc::clone(&admission);
            async move {
                let Ok(_permit) = admission.try_acquire_owned() else {
                    return StatusCode::TOO_MANY_REQUESTS.into_response();
                };
                next.run(request).await
            }
        }))
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
    fn production_registry_matches_exported_schema() {
        let snapshot: Value = serde_json::from_str(include_str!("../schemas/tools.json")).unwrap();
        assert_eq!(tool_snapshot(), snapshot);
        assert_eq!(TOOLS.len(), 12);
    }

    #[test]
    fn fixture_registry_is_read_only_and_modern_only() {
        assert_eq!(
            Handler::Fixture.supported_protocol_versions().as_ref(),
            VERSIONS
        );
        let tool = Handler::Fixture
            .get_tool(TOOL)
            .expect("fixture was registered");
        assert_eq!(tool.name, TOOL);
        assert_eq!(tool.input_schema["additionalProperties"], false);
        assert_eq!(
            tool.annotations.expect("annotations").read_only_hint,
            Some(true)
        );
        assert!(Handler::Fixture.get_tool("git.read").is_none());
    }
}
