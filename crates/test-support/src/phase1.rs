//! Qualification commands for the disposable Phase 1 image targets.

use std::{
    fs, io,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use docs_mcp_core::{
    Result,
    git::{BytePath, ObjectFormat, ObjectType, Oid, RefName, RepositoryId, checked_range},
};
use docs_mcp_git::{
    reader::{Reachability, Reader, TreeMode},
    store::{self, Location, LockMode, LockScope},
};
use docs_mcp_storage::{
    catalog::{self, FixtureDisposition, RefSnapshot},
    connect,
};
use serde::Deserialize;
use tokio_postgres::{Client, error::SqlState};
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    repo: String,
    source: String,
    job: String,
    format: String,
    root: String,
    merge: String,
    old: String,
    left: String,
    oversized: String,
    tag: String,
    noncommit_tag: String,
    entries: Vec<Entry>,
    refs: Vec<Reference>,
}

#[derive(Deserialize)]
struct Reference {
    name: String,
    target: String,
    kind: String,
    peeled: Option<String>,
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    mode: String,
    oid: String,
    bytes: Option<String>,
}

pub async fn run(args: &[String]) -> Result<()> {
    let [mode, oracle, rest @ ..] = args else {
        return Err("usage: docs-mcp-phase1-fixture MODE ORACLE".into());
    };
    let fixtures: Vec<Fixture> = serde_json::from_slice(&fs::read(oracle)?)?;
    for expected in &fixtures {
        let repo = expected.repo.parse()?;
        let format = expected.format.parse()?;
        let canonical = Location::Canonical(repo);
        match (mode.as_str(), rest) {
            ("prepare-locks", []) => {
                for scope in [LockScope::Coordinator, LockScope::Repository] {
                    store::ensure_lock(repo, scope)?;
                }
            }
            ("fence-probe", []) => {
                let _fence = store::lock(
                    repo,
                    LockScope::Repository,
                    LockMode::Shared,
                    Instant::now() + Duration::from_secs(1),
                )?;
                println!("{repo} locked");
                use io::Write as _;
                io::stdout().flush()?;
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
            }
            ("exclusive-probe", []) => {
                let _fence = store::lock(
                    repo,
                    LockScope::Repository,
                    LockMode::Exclusive,
                    Instant::now() + Duration::from_millis(100),
                )?;
            }
            ("read", []) => verify(expected, canonical)?,
            ("reject-store", [selected]) if selected == &expected.repo => {
                let result = Reader::open(canonical, format)?.batch(|batch| {
                    let oid =
                        Oid::parse(format, expected.root.as_bytes()).map_err(io::Error::other)?;
                    batch.info(oid)
                });
                if result.is_ok() {
                    return Err("invalid bare Git store was accepted".into());
                }
            }
            ("reject-store", [_]) => (),
            ("fail-setup", []) => {
                let oid = Oid::parse(format, expected.root.as_bytes())?;
                if Reader::open(canonical, format)?
                    .force_setup_failure_for_probe()
                    .batch(|batch| batch.info(oid))
                    .is_ok()
                {
                    return Err("injected Git sandbox failure was accepted".into());
                }
            }
            ("register", []) => register(expected, repo, format).await?,
            ("serve-db", []) => {
                check_serve_database().await?;
                break;
            }
            _ => return Err("unsupported Phase 1 fixture command".into()),
        }
    }
    Ok(())
}

fn verify(expected: &Fixture, location: Location) -> Result<()> {
    let format = expected.format.parse::<ObjectFormat>()?;
    let oid = |value: &str| Oid::parse(format, value.as_bytes()).map_err(io::Error::other);
    let root = oid(&expected.root)?;
    let merge = oid(&expected.merge)?;
    let old = oid(&expected.old)?;
    let left = oid(&expected.left)?;
    let tag = oid(&expected.tag)?;
    let noncommit_tag = oid(&expected.noncommit_tag)?;
    let oversized = oid(&expected.oversized)?;
    let reader = Reader::open(location, format)?;
    let entries = reader.tree(root)?;
    if entries.len() != expected.entries.len() || reader.tree(merge).is_ok() {
        return Err("unexpected tree enumeration result".into());
    }
    reader.batch(|batch| {
        for (entry, oracle) in entries.iter().zip(&expected.entries) {
            let name = STANDARD.decode(&oracle.name).map_err(io::Error::other)?;
            let mode = match oracle.mode.as_str() {
                "040000" => TreeMode::Directory,
                "100644" => TreeMode::Regular,
                "100755" => TreeMode::Executable,
                "120000" => TreeMode::Symlink,
                "160000" => TreeMode::Gitlink,
                _ => return Err(io::Error::other("invalid fixture mode")),
            };
            if entry.name != name
                || entry.oid != oid(&oracle.oid)?
                || entry.mode != mode
                || BytePath::encode(&entry.name)
                    .and_then(|path| path.decode())
                    .map_err(io::Error::other)?
                    != name
            {
                return Err(io::Error::other(
                    "tree identity or raw path differs from trusted Git",
                ));
            }
            if let Some(bytes) = &oracle.bytes {
                let raw = batch.contents(entry.oid, ObjectType::Blob)?;
                if raw != STANDARD.decode(bytes).map_err(io::Error::other)? {
                    return Err(io::Error::other("blob bytes differ from trusted Git"));
                }
                let end = raw.len() as u64;
                let coordinates =
                    checked_range(end.min(1), end, raw.len()).map_err(io::Error::other)?;
                if raw[coordinates] != raw[end.min(1) as usize..]
                    || checked_range(0, end + 1, raw.len()).is_ok()
                {
                    return Err(io::Error::other("incorrect byte range semantics"));
                }
            }
        }
        for reference in &expected.refs {
            let target = oid(&reference.target)?;
            let kind = ObjectType::parse(reference.kind.as_bytes()).map_err(io::Error::other)?;
            let info = batch
                .info(target)?
                .ok_or_else(|| io::Error::other("missing fixture ref target"))?;
            let peeled = reference.peeled.as_deref().map(oid).transpose()?;
            if kind != info.kind || batch.peel_commit(target)? != peeled {
                return Err(io::Error::other(
                    "ref type or peeled OID differs from trusted Git",
                ));
            }
        }
        let commit = batch.commit(merge)?;
        if commit.tree != root
            || commit.parents.len() != 2
            || batch.peel_commit(tag)? != Some(left)
            || batch.peel_commit(noncommit_tag)?.is_some()
            || batch.reachable(&[merge], left)? != Reachability::Reachable
            || batch.reachable(&[merge], old)? != Reachability::NotReachable
            || batch.reachable(&[old], old)? != Reachability::Reachable
            || batch.reachable(&vec![merge; 50_001], left)? != Reachability::Indeterminate
        {
            return Err(io::Error::other(
                "incorrect annotated tags or commit reachability",
            ));
        }
        Ok(())
    })?;
    let large = reader.batch(|batch| batch.contents(oversized, ObjectType::Blob));
    if !large.is_err_and(|error| error.kind() == io::ErrorKind::FileTooLarge) {
        return Err("oversized blob was not rejected".into());
    }
    let wrong_type = reader.batch(|batch| batch.contents(merge, ObjectType::Blob));
    if !wrong_type.is_err_and(|error| error.kind() == io::ErrorKind::InvalidData) {
        return Err("unexpected Git object type was not rejected".into());
    }
    println!(
        "{} {} raw object, tree and reachability oracle passed",
        expected.repo, expected.format
    );
    Ok(())
}

async fn register(expected: &Fixture, repo: RepositoryId, format: ObjectFormat) -> Result<()> {
    let source = expected.source.parse::<Uuid>()?;
    let job = expected.job.parse::<Uuid>()?;
    let (client, connection) = connect("gh_docs_sync").await?;
    let mut refs: Vec<RefSnapshot> = expected
        .refs
        .iter()
        .map(|reference| {
            Ok(RefSnapshot {
                name: RefName::new(STANDARD.decode(&reference.name)?)?,
                target: Oid::parse(format, reference.target.as_bytes())?,
                kind: ObjectType::parse(reference.kind.as_bytes())?,
                peeled_commit: reference
                    .peeled
                    .as_ref()
                    .map(|oid| Oid::parse(format, oid.as_bytes()))
                    .transpose()?,
            })
        })
        .collect::<Result<_>>()?;
    let scope: Vec<_> = refs
        .iter()
        .map(|reference| reference.name.clone())
        .collect();
    let default = RefName::new(b"refs/heads/main".to_vec())?;
    let previous = Oid::parse(format, expected.old.as_bytes())?;
    catalog::register_fixture(&client, repo, source, format, &scope).await?;
    catalog::register_fixture(&client, repo, source, format, &scope).await?;
    let main = refs
        .iter()
        .position(|reference| reference.name == default)
        .ok_or("missing fixture main ref")?;
    let active = refs[main].target;
    refs[main].target = previous;
    refs[main].peeled_commit = Some(previous);
    for (index, target) in [previous, active].into_iter().enumerate() {
        refs[main].target = target;
        refs[main].peeled_commit = Some(target);
        let generation =
            catalog::begin_fixture_candidate(&client, repo, source, &default, &refs).await?;
        if generation != index as i64 {
            return Err("fixture generation IDs are not monotonic".into());
        }
        store::ensure_lock(repo, LockScope::Generation(generation))?;
        if let Err(error) = verify(expected, Location::Incoming { job }) {
            catalog::finish_fixture_candidate(
                &client,
                repo,
                generation,
                FixtureDisposition::Failed,
            )
            .await?;
            return Err(error);
        }
        let observed_at: String = client.query_one(
            "SELECT to_char(observed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')
             FROM git_catalog.repository_generations WHERE repository_id = $1 AND generation_id = $2",
            &[&repo.uuid(), &generation],
        ).await?.get(0);
        let digests = crate::manifest::Observation {
            repository: repo,
            source,
            format,
            policy: 1,
            default_ref: &default,
            refs: &refs,
            observed_at: &observed_at,
        }
        .digests()?;
        for _ in 0..2 {
            catalog::finish_fixture_candidate(
                &client,
                repo,
                generation,
                FixtureDisposition::Verified(digests.observed),
            )
            .await?;
        }
        if catalog::finish_fixture_candidate(
            &client,
            repo,
            generation,
            FixtureDisposition::Discarded,
        )
        .await
        .is_ok()
        {
            return Err("verified candidate state was mutable".into());
        }
        let logical: String = digests
            .logical
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        println!("{repo} generation {generation} verified with logical manifest digest {logical}");
    }
    let previous_preserved: bool = client
        .query_one(
            "SELECT target_oid = $2 FROM git_catalog.generation_refs
         WHERE repository_id = $1 AND generation_id = 0 AND ref_name = $3",
            &[&repo.uuid(), &previous.bytes(), &default.bytes()],
        )
        .await?
        .get(0);
    if !previous_preserved {
        return Err("verified ref snapshot changed after force-push fixture".into());
    }
    expect_denial(
        &client,
        "UPDATE git_catalog.generation_refs SET target_oid = target_oid",
    )
    .await?;
    expect_denial(
        &client,
        "INSERT INTO git_catalog.active_generations DEFAULT VALUES",
    )
    .await?;
    expect_denial(&client, "INSERT INTO git_catalog.sync_jobs DEFAULT VALUES").await?;
    expect_denial(
        &client,
        "INSERT INTO git_catalog.notification_outbox DEFAULT VALUES",
    )
    .await?;
    expect_denial(
        &client,
        "INSERT INTO git_catalog.repository_incidents DEFAULT VALUES",
    )
    .await?;
    drop(client);
    connection.await?;
    Ok(())
}

async fn check_serve_database() -> Result<()> {
    let (client, driver) = connect("gh_docs_serve").await?;
    let count: i64 = client
        .query_one("SELECT count(*) FROM git_catalog.visible_repositories", &[])
        .await?
        .get(0);
    if count != 0 {
        return Err("private candidates are visible to serve".into());
    }
    for sql in [
        "SELECT * FROM git_catalog.repository_sources",
        "SELECT * FROM git_catalog.repository_generations",
        "INSERT INTO git_catalog.active_generations DEFAULT VALUES",
        "SELECT git_catalog.register_fixture(NULL, NULL, NULL, NULL, NULL)",
    ] {
        expect_denial(&client, sql).await?;
    }
    drop(client);
    driver.await?;
    Ok(())
}

async fn expect_denial(client: &Client, sql: &str) -> Result<()> {
    client.batch_execute("BEGIN READ WRITE").await?;
    let result = client.batch_execute(sql).await;
    client.batch_execute("ROLLBACK").await?;
    if !result.is_err_and(|error| error.code() == Some(&SqlState::INSUFFICIENT_PRIVILEGE)) {
        return Err(format!("expected privilege denial for {sql}").into());
    }
    Ok(())
}
