//! Durable admission and atomic publication for both Rustdoc inputs.

use std::{path::Path, time::Duration};

use docs_mcp_core::{
    Result,
    git::{ObjectFormat, Oid, RepositoryId},
    markdown::{Approval, DOCUMENTS, Document, Input, SNAPSHOT_BYTES, digest},
};
use docs_mcp_git::store::{LockMode, LockScope};
use docs_mcp_storage::publication::filesystem_fence;
use serde_json::{Value, json};
use tokio_postgres::{Client, Row, Transaction};
use uuid::Uuid;

const LEASE_SECONDS: i32 = 1800;
const RETRIES: i32 = 3;
const QUOTA: i64 = 1024 * 1024 * 1024;

pub struct Job {
    pub id: Uuid,
    pub attempt: i32,
    pub approval: Approval,
    pub git: Option<GitSnapshot>,
    pub etag: Option<String>,
}

pub struct GitSnapshot {
    pub repository: RepositoryId,
    pub generation: i64,
    pub commit: Oid,
    pub policy: i64,
}

pub async fn register(client: &Client, path: &Path) -> Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 {
        return Err("Markdown approval exceeds byte limit".into());
    }
    let approval: Approval = serde_json::from_slice(&bytes)?;
    approval.validate()?;
    let (repository, reference) = match &approval.input {
        Input::GitRustdoc {
            repository_id,
            reference,
            ..
        } => (
            Some(repository_id.parse::<RepositoryId>()?.uuid()),
            Some(reference.as_bytes()),
        ),
        Input::DocsrsRustdoc { .. } => (None, None),
    };
    let source = approval.source_id.parse::<RepositoryId>()?.uuid();
    let policy = serde_json::to_value(&approval)?;
    if client
        .execute(
            "INSERT INTO git_catalog.markdown_approvals AS a
          (source_id, policy_version, repository_id, ref_name, policy)
         VALUES ($1,$2,$3,$4,$5)
         ON CONFLICT (source_id) DO UPDATE SET
           policy_version = excluded.policy_version, policy = excluded.policy,
           revoked = false, next_due = clock_timestamp()
         WHERE excluded.policy_version > a.policy_version
           AND a.policy -> 'input' = excluded.policy -> 'input'",
            &[
                &source,
                &approval.policy_version,
                &repository,
                &reference,
                &policy,
            ],
        )
        .await?
        != 1
    {
        return Err("Markdown approval requires a higher policy version for the same input".into());
    }
    Ok(())
}

pub async fn revoke(client: &Client, source: RepositoryId) -> Result<()> {
    if client
        .execute(
            "UPDATE git_catalog.markdown_approvals SET revoked = true WHERE source_id = $1",
            &[&source.uuid()],
        )
        .await?
        != 1
    {
        return Err("Markdown source unavailable".into());
    }
    Ok(())
}

pub async fn status(client: &Client, source: RepositoryId, retry: bool) -> Result<Value> {
    let id = source.uuid();
    if retry {
        client
            .execute(
                "UPDATE git_catalog.markdown_approvals SET next_due = clock_timestamp()
              WHERE source_id = $1 AND NOT revoked",
                &[&id],
            )
            .await?;
        client.execute(
            "DELETE FROM git_catalog.markdown_jobs j USING git_catalog.markdown_approvals a
              WHERE j.source_id = $1 AND j.source_id = a.source_id AND j.policy_version = a.policy_version
                AND NOT a.revoked AND j.state = 'failed'", &[&id],
        ).await?;
    }
    Ok(client.query_opt(
        "SELECT jsonb_build_object('source_id',a.source_id,'policy_version',a.policy_version::text,
            'revoked',a.revoked,'next_due',a.next_due,
            'jobs',coalesce((SELECT jsonb_agg(to_jsonb(j)) FROM (
                SELECT job_id, state, attempts, reason, generation_id::text, due, lease_until
                FROM git_catalog.markdown_jobs WHERE source_id = a.source_id
                ORDER BY job_id DESC LIMIT 10) j),'[]'))
         FROM git_catalog.markdown_approvals a WHERE a.source_id = $1",
        &[&id],
    ).await?.ok_or("Markdown source unavailable")?.get(0))
}

// Resolve the authorized Git tip once before joining jobs. Joining both readable
// views into the scheduling queries expands the same publication joins twice.
const GIT_TIPS: &str = "
    tips AS MATERIALIZED (
        SELECT a.source_id, g.repository_id, g.generation_id, g.policy_version,
               g.object_format, f.peeled_commit
        FROM git_catalog.markdown_approvals a
        JOIN git_catalog.exploration_generations g ON g.repository_id = a.repository_id
        JOIN git_catalog.generation_refs f ON (f.repository_id, f.generation_id) = (g.repository_id, g.generation_id)
        WHERE NOT a.revoked AND g.is_active AND f.ref_name = a.ref_name
          AND f.peeled_commit IS NOT NULL
    )";

/// Tick under the existing scheduler owner's connection.
pub async fn schedule(client: &Client) -> Result<()> {
    client.execute(&format!(
        "WITH {GIT_TIPS}
         UPDATE git_catalog.markdown_jobs j SET state = 'cancelled', reason = 'superseded', lease_until = NULL
         FROM git_catalog.markdown_approvals a LEFT JOIN tips tip USING (source_id)
         WHERE j.source_id = a.source_id AND j.state IN ('queued','running')
           AND (a.revoked OR j.policy_version <> a.policy_version
             OR (j.generation_id, j.commit_oid, j.git_policy_version)
                   IS DISTINCT FROM (tip.generation_id, tip.peeled_commit, tip.policy_version))"
    ), &[]).await?;
    client.execute(
        "UPDATE git_catalog.markdown_jobs SET state = CASE WHEN attempts < $1 THEN 'queued' ELSE 'failed' END,
              reason = 'lease_expired', due = clock_timestamp() + interval '1 minute', lease_until = NULL
         WHERE state = 'running' AND lease_until <= clock_timestamp()", &[&RETRIES],
    ).await?;
    let rows = client
        .query(
            &format!(
                "WITH {GIT_TIPS}
         SELECT a.source_id, a.policy_version, tip.repository_id, tip.generation_id,
            tip.peeled_commit, tip.policy_version AS git_policy
         FROM git_catalog.markdown_approvals a LEFT JOIN tips tip USING (source_id)
         WHERE NOT a.revoked AND a.next_due <= clock_timestamp()
            AND (a.repository_id IS NULL OR tip.generation_id IS NOT NULL)
            AND NOT EXISTS (SELECT FROM git_catalog.markdown_jobs j WHERE j.source_id = a.source_id
            AND (j.state IN ('queued','running') OR
              (a.repository_id IS NOT NULL AND j.policy_version = a.policy_version
                AND j.generation_id = tip.generation_id AND j.state IN ('succeeded','failed'))
              OR (a.repository_id IS NULL AND j.policy_version = a.policy_version
                  AND j.state = 'failed' AND j.reason = 'lease_expired'
                  AND j.due > clock_timestamp() - interval '1 day')))
         ORDER BY a.next_due, a.source_id LIMIT 32"
            ),
            &[],
        )
        .await?;
    for row in rows {
        let id: Uuid = row.get("source_id");
        let policy: i64 = row.get("policy_version");
        let repo: Option<Uuid> = row.get("repository_id");
        let generation: Option<i64> = row.get("generation_id");
        let commit: Option<&[u8]> = row.get("peeled_commit");
        let git_policy: Option<i64> = row.get("git_policy");
        // Enrollment may be superseded concurrently. Claim rechecks the current policy.
        client.execute(
            "INSERT INTO git_catalog.markdown_jobs
             (job_id,source_id,policy_version,repository_id,generation_id,commit_oid,git_policy_version,state)
             VALUES ($1,$2,$3,$4,$5,$6,$7,'queued') ON CONFLICT DO NOTHING",
            &[&Uuid::now_v7(), &id, &policy, &repo, &generation, &commit, &git_policy],
        ).await?;
    }
    Ok(())
}

pub async fn claim(client: &Client, git: bool) -> Result<Option<Job>> {
    let row = client
        .query_opt(
            &format!(
                "WITH {GIT_TIPS}, picked AS (
             SELECT j.job_id FROM git_catalog.markdown_jobs j
             JOIN git_catalog.markdown_approvals a USING (source_id, policy_version)
             LEFT JOIN tips tip USING (source_id)
             WHERE j.state = 'queued' AND j.due <= clock_timestamp() AND NOT a.revoked
                AND (j.repository_id IS NOT NULL) = $1
                AND (j.generation_id, j.commit_oid, j.git_policy_version)
                     IS NOT DISTINCT FROM (tip.generation_id, tip.peeled_commit, tip.policy_version)
             ORDER BY j.due, j.job_id FOR UPDATE OF j SKIP LOCKED LIMIT 1
         ), claimed AS (
             UPDATE git_catalog.markdown_jobs j SET state = 'running', attempts = attempts + 1,
                 lease_until = clock_timestamp() + make_interval(secs => $2::integer)
             FROM picked WHERE j.job_id = picked.job_id RETURNING j.*
         )
         SELECT j.*, a.policy, tip.object_format,
            (SELECT m.etag FROM git_catalog.markdown_snapshots m
              WHERE m.source_id = j.source_id AND m.policy_version = j.policy_version
                AND m.retired_at IS NULL AND m.repository_id IS NULL) AS etag
         FROM claimed j JOIN git_catalog.markdown_approvals a USING (source_id, policy_version)
         LEFT JOIN tips tip USING (source_id)"
            ),
            &[&git, &LEASE_SECONDS],
        )
        .await?;
    row.map(read_job).transpose()
}

pub async fn active_job(client: &Client, id: Uuid, attempt: i32) -> Result<Option<Job>> {
    let row = client
        .query_opt(
            &format!(
                "WITH {GIT_TIPS}
         SELECT j.*, a.policy, tip.object_format, NULL::text AS etag
         FROM git_catalog.markdown_jobs j
         JOIN git_catalog.markdown_approvals a USING (source_id, policy_version)
         JOIN tips tip ON tip.source_id = j.source_id AND tip.generation_id = j.generation_id
             AND tip.peeled_commit = j.commit_oid AND tip.policy_version = j.git_policy_version
         WHERE j.job_id = $1 AND j.attempts = $2 AND NOT a.revoked AND j.state = 'running'
           AND j.lease_until > clock_timestamp()"
            ),
            &[&id, &attempt],
        )
        .await?;
    row.map(read_job).transpose()
}

fn read_job(row: Row) -> Result<Job> {
    let approval: Approval = serde_json::from_value(row.get("policy"))?;
    approval.validate()?;
    let git = if let Some(repo) = row.get::<_, Option<Uuid>>("repository_id") {
        let format: ObjectFormat = row.get::<_, &str>("object_format").parse()?;
        Some(GitSnapshot {
            repository: RepositoryId::new(repo)?,
            generation: row.get("generation_id"),
            commit: Oid::from_bytes(format, row.get("commit_oid"))?,
            policy: row.get("git_policy_version"),
        })
    } else {
        None
    };
    Ok(Job {
        id: row.get("job_id"),
        attempt: row.get("attempts"),
        approval,
        git,
        etag: row.get("etag"),
    })
}

async fn recheck(tx: &Transaction<'_>, job: &Job) -> Result<bool> {
    let source = job.approval.source_id.parse::<RepositoryId>()?.uuid();
    if tx
        .query_opt(
            "SELECT source_id FROM git_catalog.markdown_approvals
           WHERE source_id = $1 AND policy_version = $2 AND NOT revoked FOR UPDATE",
            &[&source, &job.approval.policy_version],
        )
        .await?
        .is_none()
    {
        return Ok(false);
    }
    Ok(tx
        .query_opt(
            &format!(
                "WITH {GIT_TIPS}
         SELECT j.job_id FROM git_catalog.markdown_jobs j
         JOIN git_catalog.markdown_approvals a USING (source_id, policy_version)
         LEFT JOIN tips tip USING (source_id)
         WHERE j.job_id = $1 AND j.attempts = $2 AND j.state = 'running'
           AND j.lease_until > clock_timestamp()
           AND (j.generation_id, j.commit_oid, j.git_policy_version)
               IS NOT DISTINCT FROM (tip.generation_id, tip.peeled_commit, tip.policy_version)
         FOR UPDATE OF j"
            ),
            &[&job.id, &job.attempt],
        )
        .await?
        .is_some())
}

pub async fn finish(client: &mut Client, job: &Job, status: Outcome<'_>) -> Result<bool> {
    let _fence = if let Some(git) = &job.git {
        Some(filesystem_fence(git.repository, LockScope::Repository, LockMode::Shared).await?)
    } else {
        None
    };
    let tx = client.transaction().await?;
    if !recheck(&tx, job).await? {
        return Ok(false);
    }
    let source = job.approval.source_id.parse::<RepositoryId>()?.uuid();
    let (state, reason, delay) = match status {
        Outcome::Publish {
            artifact,
            docs,
            etag,
        } => {
            publish(&tx, job, source, artifact, docs, etag).await?;
            ("succeeded", None, 0)
        }
        Outcome::Unchanged => ("succeeded", None, 0),
        Outcome::Failure { reason, retry } => match retry.filter(|_| job.attempt < RETRIES) {
            Some(delay) => (
                "queued",
                Some(reason),
                delay.as_secs().clamp(60, 86_400) as i32,
            ),
            None => ("failed", Some(reason), 0),
        },
    };
    if tx
        .execute(
            "UPDATE git_catalog.markdown_jobs SET state = $2, reason = $3, lease_until = NULL,
             due = clock_timestamp() + make_interval(secs => $4::integer)
         WHERE job_id = $1 AND lease_until > clock_timestamp()",
            &[&job.id, &state, &reason, &delay],
        )
        .await?
        != 1
    {
        return Ok(false);
    }
    if job.git.is_none() && state != "queued" {
        tx.execute(
            "UPDATE git_catalog.markdown_approvals SET next_due = clock_timestamp() + interval '1 day'
              WHERE source_id = $1", &[&source],
        ).await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub enum Outcome<'a> {
    Publish {
        artifact: &'a str,
        docs: &'a [Document],
        etag: Option<&'a str>,
    },
    Unchanged,
    Failure {
        reason: &'static str,
        retry: Option<Duration>,
    },
}

async fn publish(
    tx: &Transaction<'_>,
    job: &Job,
    source: Uuid,
    artifact: &str,
    docs: &[Document],
    etag: Option<&str>,
) -> Result<()> {
    let grants = job.approval.validate()?;
    let byte_count: usize = docs
        .iter()
        .map(|d| {
            d.markdown.len() + d.path.len() + d.source_path.len() + d.item.len() + d.kind.len() + 32
        })
        .sum();
    if byte_count > SNAPSHOT_BYTES
        || docs.len() > DOCUMENTS
        || docs.iter().any(|document| {
            !document.valid() || !grants.allows(&document.source_path, &document.path)
        })
        || docs.windows(2).any(|docs| docs[0].path >= docs[1].path)
    {
        return Err("invalid approved Markdown projection".into());
    }
    let (repo, generation, commit, git_policy) = if let Some(git) = &job.git {
        (
            Some(git.repository.uuid()),
            Some(git.generation),
            Some(git.commit.bytes()),
            Some(git.policy),
        )
    } else {
        (None, None, None, None)
    };
    let snapshot = digest(&serde_json::to_vec(&json!([
        source,
        job.approval.policy_version,
        generation,
        commit,
        git_policy,
        docs,
    ]))?);
    tx.query_one("SELECT pg_advisory_xact_lock(-4::bigint)", &[])
        .await?;
    let (count, existing): (i64, bool) = {
        let row = tx.query_one(
            "SELECT coalesce(sum(byte_count),0)::bigint, coalesce(bool_or(source_id = $1 AND digest = $2),false)
             FROM git_catalog.markdown_snapshots",
            &[&source, &snapshot],
        ).await?;
        (row.get(0), row.get(1))
    };
    if !existing && count + byte_count as i64 > QUOTA {
        return Err("Markdown storage quota exceeded".into());
    }
    if repo.is_none() {
        tx.execute(
            "UPDATE git_catalog.markdown_snapshots SET retired_at = clock_timestamp()
             WHERE source_id = $1 AND digest <> $2 AND retired_at IS NULL",
            &[&source, &snapshot],
        )
        .await?;
    }
    if existing {
        tx.execute(
            "UPDATE git_catalog.markdown_snapshots SET retired_at = NULL, etag = $3, artifact_digest = $4
             WHERE source_id = $1 AND digest = $2",
            &[&source, &snapshot, &etag, &artifact],
        )
        .await?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO git_catalog.markdown_snapshots
           (source_id,digest,artifact_digest,policy_version,repository_id,generation_id,commit_oid,git_policy_version,byte_count,etag)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
        &[&source,&snapshot,&artifact,&job.approval.policy_version,&repo,&generation,&commit,&git_policy,&(byte_count as i64),&etag],
    ).await?;
    for batch in docs.chunks(256) {
        let paths: Vec<_> = batch.iter().map(|doc| doc.path.as_str()).collect();
        let source_paths: Vec<_> = batch.iter().map(|doc| doc.source_path.as_str()).collect();
        let items: Vec<_> = batch.iter().map(|doc| doc.item.as_str()).collect();
        let kinds: Vec<_> = batch.iter().map(|doc| doc.kind.as_str()).collect();
        let lines: Vec<_> = batch.iter().map(|doc| doc.line as i64).collect();
        let bodies: Vec<_> = batch.iter().map(|doc| doc.markdown.as_bytes()).collect();
        tx.execute(
            "INSERT INTO git_catalog.markdown_documents
              SELECT $1,$2,p,s,i,k,l,b FROM unnest($3::text[],$4::text[],$5::text[],$6::text[],$7::bigint[],$8::bytea[])
              AS docs(p,s,i,k,l,b)", &[&source,&snapshot,&paths,&source_paths,&items,&kinds,&lines,&bodies],
        ).await?;
    }
    Ok(())
}

pub async fn prune(client: &Client) -> Result<()> {
    // Keep previous hosted snapshots through outstanding cursors and Git snapshots through generation retention.
    client
        .execute(
            "DELETE FROM git_catalog.markdown_snapshots m USING git_catalog.markdown_approvals a
          WHERE m.source_id = a.source_id AND (a.revoked OR m.policy_version <> a.policy_version
            OR m.retired_at <= clock_timestamp() - interval '24 hours'
            OR (m.repository_id IS NOT NULL AND NOT EXISTS (
                SELECT FROM git_catalog.exploration_generations g
                 WHERE (g.repository_id,g.generation_id,g.policy_version) =
                       (m.repository_id,m.generation_id,m.git_policy_version))))",
            &[],
        )
        .await?;
    client.execute(
        "DELETE FROM git_catalog.markdown_jobs j USING git_catalog.markdown_approvals a
          WHERE j.source_id = a.source_id AND j.state NOT IN ('queued','running')
            AND j.due < clock_timestamp() - interval '30 days'
            AND (j.repository_id IS NULL OR j.state = 'cancelled' OR a.revoked OR j.policy_version <> a.policy_version
              OR NOT EXISTS (SELECT FROM git_catalog.exploration_generations g WHERE g.is_active
                    AND (g.repository_id,g.generation_id,g.policy_version) =
                        (j.repository_id,j.generation_id,j.git_policy_version)))", &[],
    ).await?;
    Ok(())
}
