use gh_docs_core::Result;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [arg] if arg == "--confinement-probe") {
        return confinement();
    }
    cli(args)
}

#[tokio::main]
async fn cli(args: Vec<String>) -> Result<()> {
    if args.len() == 3 {
        let status = gh_docs_sync::supervisor::run(&args).await?;
        return if status.success() {
            Ok(())
        } else {
            Err("fixture reconciliation failed".into())
        };
    }
    let [child, repo, address, ca] = args.as_slice() else {
        return Err("expected fixture repository, address and CA".into());
    };
    if child != gh_docs_sync::supervisor::CHILD {
        return Err("invalid fixture supervisor invocation".into());
    }
    let lifeline = gh_docs_sync::supervisor::child_lifeline()?;
    let address: std::net::SocketAddr = address.parse()?;
    if address.ip().is_unspecified() || address.ip().is_multicast() {
        return Err("invalid fixture address".into());
    }
    let fixture = gh_docs_sync::Fixture {
        address,
        ca: (ca != "-").then(|| ca.into()),
        fault: std::env::var("GH_DOCS_FIXTURE_FAULT").ok(),
    };
    let (client, driver) = gh_docs_storage::connect("gh_docs_sync").await?;
    let result = gh_docs_sync::reconcile(&client, repo.parse()?, Some(fixture), lifeline).await;
    drop(client);
    driver.await?;
    result
}

fn confinement() -> Result<()> {
    use std::{fs::File, net::TcpStream};
    gh_docs_git::sandbox::confine_sync("/git-sync-scratch/1", false)?;
    let denied = |error: &std::io::Error| error.kind() == std::io::ErrorKind::PermissionDenied;
    if !matches!(File::open("/run/secrets/db-password"), Err(error) if denied(&error))
        || !matches!(TcpStream::connect("127.0.0.1:9"), Err(error) if denied(&error))
        || !matches!(File::create("/git-sync-private/worker-denied"), Err(error) if denied(&error))
        || File::create("/git-sync-scratch/1/confinement-probe").is_err()
    {
        return Err("sync sandbox boundary probe failed".into());
    }
    std::fs::remove_file("/git-sync-scratch/1/confinement-probe")?;
    println!("sync worker network, credential and unrelated write denials passed");
    Ok(())
}
