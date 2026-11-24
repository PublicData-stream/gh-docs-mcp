//! Migration and typed access to the offline candidate catalog.

use gh_docs_core::{
    Result,
    git::{ObjectFormat, ObjectType, Oid, RefName, RepositoryId},
};
use serde_json::json;
use tokio_postgres::Client;
use uuid::Uuid;

pub async fn migrate(client: &mut Client) -> Result<()> {
    // The legacy migration owns its transaction and already takes this key.
    client
        .batch_execute(include_str!("../../../migrations/0001_foundation.sql"))
        .await?;
    let transaction = client.transaction().await?;
    transaction
        .query_one("SELECT pg_advisory_xact_lock(1)", &[])
        .await?;
    let exists: bool = transaction
        .query_one(
            "SELECT to_regclass('git_catalog.migrations') IS NOT NULL",
            &[],
        )
        .await?
        .get(0);
    if !exists {
        transaction
            .batch_execute(include_str!("../../../migrations/0002_git_catalog.sql"))
            .await?;
    }
    let versions: Vec<i32> = transaction
        .query(
            "SELECT version FROM git_catalog.migrations ORDER BY version",
            &[],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    if !matches!(versions.as_slice(), [2] | [2, 3] | [2, 3, 4]) {
        return Err("unsupported Git catalog migration version".into());
    }
    if !versions.contains(&3) {
        transaction
            .batch_execute(include_str!("../../../migrations/0003_synchronization.sql"))
            .await?;
    }
    if !versions.contains(&4) {
        transaction
            .batch_execute(include_str!("../../../migrations/0004_publication.sql"))
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

#[derive(Debug)]
pub struct RefSnapshot {
    pub name: RefName,
    pub target: Oid,
    pub kind: ObjectType,
    pub peeled_commit: Option<Oid>,
}

pub async fn register_fixture(
    client: &Client,
    repo: RepositoryId,
    source: Uuid,
    format: ObjectFormat,
    refs: &[RefName],
) -> Result<()> {
    RepositoryId::new(source)?;
    let refs: Vec<_> = refs.iter().map(RefName::bytes).collect();
    client
        .query_one(
            "SELECT git_catalog.register_fixture($1, $2, $3, $4, $5)",
            &[
                &repo.uuid(),
                &source,
                &format.name(),
                &format!("urn:gh-docs:fixture:{repo}"),
                &refs,
            ],
        )
        .await?;
    Ok(())
}

pub async fn begin_fixture_candidate(
    client: &Client,
    repo: RepositoryId,
    source: Uuid,
    default_ref: &RefName,
    refs: &[RefSnapshot],
) -> Result<i64> {
    if !(1..=50_000).contains(&refs.len()) {
        return Err("invalid fixture snapshot size".into());
    }
    let refs: Vec<_> = refs
        .iter()
        .map(|reference| {
            let encoded: String = reference
                .name
                .bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            json!({
                "ref": encoded,
                "target": reference.target.to_string(),
                "kind": reference.kind.name(),
                "peeled": reference.peeled_commit.map(|oid| oid.to_string()),
            })
        })
        .collect();
    Ok(client
        .query_one(
            "SELECT git_catalog.begin_fixture_candidate($1, $2, $3, $4)",
            &[&repo.uuid(), &source, &default_ref.bytes(), &json!(refs)],
        )
        .await?
        .get(0))
}

pub enum FixtureDisposition {
    Verified([u8; 32]),
    Failed,
    Discarded,
}

pub async fn finish_fixture_candidate(
    client: &Client,
    repo: RepositoryId,
    generation: i64,
    disposition: FixtureDisposition,
) -> Result<()> {
    let (state, digest) = match &disposition {
        FixtureDisposition::Verified(digest) => ("verified", Some(digest.as_slice())),
        FixtureDisposition::Failed => ("failed", None),
        FixtureDisposition::Discarded => ("discarded", None),
    };
    client
        .query_one(
            "SELECT git_catalog.finish_fixture_candidate($1, $2, $3, $4)",
            &[&repo.uuid(), &generation, &state, &digest],
        )
        .await?;
    Ok(())
}
