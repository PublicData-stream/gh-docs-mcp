use gh_docs_core::{Result, required};
use gh_docs_mcp::{health, health_listener, mcp};
use gh_docs_storage::connect;
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
            "git" => gh_docs_git::phase0_probe(force_failure),
            "search" => gh_docs_search::phase0_probe(force_failure),
            _ => return Err("unknown worker profile".into()),
        };
        return result.map_err(Into::into);
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
    if let Some(probe) = mode {
        let result = gh_docs_storage::probe(client, probe).await;
        driver.await?;
        return result;
    }
    gh_docs_storage::verify_fixture(&client).await?;
    let api = TcpListener::bind(required("MCP_BIND")?).await?;
    let host = required("MCP_HOST")?;
    let origin = required("MCP_ORIGIN")?;
    let health = health(health_listener().await?);
    let server = axum::serve(api, mcp::router(host, origin));
    tokio::select! {
        outcome = server => outcome?,
        outcome = health => outcome?,
        _ = driver => return Err("serve database connection closed".into()),
        outcome = tokio::signal::ctrl_c() => outcome?,
    }
    Ok(())
}
