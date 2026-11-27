//! Read-only Markdown snapshot projections.

use docs_mcp_core::Result;
use tokio_postgres::{Client, Row};
use uuid::Uuid;

const SNAPSHOTS: &str = "git_catalog.readable_markdown_snapshots";
const DOCUMENTS: &str = "git_catalog.readable_markdown_documents";

pub async fn sources(client: &Client, offset: i64, limit: i64) -> Result<Vec<Row>> {
    Ok(client
        .query(
            &format!(
                "SELECT * FROM (
            SELECT repository_id AS source_id, NULL::text AS digest,
                repository_id, generation_id, NULL::text AS commit_oid, object_format,
                jsonb_build_object('kind','git_markdown') AS input,
                exploration ->> 'metadata_notice' AS notice
            FROM git_catalog.exploration_generations WHERE is_active
            UNION ALL
            SELECT source_id, digest, repository_id, generation_id, commit_oid,
                object_format, input, notice
            FROM {SNAPSHOTS} WHERE is_current
        ) AS sources ORDER BY source_id, input ->> 'kind', digest OFFSET $1 LIMIT $2"
            ),
            &[&offset, &limit],
        )
        .await?)
}

pub async fn snapshot(client: &Client, source: Uuid, digest: &str) -> Result<Option<Row>> {
    Ok(client
        .query_opt(
            &format!("SELECT * FROM {SNAPSHOTS} WHERE source_id = $1 AND digest = $2"),
            &[&source, &digest],
        )
        .await?)
}

pub async fn documents(
    client: &Client,
    source: Uuid,
    digest: &str,
    offset: i64,
    limit: i64,
    bodies: bool,
) -> Result<Vec<Row>> {
    Ok(client
        .query(
            &format!(
                "SELECT path, source_path, item, kind, line, CASE WHEN $5 THEN markdown END AS body
            FROM {DOCUMENTS} WHERE source_id = $1 AND digest = $2 ORDER BY path OFFSET $3 LIMIT $4"
            ),
            &[&source, &digest, &offset, &limit, &bodies],
        )
        .await?)
}

pub async fn document(
    client: &Client,
    source: Uuid,
    digest: &str,
    path: &str,
) -> Result<Option<Row>> {
    Ok(client
        .query_opt(
            &format!(
                "SELECT path, source_path, item, kind, line, markdown AS body FROM {DOCUMENTS}
            WHERE source_id = $1 AND digest = $2 AND path = $3"
            ),
            &[&source, &digest, &path],
        )
        .await?)
}
