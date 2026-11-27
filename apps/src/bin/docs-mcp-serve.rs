use std::{fs::File, io::Read, net::SocketAddr, sync::Arc};

use docs_mcp::{
    control::{self, Command, Keys, Link, Outcome, Rejection},
    health, health_listener, mcp,
};
use docs_mcp_core::{Result, required};
use docs_mcp_search::exploration::{CursorKey, Explorer};
use docs_mcp_storage::connect;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use tokio::net::TcpListener;

const ROLE: &str = "gh_docs_serve";

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let [flag, profile, rest @ ..] = args.as_slice()
        && flag == "--phase0-worker"
    {
        let force_failure = match rest {
            [] => false,
            [flag] if flag == "--fail-setup" => true,
            _ => return Err("invalid worker arguments".into()),
        };
        let result = match profile.as_str() {
            "git" => docs_mcp_git::phase0_probe(force_failure),
            "search" => docs_mcp_search::phase0_probe(force_failure),
            _ => return Err("unknown worker profile".into()),
        };
        return result.map_err(Into::into);
    }
    if args.as_slice() == ["--tool-schemas"] {
        println!("{}", serde_json::to_string_pretty(&mcp::tool_snapshot())?);
        return Ok(());
    }
    if !matches!(args.as_slice(), [] | [_]) {
        return Err("invalid serve arguments".into());
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args.first().map(String::as_str)))
}

async fn run(mode: Option<&str>) -> Result<()> {
    let (client, driver) = connect(ROLE).await?;
    let fixture = mode == Some("--phase0-mcp-fixture");
    if let Some(probe) = mode.filter(|_| !fixture) {
        let result = docs_mcp_storage::probe(client, probe).await;
        driver.await?;
        return result;
    }
    let notify = control::enabled()? && !fixture;
    let mut notify_keys = None;
    let explorer = if fixture {
        docs_mcp_storage::verify_fixture(&client).await?;
        None
    } else {
        client
            .simple_query("SELECT 1 FROM git_catalog.exploration_generations LIMIT 0")
            .await?;
        let mut key = Vec::with_capacity(49);
        File::open(required("EXPLORATION_CURSOR_KEY_FILE")?)?
            .take(49)
            .read_to_end(&mut key)?;
        if notify {
            let keys = Keys::load(Link::SyncServe)?;
            if key
                .as_slice()
                .try_into()
                .is_ok_and(|cursor| keys.contains(cursor))
            {
                return Err("notification and cursor keys must be independent".into());
            }
            notify_keys = Some(keys);
        }
        Some(Arc::new(Explorer::new(CursorKey::new(
            required("EXPLORATION_CURSOR_KEY_ID")?,
            &key,
        )?)))
    };
    let api = TcpListener::bind(required("MCP_BIND")?).await?;
    let host = required("MCP_HOST")?;
    let origin = required("MCP_ORIGIN")?;
    let health = health(health_listener().await?);
    let config = StreamableHttpServerConfig::default();
    let shutdown = config.cancellation_token.clone();
    let router = mcp::router(host.clone(), origin, explorer.clone(), config);
    let router = match std::env::var("MCP_PROXY_IP") {
        Ok(peer) => mcp::with_trusted_proxy(router, peer.parse()?, &host)?,
        Err(std::env::VarError::NotPresent) => router,
        Err(error) => return Err(error.into()),
    };
    let server = axum::serve(
        api,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    );
    let notification = async {
        let Some(keys) = notify_keys else {
            return std::future::pending().await;
        };
        let address = control::address(&required("DOCS_MCP_SERVE_NOTIFY_BIND")?)?;
        let listener = TcpListener::bind(address).await?;
        let client = Arc::new(client);
        control::serve(listener, Link::SyncServe, keys, move |command| {
            observe(client.clone(), command)
        })
        .await
    };
    let result = tokio::select! {
        outcome = server => outcome.map_err(Into::into),
        outcome = notification => outcome,
        outcome = health => outcome.map_err(Into::into),
        _ = driver => Err("serve database connection closed".into()),
        outcome = control::shutdown() => outcome.map_err(Into::into),
    };
    shutdown.cancel();
    if let Some(explorer) = explorer {
        explorer.drain().await;
    }
    result
}

async fn observe(
    client: Arc<tokio_postgres::Client>,
    command: Command,
) -> std::result::Result<Outcome, Rejection> {
    let Command::Generation(event) = command else {
        return Err(Rejection::Conflict);
    };
    let generation: i64 = event
        .generation_id
        .parse()
        .map_err(|_| Rejection::Conflict)?;
    let sequence: i64 = event
        .publication_sequence
        .parse()
        .map_err(|_| Rejection::Conflict)?;
    let row = client
        .query_one(
            "SELECT git_catalog.observe_notification($1,$2,$3,$4)",
            &[
                &event.event_id,
                &event.repository_id,
                &generation,
                &sequence,
            ],
        )
        .await
        .map_err(|_| Rejection::Unavailable)?;
    match row.get::<_, Option<&str>>(0) {
        Some("observed") => {
            control::fault("after_notification_observed");
            Ok(Outcome::Observed)
        }
        Some("superseded") => Ok(Outcome::Superseded),
        _ => Err(Rejection::Conflict),
    }
}
