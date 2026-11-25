//! The private sync daemon owns publication and notification delivery.

use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};

use gh_docs_core::{Result, git::RepositoryId, required};
use gh_docs_storage::connect;
use gh_docs_sync::{
    publication::{self, Command as Publication},
    supervisor,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};
use tokio_postgres::{Client, Row};
use uuid::Uuid;

use crate::control::{self, Command, GenerationCommitted, Keys, Link, Outcome, Rejection};

struct Job {
    id: Uuid,
    repo: RepositoryId,
    expected: i64,
    attempts: i32,
}

impl Job {
    fn read(row: Row) -> Result<Self> {
        Ok(Self {
            id: row.get("job_id"),
            repo: RepositoryId::new(row.get("repository_id"))?,
            expected: row.get("expected_sequence"),
            attempts: row.get("attempts"),
        })
    }
}

struct Attempt {
    state: String,
    outcome: Option<String>,
    retry_after: Option<i64>,
    generation: Option<i64>,
}

pub async fn run() -> Result<()> {
    let roster = match std::env::var("GH_DOCS_SYNC_ROSTER_FILE") {
        Ok(path) if !path.is_empty() => control::config::<Vec<String>>(Path::new(&path), 8 * 1024)?,
        Ok(_) | Err(std::env::VarError::NotPresent) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let roster = roster
        .iter()
        .map(|repo| repo.parse::<RepositoryId>().map(RepositoryId::uuid))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if roster.len() > 128 || roster.iter().collect::<HashSet<_>>().len() != roster.len() {
        return Err("invalid scheduler roster".into());
    }
    let concurrency = gh_docs_sync::concurrency()?;
    let (db, mut driver) = connect("gh_docs_sync").await?;
    if !db
        .query_one("SELECT pg_try_advisory_lock(-3::bigint)", &[])
        .await?
        .get::<_, bool>(0)
    {
        return Err("scheduler is already owned".into());
    }

    let receiver_keys = Keys::load(Link::FeedSync)?;
    let sender_keys = Keys::load(Link::SyncServe)?;
    if receiver_keys.overlaps(&sender_keys) {
        return Err("control links require independent keys".into());
    }
    let receiver =
        TcpListener::bind(control::address(&required("GH_DOCS_SYNC_CONTROL_BIND")?)?).await?;
    let sender = control::Client::new(
        control::address(&required("GH_DOCS_SYNC_SERVE_ADDR")?)?,
        Link::SyncServe,
        sender_keys,
    );
    db.query_one("SELECT git_catalog.configure_schedule($1)", &[&roster])
        .await?;
    let db = Arc::new(db);
    let (stop, signal) = watch::channel(false);
    let mut workers = JoinSet::new();
    let mut active = HashSet::new();
    let database = db.clone();
    let inbox = control::serve(receiver, Link::FeedSync, receiver_keys, move |command| {
        accept(database.clone(), command)
    });
    let health = crate::health(crate::health_listener().await?);
    let result = tokio::select! {
        outcome = schedule(&db, &roster, concurrency, &mut workers, &mut active, signal) => outcome,
        outcome = inbox => outcome,
        outcome = deliver(db.clone(), sender) => outcome,
        outcome = health => outcome.map_err(Into::into),
        outcome = control::shutdown() => outcome.map_err(Into::into),
        outcome = watch_owner(&db) => outcome,
        _ = &mut driver => Err("scheduler database ownership lost".into()),
    };
    let _ = stop.send(true);
    while workers.join_next().await.is_some() {}
    driver.abort();
    result
}

async fn watch_owner(db: &Client) -> Result<()> {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        tokio::time::timeout(Duration::from_secs(6), db.simple_query("SELECT 1")).await??;
    }
}

async fn accept(db: Arc<Client>, command: Command) -> std::result::Result<Outcome, Rejection> {
    let Command::Sync(event) = command else {
        return Err(Rejection::Conflict);
    };
    let observed = i64::try_from(event.observed_at).map_err(|_| Rejection::Conflict)?;
    if db
        .query_one(
            "SELECT git_catalog.enqueue_hint($1,$2,$3,$4)",
            &[
                &event.event_id,
                &event.repository_id,
                &event.observation_digest,
                &observed,
            ],
        )
        .await
        .map_err(|_| Rejection::Unavailable)?
        .get(0)
    {
        control::fault("after_hint_commit");
        Ok(Outcome::Accepted)
    } else {
        Err(Rejection::Conflict)
    }
}

async fn schedule(
    db: &Client,
    roster: &[Uuid],
    concurrency: usize,
    workers: &mut JoinSet<Result<Job>>,
    active: &mut HashSet<Uuid>,
    stop: watch::Receiver<bool>,
) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut periodic = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = periodic.tick() => {
                db.query_one("SELECT git_catalog.configure_schedule($1)", &[&roster]).await?;
                db.query_one("SELECT git_catalog.schedule_periodic($1)", &[&86_400_i32]).await?;
            }
            _ = tick.tick() => {
                while active.len() < concurrency {
                    let excluded: Vec<_> = active.iter().copied().collect();
                    let Some(row) = db.query_opt("SELECT * FROM git_catalog.claim_scheduled($1)", &[&excluded]).await?
                    else { break; };
                    let job = Job::read(row)?;
                    if stale(db, &job).await? {
                        finish(db, &job, "stale_sequence", 0).await?;
                        continue;
                    }
                    if let Some(attempt) = attempt(db, &job).await? {
                        settle_attempt(db, &job, Some(attempt)).await?;
                        continue;
                    }
                    active.insert(job.id);
                    workers.spawn(fetch(job, stop.clone()));
                }
            }
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                let job = result??;
                active.remove(&job.id);
                if stale(db, &job).await? {
                    finish(db, &job, "stale_sequence", 0).await?;
                } else {
                    let completed = attempt(db, &job).await?;
                    if completed.as_ref().is_some_and(|attempt| attempt.state == "succeeded") {
                        control::fault("after_fetch_commit");
                    }
                    settle_attempt(db, &job, completed).await?;
                }
            }
        }
    }
}

async fn settle_attempt(db: &Client, job: &Job, attempt: Option<Attempt>) -> Result<()> {
    match attempt {
        Some(attempt) if attempt.state == "succeeded" => publish(db, job, attempt).await,
        Some(attempt) if attempt.state == "failed" => {
            let reason = attempt.outcome.as_deref().unwrap_or("fetch_failed");
            let retry = attempt
                .retry_after
                .map(|delay| delay.max(backoff(job.attempts)))
                .unwrap_or(0);
            let retry = if matches!(reason, "interrupted" | "cancelled" | "database_unavailable") {
                retry.max(backoff(job.attempts))
            } else {
                retry
            };
            finish(db, job, reason, retry).await
        }
        _ => finish(db, job, "fetch_interrupted_or_busy", backoff(job.attempts)).await,
    }
}

fn backoff(attempt: i32) -> i64 {
    let delay = 10_u64 * 2_u64.pow(attempt.clamp(1, 3) as u32 - 1);
    control::jitter(Duration::from_secs(delay)).as_secs() as i64
}

async fn fetch(job: Job, mut stop: watch::Receiver<bool>) -> Result<Job> {
    let args = [
        "--scheduled-reconcile".into(),
        "--repository".into(),
        job.repo.to_string(),
        "--job".into(),
        job.id.to_string(),
    ];
    let mut worker = supervisor::spawn(&args, true)?;
    tokio::select! {
        result = worker.wait() => { result?; }
        _ = stop.changed() => {
            worker.cancel();
            worker.wait().await?;
        }
    }
    Ok(job)
}

async fn stale(db: &Client, job: &Job) -> Result<bool> {
    Ok(db
        .query_one(
            "SELECT coalesce((SELECT publication_sequence FROM git_catalog.active_generations
        WHERE repository_id = $1), 0) <> $2",
            &[&job.repo.uuid(), &job.expected],
        )
        .await?
        .get(0))
}

async fn attempt(db: &Client, job: &Job) -> Result<Option<Attempt>> {
    Ok(db
        .query_opt(
            "SELECT a.state, a.outcome, a.scheduler_retry_after,
            coalesce(p.generation_id, CASE WHEN a.outcome = 'no_change'
                THEN (a.observation->>'baseline_generation')::bigint END)
         FROM git_catalog.scheduled_jobs j JOIN git_catalog.sync_jobs a ON a.job_id = j.attempt_id
         LEFT JOIN git_catalog.private_candidates p ON p.job_id = a.job_id
         WHERE j.job_id = $1",
            &[&job.id],
        )
        .await?
        .map(|row| Attempt {
            state: row.get(0),
            outcome: row.get(1),
            retry_after: row.get(2),
            generation: row.get(3),
        }))
}

async fn finish(db: &Client, job: &Job, outcome: &str, retry_seconds: i64) -> Result<()> {
    db.query_one(
        "SELECT git_catalog.finish_scheduled($1,$2,$3)",
        &[&job.id, &outcome, &retry_seconds],
    )
    .await?;
    Ok(())
}

async fn publication_command(db: &Client, job: &Job, command: Publication) -> Result<Value> {
    #[cfg(feature = "phase6-fixtures")]
    let fault = publication::Fault::fixture(control::fixture_fault());
    #[cfg(not(feature = "phase6-fixtures"))]
    let fault = publication::Fault::default();
    let result = publication::run(db, job.repo, command, &fault).await;
    db.query_one(
        "SELECT git_catalog.release_scheduler_fences($1)",
        &[&job.repo.uuid()],
    )
    .await?;
    result
}

async fn prepare(db: &Client, job: &Job, generation: i64) -> Result<()> {
    let pending: bool = db
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM git_catalog.publication_work WHERE repository_id = $1)",
            &[&job.repo.uuid()],
        )
        .await?
        .get(0);
    if pending {
        publication_command(db, job, Publication::Recover).await?;
    }
    let state = db
        .query_one(
            "SELECT state FROM git_catalog.repository_generations
        WHERE repository_id = $1 AND generation_id = $2",
            &[&job.repo.uuid(), &generation],
        )
        .await?;
    if state.get::<_, &str>(0) == "verified" {
        publication_command(db, job, Publication::Prepare(generation)).await?;
    }
    Ok(())
}

async fn publish(db: &Client, job: &Job, attempt: Attempt) -> Result<()> {
    let Some(generation) = attempt.generation else {
        return finish(db, job, "candidate_missing", 0).await;
    };
    let row = db.query_one("SELECT j.state = 'active' AND s.enrolled AND NOT s.paused,
        (SELECT g.last_publication_sequence FROM git_catalog.repository_generations g
            WHERE g.repository_id = j.repository_id AND g.generation_id = $2),
        EXISTS (SELECT 1 FROM git_catalog.private_candidates p
            WHERE p.repository_id = j.repository_id AND p.generation_id = $2 AND p.logical_digest = s.rollback_digest)
        FROM git_catalog.scheduled_jobs j JOIN git_catalog.sync_schedule s USING (repository_id)
        WHERE j.job_id = $1", &[&job.id, &generation]).await?;
    if !row.get::<_, bool>(0) {
        return finish(db, job, "paused", 0).await;
    }
    if row.get::<_, Option<i64>>(1).is_some() || row.get::<_, bool>(2) {
        return finish(db, job, "no_change", 0).await;
    }
    let result = async {
        prepare(db, job, generation).await?;
        publication_command(
            db,
            job,
            Publication::PublishScheduled(generation, job.expected, job.id),
        )
        .await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let Err(error) = result else {
        return Ok(());
    };
    #[cfg(feature = "phase6-fixtures")]
    if control::fixture().is_ok_and(|fixture| fixture.is_some()) {
        eprintln!("synthetic scheduled publication failed: {error:?}");
    }
    if let Some(error) = error.downcast_ref::<tokio_postgres::Error>()
        && let Some(error) = error.as_db_error()
    {
        eprintln!(
            "scheduled publication database error {}",
            error.code().code()
        );
    }
    if db.is_closed() {
        return Err("scheduler database ownership lost".into());
    }
    if stale(db, job).await? {
        // When the promotion did commit, the SQL transaction already finished the job.
        return finish(db, job, "stale_sequence", 0).await;
    }
    finish(db, job, "publication_retry", backoff(job.attempts)).await
}

async fn deliver(db: Arc<Client>, mut sender: control::Client) -> Result<()> {
    loop {
        let row = db.query_opt(
            "SELECT event_id, repository_id, generation_id, publication_sequence, delivery_attempts
             FROM git_catalog.notification_outbox
             WHERE delivered_at IS NULL AND next_attempt_at <= clock_timestamp()
             ORDER BY next_attempt_at, event_id LIMIT 1", &[]).await?;
        let Some(row) = row else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        let event = GenerationCommitted {
            event_id: row.get(0),
            repository_id: row.get(1),
            generation_id: row.get::<_, i64>(2).to_string(),
            publication_sequence: row.get::<_, i64>(3).to_string(),
        };
        let delay = if sender.notify_generation(&event).await.is_ok() {
            0
        } else {
            let attempts: i32 = row.get(4);
            let seconds = (2_u64 * 2_u64.pow(attempts.min(8) as u32)).min(300);
            control::jitter(Duration::from_secs(seconds)).as_secs() as i32
        };
        db.query_one(
            "SELECT git_catalog.record_delivery($1,$2)",
            &[&event.event_id, &delay],
        )
        .await?;
    }
}

pub async fn operator(repo: RepositoryId, command: &str, expected: Option<i64>) -> Result<Value> {
    let (db, driver) = connect("gh_docs_sync").await?;
    if command != "--schedule-status" {
        let pause = command == "--pause";
        if !db
            .query_one(
                "SELECT git_catalog.change_schedule($1,$2,$3)",
                &[&repo.uuid(), &pause, &expected],
            )
            .await?
            .get::<_, bool>(0)
        {
            return Err("repository is not enrolled or publication sequence changed".into());
        }
    }
    let row = db
        .query_opt(
            "SELECT enrolled, paused, next_run_at::text FROM git_catalog.sync_schedule
        WHERE repository_id = $1",
            &[&repo.uuid()],
        )
        .await?;
    let jobs = db
        .query(
            "SELECT job_id, state, outcome, attempts, expected_sequence, next_attempt_at::text
        FROM git_catalog.scheduled_jobs WHERE repository_id = $1
        ORDER BY (state = 'finished'), job_id DESC LIMIT 12",
            &[&repo.uuid()],
        )
        .await?;
    let jobs: Vec<_> = jobs.into_iter().map(|job| json!({
        "job": job.get::<_, Uuid>(0), "state": job.get::<_, &str>(1), "outcome": job.get::<_, Option<&str>>(2),
        "attempts": job.get::<_, i32>(3), "expected_sequence": job.get::<_, Option<i64>>(4).map(|v| v.to_string()),
        "next_attempt_at": job.get::<_, &str>(5)
    })).collect();
    let status = row
        .map(|row| {
            json!({
                "repository": repo.to_string(), "enrolled": row.get::<_, bool>(0),
                "paused": row.get::<_, bool>(1), "next_run_at": row.get::<_, &str>(2), "jobs": jobs
            })
        })
        .unwrap_or_else(|| json!({"repository": repo.to_string(), "enrolled": false}));
    drop(db);
    driver.await?;
    Ok(status)
}
