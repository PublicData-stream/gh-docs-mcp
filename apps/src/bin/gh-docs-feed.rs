use gh_docs_core::Result;
use gh_docs_mcp::{health, health_listener};

#[tokio::main]
async fn main() -> Result<()> {
    let health = health(health_listener().await?);
    tokio::select! {
        outcome = health => outcome?,
        outcome = tokio::signal::ctrl_c() => outcome?,
    }
    Ok(())
}
