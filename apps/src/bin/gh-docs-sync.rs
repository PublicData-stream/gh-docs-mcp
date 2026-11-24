use gh_docs_core::Result;
use gh_docs_mcp::{health, health_listener};
use gh_docs_storage::connect;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [flag] if flag == "--migrate") {
        let (mut client, driver) = connect("gh_docs_migrate").await?;
        let result = gh_docs_storage::catalog::migrate(&mut client).await;
        drop(client);
        driver.await?;
        result?;
        return Ok(());
    }
    let (client, driver) = connect("gh_docs_sync").await?;
    match args.as_slice() {
        [] => (),
        [probe] => {
            let result = gh_docs_storage::probe(client, probe).await;
            driver.await?;
            return result;
        }
        _ => return Err("invalid sync arguments".into()),
    }
    gh_docs_storage::verify_fixture(&client).await?;
    gh_docs_git::store::initialize()?;
    let health = health(health_listener().await?);
    tokio::select! {
        outcome = health => outcome?,
        _ = driver => return Err("sync database connection closed".into()),
        outcome = tokio::signal::ctrl_c() => outcome?,
    }
    Ok(())
}
