use gh_docs_core::Result;
use gh_docs_mcp::{health, health_listener};
use gh_docs_storage::connect;
use gh_docs_sync::{publication, supervisor};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [command, selector, _] if command == "--reconcile" && selector == "--repository")
    {
        let status = gh_docs_sync::supervisor::run(&args).await?;
        if !status.success() {
            return Err("reconciliation failed".into());
        }
        return Ok(());
    }
    if let [child, command, selector, value] = args.as_slice()
        && child == gh_docs_sync::supervisor::CHILD
        && command == "--reconcile"
        && selector == "--repository"
    {
        let lifeline = gh_docs_sync::supervisor::child_lifeline()?;
        let (client, driver) = connect("gh_docs_sync").await?;
        let result = gh_docs_sync::reconcile(&client, value.parse()?, None, lifeline).await;
        drop(client);
        driver.await?;
        return result;
    }
    if matches!(args.as_slice(), [flag] if flag == "--migrate") {
        let (mut client, driver) = connect("gh_docs_migrate").await?;
        let result = gh_docs_storage::catalog::migrate(&mut client).await;
        drop(client);
        driver.await?;
        result?;
        return Ok(());
    }
    if let [command, selector, value] = args.as_slice()
        && matches!(
            (command.as_str(), selector.as_str()),
            ("--register-source", "--approval-file") | ("--revoke-source", "--source")
        )
    {
        let (client, driver) = connect("gh_docs_migrate").await?;
        let result = if command == "--register-source" {
            gh_docs_sync::register(&client, std::path::Path::new(value)).await
        } else {
            gh_docs_sync::revoke(&client, value.parse()?).await
        };
        drop(client);
        driver.await?;
        return result;
    }
    let supervised = args.first().is_some_and(|flag| flag == supervisor::CHILD);
    let command_args = if supervised { &args[1..] } else { &args };
    if let [flag, selector, repository, rest @ ..] = command_args
        && selector == "--repository"
        && let Some(command) = publication::Command::parse(flag, rest)?
    {
        let status = matches!(command, publication::Command::Status(_));
        if !status && !supervised {
            if !supervisor::run(&args).await?.success() {
                return Err("publication command failed".into());
            }
            return Ok(());
        }
        let (client, mut driver) = connect("gh_docs_sync").await?;
        let repo = repository.parse()?;
        let fault = publication::Fault::default();
        let result = if status {
            publication::run(&client, repo, command, &fault).await
        } else {
            let mut lifeline = supervisor::child_lifeline()?;
            tokio::select! {
                outcome = publication::run(&client, repo, command, &fault) => outcome,
                _ = supervisor::until_lost(&mut lifeline) => Err("publication supervisor disconnected".into()),
                _ = &mut driver => return Err("publication database connection closed".into()),
            }
        };
        drop(client);
        driver.await?;
        println!("{}", result?);
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
