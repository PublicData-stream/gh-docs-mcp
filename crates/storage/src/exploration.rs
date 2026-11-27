//! Read-only projection of current, reviewed exploration scope.

use docs_mcp_core::{
    Result,
    exploration::{ExplorationApproval, Policy},
    git::{ObjectFormat, RefName, RepositoryId},
};
use tokio_postgres::{Client, GenericClient, Row};
use uuid::Uuid;

const FIELDS: &str = "repository_id, source_id, object_format, generation_id, publication_sequence,                       canonical_identity, observed_at, ref_count, policy_version, default_ref, exploration";
const VIEW: &str = "git_catalog.exploration_generations";

pub struct Metadata {
    pub repository: RepositoryId,
    pub source: Uuid,
    pub format: ObjectFormat,
    pub generation: i64,
    pub sequence: i64,
    pub identity: String,
    pub observed_at: String,
    pub ref_count: i64,
    pub policy_version: i64,
    pub default_ref: Option<RefName>,
    pub policy: Policy,
}

impl Metadata {
    fn from_row(row: Row) -> Result<Self> {
        let approval: ExplorationApproval = serde_json::from_value(row.try_get("exploration")?)?;
        let source = RepositoryId::new(row.try_get("source_id")?)?.uuid();
        Ok(Self {
            repository: RepositoryId::new(row.try_get("repository_id")?)?,
            source,
            format: row.try_get::<_, &str>("object_format")?.parse()?,
            generation: row.try_get("generation_id")?,
            sequence: row.try_get("publication_sequence")?,
            identity: row.try_get("canonical_identity")?,
            observed_at: row.try_get("observed_at")?,
            ref_count: row.try_get("ref_count")?,
            policy_version: row.try_get("policy_version")?,
            default_ref: row
                .try_get::<_, Option<Vec<u8>>>("default_ref")?
                .map(RefName::new)
                .transpose()?,
            policy: approval.validate()?,
        })
    }
}

pub async fn list(
    client: &Client,
    query: &str,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Metadata>> {
    client
        .query(
            &format!(
                "SELECT {FIELDS} FROM {VIEW} WHERE is_active
                  AND ($1::uuid IS NULL OR repository_id > $1)
                  AND ($2 = '' OR strpos(repository_id::text, $2) > 0
                      OR strpos(canonical_identity, $2) > 0)
                  ORDER BY repository_id LIMIT $3"
            ),
            &[&after, &query, &limit],
        )
        .await?
        .into_iter()
        .map(Metadata::from_row)
        .collect()
}

pub async fn get(
    client: &impl GenericClient,
    repo: RepositoryId,
    generation: Option<i64>,
) -> Result<Option<Metadata>> {
    client
        .query_opt(
            &format!(
                "SELECT {FIELDS} FROM {VIEW}
                  WHERE repository_id = $1 AND (generation_id = $2 OR ($2 IS NULL AND is_active))"
            ),
            &[&repo.uuid(), &generation],
        )
        .await?
        .map(Metadata::from_row)
        .transpose()
}
