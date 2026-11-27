use docs_mcp::{control, health, health_listener, scheduler};
use docs_mcp_core::Result;
use docs_mcp_storage::connect;
use docs_mcp_sync::{markdown::jobs as markdown, publication, supervisor};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [command, selector, _] if command == "--reconcile" && selector == "--repository")
    {
        let status = docs_mcp_sync::supervisor::run(&args).await?;
        if !status.success() {
            return Err("reconciliation failed".into());
        }
        return Ok(());
    }
    if let [child, command, selector, value] = args.as_slice()
        && child == docs_mcp_sync::supervisor::CHILD
        && command == "--reconcile"
        && selector == "--repository"
    {
        let lifeline = docs_mcp_sync::supervisor::child_lifeline()?;
        let (client, driver) = connect("gh_docs_sync").await?;
        let result = docs_mcp_sync::reconcile(&client, value.parse()?, None, lifeline).await;
        drop(client);
        driver.await?;
        return result;
    }
    if let [child, flag, repository, repo, job, id] = args.as_slice()
        && child == supervisor::CHILD
        && flag == "--scheduled-reconcile"
        && repository == "--repository"
        && job == "--job"
    {
        // Observe ownership loss during connection setup and cleanup as well as
        // during the Git transfer. The reconcile future reaps its Git group on drop.
        let lifeline = supervisor::child_lifeline()?.into_std()?;
        let mut cancelled = tokio::net::UnixStream::from_std(lifeline.try_clone()?)?;
        let lifeline = tokio::net::UnixStream::from_std(lifeline)?;
        let reconcile = async {
            let (client, driver) = connect("gh_docs_sync").await?;
            let result = docs_mcp_sync::reconcile_job(
                &client,
                repo.parse()?,
                control::fixture()?,
                lifeline,
                Some(id.parse()?),
            )
            .await;
            drop(client);
            driver.await?;
            result
        };
        return tokio::select! {
            result = reconcile => result,
            _ = supervisor::until_lost(&mut cancelled) => Err("scheduler supervisor disconnected".into()),
        };
    }
    if let [flag, repository, repo, rest @ ..] = args.as_slice()
        && repository == "--repository"
        && matches!(flag.as_str(), "--pause" | "--resume" | "--schedule-status")
    {
        let expected = match (flag.as_str(), rest) {
            ("--pause" | "--schedule-status", []) => None,
            ("--resume", [selector, value]) if selector == "--expect-sequence" => {
                let number: i64 = value.parse()?;
                if number < 0 || number.to_string() != *value {
                    return Err("expected a canonical nonnegative BIGINT".into());
                }
                Some(number)
            }
            _ => return Err("invalid scheduler arguments".into()),
        };
        println!(
            "{}",
            scheduler::operator(repo.parse()?, flag, expected).await?
        );
        return Ok(());
    }
    if args.is_empty() {
        if control::enabled()? {
            return scheduler::run().await;
        }
        if docs_mcp::rustdoc::enabled()? {
            return Err("Rustdoc acquisition requires the Sync scheduler".into());
        }
    }
    if let [command, selector, value] = args.as_slice()
        && matches!(
            (command.as_str(), selector.as_str()),
            ("--register-markdown-source", "--approval-file")
                | (
                    "--revoke-markdown-source" | "--markdown-status" | "--markdown-retry",
                    "--source"
                )
        )
    {
        let operator = matches!(
            command.as_str(),
            "--register-markdown-source" | "--revoke-markdown-source"
        );
        let (client, driver) = connect(if operator {
            "gh_docs_migrate"
        } else {
            "gh_docs_sync"
        })
        .await?;
        let result = match command.as_str() {
            "--register-markdown-source" => {
                markdown::register(&client, std::path::Path::new(value)).await
            }
            "--revoke-markdown-source" => markdown::revoke(&client, value.parse()?).await,
            _ => {
                let status =
                    markdown::status(&client, value.parse()?, command == "--markdown-retry")
                        .await?;
                println!("{status}");
                Ok(())
            }
        };
        drop(client);
        driver.await?;
        return result;
    }
    if matches!(args.as_slice(), [flag] if flag == "--migrate") {
        let (mut client, driver) = connect("gh_docs_migrate").await?;
        let result = docs_mcp_storage::catalog::migrate(&mut client).await;
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
            docs_mcp_sync::register(&client, std::path::Path::new(value)).await
        } else {
            docs_mcp_sync::revoke(&client, value.parse()?).await
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
            let result = docs_mcp_storage::probe(client, probe).await;
            driver.await?;
            return result;
        }
        _ => return Err("invalid sync arguments".into()),
    }
    docs_mcp_storage::verify_fixture(&client).await?;
    docs_mcp_git::store::initialize()?;
    let health = health(health_listener().await?);
    tokio::select! {
        outcome = health => outcome?,
        _ = driver => return Err("sync database connection closed".into()),
        outcome = tokio::signal::ctrl_c() => outcome?,
    }
    Ok(())
}
