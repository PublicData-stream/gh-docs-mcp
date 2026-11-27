//! Background Rustdoc acquisition owned by the Sync scheduler.

use std::{future::pending, sync::Arc, time::Duration};

use docs_mcp_core::Result;
use docs_mcp_storage::connect;
use docs_mcp_sync::markdown::{hosted, jobs};
use tokio_postgres::Client;

mod runner;

pub fn enabled() -> Result<bool> {
    match std::env::var("DOCS_MCP_RUSTDOC_ENABLED").as_deref() {
        Ok("1") => Ok(true),
        Ok("0" | "") | Err(std::env::VarError::NotPresent) => Ok(false),
        _ => Err("DOCS_MCP_RUSTDOC_ENABLED must be 0 or 1".into()),
    }
}

pub async fn run(owner: Arc<Client>) -> Result<()> {
    if !enabled()? {
        return pending().await;
    }
    tokio::try_join!(schedule(&owner), hosted_worker(), runner::serve())?;
    Ok(())
}

async fn schedule(owner: &Client) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut prune = tokio::time::interval(Duration::from_secs(300));
    loop {
        tokio::select! {
            _ = tick.tick() => jobs::schedule(owner).await?,
            _ = prune.tick() => jobs::prune(owner).await?,
        }
    }
}

async fn hosted_worker() -> Result<()> {
    let fixture = crate::control::fixture()?;
    let (mut client, mut driver) = connect("gh_docs_sync").await?;
    let run = async {
        loop {
            if let Some(job) = jobs::claim(&client, false).await? {
                if let Err(_error) = hosted::process(&mut client, &job, fixture.as_ref()).await {
                    #[cfg(feature = "phase6-fixtures")]
                    eprintln!("synthetic hosted Rustdoc publication failed: {_error:?}");
                    jobs::finish(
                        &mut client,
                        &job,
                        jobs::Outcome::Failure {
                            reason: "hosted_publication_failed",
                            retry: Some(Duration::from_secs(60)),
                        },
                    )
                    .await?;
                }
            } else {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };
    tokio::select! {
        result = run => result,
        _ = &mut driver => Err("hosted Rustdoc database connection closed".into()),
    }
}
