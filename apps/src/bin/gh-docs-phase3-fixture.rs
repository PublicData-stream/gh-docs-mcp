use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use gh_docs_core::{
    Result,
    git::{ObjectType, Oid, RepositoryId},
};
use gh_docs_git::reader::Reachability;
use gh_docs_storage::{connect, publication::read_generation};
use gh_docs_sync::publication::{self, Command, Fault};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag, repo, generation, commit, rest @ ..] if flag == "read" => {
            let hold = match rest {
                [] => None,
                [flag, dir] if flag == "--hold" && dir == "/tmp/phase3-reader" => {
                    Some(PathBuf::from(dir))
                }
                _ => return Err("invalid reader fixture arguments".into()),
            };
            let generation = if generation == "active" {
                None
            } else {
                Some(generation.parse()?)
            };
            read(repo.parse()?, generation, commit.clone(), hold).await
        }
        [flag, repository, rest @ ..] => {
            let command =
                Command::parse(flag, rest)?.ok_or("unknown fixture publication command")?;
            let (client, driver) = connect("gh_docs_sync").await?;
            let fault = Fault::fixture(std::env::var("GH_DOCS_PHASE3_FAULT").ok());
            let result = publication::run(&client, repository.parse()?, command, &fault).await;
            drop(client);
            driver.await?;
            println!("{}", result?);
            Ok(())
        }
        _ => Err("unknown Phase 3 fixture command".into()),
    }
}

async fn read(
    repo: RepositoryId,
    generation: Option<i64>,
    commit: String,
    hold: Option<PathBuf>,
) -> Result<()> {
    let (mut client, driver) = connect("gh_docs_serve").await?;
    let result = read_generation(&mut client, repo, generation, move |snapshot, reader| {
        let oid = Oid::parse(snapshot.format, commit.as_bytes())?;
        if snapshot.reachable(reader, oid)? != Reachability::Reachable {
            return Err("commit unavailable in this generation".into());
        }
        let links = reader.batch(|batch| batch.commit(oid))?;
        let entries = reader.tree(links.tree)?;
        let blob = entries
            .iter()
            .find(|entry| entry.name == b"original.bin")
            .ok_or("fixture blob unavailable")?
            .oid;
        let raw = reader.batch(|batch| {
            let raw = batch.contents(blob, ObjectType::Blob)?;
            if let Some(hold) = hold {
                fs::write(hold.join("running"), b"worker ready\n")?;
                let deadline = Instant::now() + Duration::from_secs(12);
                while !hold.join("release").exists() {
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "fixture release timed out",
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
            Ok(raw)
        })?;
        let sum: u64 = raw.iter().map(|&byte| u64::from(byte)).sum();
        Ok(
            json!({"generation":snapshot.id.to_string(), "sequence":snapshot.sequence.to_string(),
            "commit":commit, "blob":blob.to_string(), "size":raw.len(), "byte_sum":sum}),
        )
    })
    .await;
    drop(client);
    driver.await?;
    println!("{}", result?);
    Ok(())
}
