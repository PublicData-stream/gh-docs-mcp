//! Cross-store publication fences and the SELECT-only generation read boundary.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gh_docs_core::{
    Result,
    git::{ObjectFormat, Oid, RepositoryId},
};
use gh_docs_git::{
    reader::{Batch, Reachability, Reader},
    store::{self, Fence, LockMode, LockScope},
};
use tokio_postgres::{Client, IsolationLevel, Transaction};

use crate::{
    catalog::RefSnapshot,
    exploration::{self, Metadata},
};

const FENCE_WAIT: Duration = Duration::from_secs(2);

pub struct GenerationUnavailable;

impl GenerationUnavailable {
    const MESSAGE: &str = "generation unavailable";
}

impl std::fmt::Display for GenerationUnavailable {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(Self::MESSAGE)
    }
}

impl std::fmt::Debug for GenerationUnavailable {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(Self::MESSAGE, out)
    }
}

impl std::error::Error for GenerationUnavailable {}

pub async fn filesystem_fence(
    repository: RepositoryId,
    scope: LockScope,
    mode: LockMode,
) -> Result<Fence> {
    let deadline = Instant::now() + FENCE_WAIT;
    Ok(
        tokio::task::spawn_blocking(move || store::lock(repository, scope, mode, deadline))
            .await??,
    )
}

fn scope_sql(scope: LockScope) -> (&'static str, Option<i64>) {
    match scope {
        LockScope::Coordinator => ("coordinator", None),
        LockScope::Repository => ("repository", None),
        LockScope::Generation(generation) => ("generation", Some(generation)),
    }
}

/// Use a dedicated connection. Its session locks are released on disconnect.
/// Keep the returned independent filesystem guard across all protected work.
pub async fn writer_fence(client: &Client, repo: RepositoryId, scope: LockScope) -> Result<Fence> {
    let guard = filesystem_fence(repo, scope, LockMode::Exclusive).await?;
    writer_advisory_fence(client, repo, scope).await?;
    Ok(guard)
}

/// Pair an already-held exclusive filesystem fence with its database key.
pub async fn writer_advisory_fence(
    client: &Client,
    repo: RepositoryId,
    scope: LockScope,
) -> Result<()> {
    let (name, generation) = scope_sql(scope);
    let key: i64 = client
        .query_one(
            "SELECT advisory_key FROM git_catalog.lock_keys WHERE repository_id = $1
         AND scope = $2 AND generation_id IS NOT DISTINCT FROM $3",
            &[&repo.uuid(), &name, &generation],
        )
        .await?
        .get(0);
    if !client
        .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
        .await?
        .get::<_, bool>(0)
    {
        return Err("database publication fence busy".into());
    }
    Ok(())
}

/// Release a generation lock before acquiring a different generation group on
/// the same writer connection. Repository/coordinator locks live until close.
pub async fn release_generation(
    client: &Client,
    repo: RepositoryId,
    generation: i64,
) -> Result<()> {
    if !client
        .query_one(
            "SELECT pg_advisory_unlock(advisory_key) FROM git_catalog.lock_keys
         WHERE repository_id = $1 AND scope = 'generation' AND generation_id = $2",
            &[&repo.uuid(), &generation],
        )
        .await?
        .get::<_, bool>(0)
    {
        return Err("generation database fence not held".into());
    }
    Ok(())
}

async fn reader_key(
    tx: &Transaction<'_>,
    repo: RepositoryId,
    generation: Option<i64>,
) -> Result<()> {
    let key: i64 = tx
        .query_opt(
            "SELECT advisory_key FROM git_catalog.reader_lock_keys WHERE repository_id = $1
           AND generation_id IS NOT DISTINCT FROM $2",
            &[&repo.uuid(), &generation],
        )
        .await?
        .ok_or(GenerationUnavailable)?
        .get(0);
    if !tx
        .query_one("SELECT pg_try_advisory_xact_lock_shared($1)", &[&key])
        .await?
        .get::<_, bool>(0)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "database generation fence busy",
        )
        .into());
    }
    Ok(())
}

pub struct Generation {
    pub id: i64,
    pub sequence: i64,
    pub format: ObjectFormat,
    pub refs: Vec<RefSnapshot>,
    pub exploration: Option<Metadata>,
}

impl Generation {
    pub fn reachable(&self, reader: &Reader, commit: Oid) -> Result<Reachability> {
        Ok(reader.batch(|batch| self.reachable_with(batch, commit))?)
    }

    pub fn reachable_with(
        &self,
        batch: &mut Batch<'_>,
        commit: Oid,
    ) -> std::io::Result<Reachability> {
        let roots: Vec<_> = self
            .refs
            .iter()
            .filter_map(|entry| entry.peeled_commit)
            .collect();
        batch.reachable(&roots, commit)
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Select a readable generation and run bounded Git work under both filesystem
/// fences. Dropping the request cancels the detached owner, which still reaps
/// its worker before dropping its fences.
pub async fn read_generation<T: Send + 'static>(
    client: &mut Client,
    repo: RepositoryId,
    requested: Option<i64>,
    operation: impl FnOnce(Generation, &Reader) -> Result<T> + Send + 'static,
) -> Result<T> {
    read(client, repo, requested, false, operation).await
}

pub async fn read_exploration<T: Send + 'static>(
    client: &mut Client,
    repo: RepositoryId,
    requested: Option<i64>,
    operation: impl FnOnce(Generation, &Reader) -> Result<T> + Send + 'static,
) -> Result<T> {
    read(client, repo, requested, true, operation).await
}

async fn read<T: Send + 'static>(
    client: &mut Client,
    repo: RepositoryId,
    requested: Option<i64>,
    require_exploration: bool,
    operation: impl FnOnce(Generation, &Reader) -> Result<T> + Send + 'static,
) -> Result<T> {
    let repository = filesystem_fence(repo, LockScope::Repository, LockMode::Shared).await?;
    let tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::ReadCommitted)
        .read_only(true)
        .start()
        .await?;
    reader_key(&tx, repo, None).await?;
    let select = "SELECT generation_id, publication_sequence, object_format FROM git_catalog.readable_generations
                  WHERE repository_id = $1 AND (generation_id = $2 OR ($2 IS NULL AND is_active))";
    let id: i64 = tx
        .query_opt(select, &[&repo.uuid(), &requested])
        .await?
        .ok_or(GenerationUnavailable)?
        .get(0);
    let generation = filesystem_fence(repo, LockScope::Generation(id), LockMode::Shared).await?;
    reader_key(&tx, repo, Some(id)).await?;
    let row = tx
        .query_opt(select, &[&repo.uuid(), &requested])
        .await?
        .ok_or(GenerationUnavailable)?;
    if row.get::<_, i64>(0) != id {
        return Err("generation changed while acquiring reader fences".into());
    }
    let format: ObjectFormat = row.get::<_, &str>(2).parse()?;
    let sequence = row.get(1);
    let exploration = if require_exploration {
        Some(
            exploration::get(&tx, repo, Some(id))
                .await?
                .ok_or(GenerationUnavailable)?,
        )
    } else {
        None
    };
    let refs = tx
        .query(
            "SELECT ref_name, target_oid, target_type, peeled_commit FROM git_catalog.readable_refs
         WHERE repository_id = $1 AND generation_id = $2 ORDER BY ref_name LIMIT 1025",
            &[&repo.uuid(), &id],
        )
        .await?;
    if refs.is_empty() || refs.len() > 1024 {
        return Err("invalid generation ref snapshot".into());
    }
    let refs = refs
        .into_iter()
        .map(|row| -> Result<_> {
            Ok(RefSnapshot {
                name: gh_docs_core::git::RefName::new(row.get::<_, Vec<u8>>(0))?,
                target: Oid::from_bytes(format, row.get(1))?,
                kind: gh_docs_core::git::ObjectType::parse(row.get::<_, &str>(2).as_bytes())?,
                peeled_commit: row
                    .get::<_, Option<&[u8]>>(3)
                    .map(|bytes| Oid::from_bytes(format, bytes))
                    .transpose()?,
            })
        })
        .collect::<Result<_>>()?;
    let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let signal = cancellation.0.clone();
    let mut work = tokio::task::spawn_blocking(move || {
        let reader = Reader::fenced(repo, format, repository, generation, signal);
        operation(
            Generation {
                id,
                format,
                sequence,
                refs,
                exploration,
            },
            &reader,
        )
    });
    let result = tokio::select! {
        result = &mut work => result??,
        () = async {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if tx.client().is_closed() { break; }
            }
        } => {
            drop(cancellation);
            let _ = work.await;
            return Err("reader database connection closed".into());
        }
    };
    tx.commit().await?;
    Ok(result)
}
