//! Operator-run synchronization and private candidate preparation.

mod approval;
mod candidate;
mod private;
pub mod publication;
pub mod supervisor;
mod upstream;
mod worker;

use std::{collections::BTreeMap, ffi::OsString, fs, path::Path, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{SecondsFormat, Utc};
use gh_docs_core::{
    Result,
    git::{ObjectFormat, Oid, RepositoryId},
};
use gh_docs_git::store::{self, LockMode, LockScope};
use gh_docs_storage::publication::{filesystem_fence, writer_advisory_fence};
use serde_json::{Value, json};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::{
    approval::Approval,
    candidate::{Baseline, Verified},
    upstream::{TransferError, Upstream},
};
pub use upstream::Fixture;

pub async fn register(client: &Client, file: &Path) -> Result<()> {
    approval::register(client, file).await
}

pub async fn revoke(client: &Client, source: RepositoryId) -> Result<()> {
    client
        .query_one("SELECT git_catalog.revoke_source($1)", &[&source.uuid()])
        .await?;
    println!("{}", json!({"source":source.uuid(),"outcome":"revoked"}));
    Ok(())
}

pub fn git_child(args: &[OsString], remote: bool) -> Result<()> {
    if remote {
        worker::helper()
    } else {
        worker::launch(args)
    }
}

pub async fn reconcile(
    client: &Client,
    repository: RepositoryId,
    fixture: Option<Fixture>,
    mut lifeline: tokio::net::UnixStream,
) -> Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        return Err("could not establish Git process supervision".into());
    }
    store::initialize()?;
    store::ensure_lock(repository, LockScope::Coordinator)?;
    store::ensure_lock(repository, LockScope::Repository)?;
    let _fence =
        match filesystem_fence(repository, LockScope::Coordinator, LockMode::Exclusive).await {
            Ok(fence) => fence,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut) =>
            {
                println!("{}", json!({"outcome":"busy","reason":"repository_writer"}));
                return Err("repository synchronization is in progress".into());
            }
            Err(error) => return Err(error),
        };
    let Some(slot) = private::Slot::acquire()? else {
        println!("{}", json!({"outcome":"busy","reason":"global_limit"}));
        return Err("all synchronization slots are in use".into());
    };
    let approval = approval::load(client, repository).await?;
    writer_advisory_fence(client, repository, LockScope::Coordinator).await?;
    let (_, source, format) = approval.validate()?;
    let job: Uuid = client
        .query_one(
            "SELECT git_catalog.start_reconcile($1,$2,$3)",
            &[&repository.uuid(), &source, &approval.version],
        )
        .await?
        .get(0);
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(approval.limits.deadline_seconds);
    let mut attempts = 1_i32;
    let scratch = private::Scratch(slot.root.join(job.to_string()));
    let result = {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let run = prepare_and_seal(
            client,
            &slot,
            &scratch.0,
            repository,
            source,
            format,
            &approval,
            job,
            fixture,
            deadline,
            &mut attempts,
        );
        tokio::select! {
            result = run => result,
            _ = tokio::time::sleep_until(deadline) => Err(TransferError::permanent("deadline")),
            _ = tokio::signal::ctrl_c() => Err(TransferError::permanent("cancelled")),
            _ = term.recv() => Err(TransferError::permanent("cancelled")),
            _ = supervisor::until_lost(&mut lifeline) => Err(TransferError::permanent("cancelled")),
            _ = disconnect(client) => Err(TransferError::permanent("database_unavailable")),
        }
    };
    match result {
        Ok(report) => {
            println!("{report}");
            if cleanup(client, repository).await.is_err() {
                eprintln!("private artifact cleanup pending");
            }
            Ok(())
        }
        Err(error) => {
            // A commit may have completed even if its response did not arrive.
            // The SQL transition only affects a still-running job. The next
            // reconciliation consults the catalog before touching its bytes.
            let recorded = client
                .query_one(
                    "SELECT git_catalog.fail_reconcile($1,$2,$3)",
                    &[&job, &error.reason, &attempts],
                )
                .await
                .is_ok_and(|row| row.get(0));
            println!(
                "{}",
                json!({"job":job,"outcome":"failed","reason":error.reason,"incident_recorded":recorded})
            );
            if recorded {
                if matches!(
                    error.reason,
                    "candidate_invalid"
                        | "git_worker_failed"
                        | "invalid_git_protocol"
                        | "upstream_unavailable"
                ) && retain_failure_sample(client, &slot, job, &scratch.0)
                    .await
                    .is_err()
                {
                    eprintln!("private incident sample unavailable");
                }
                let _ = cleanup(client, repository).await;
            }
            Err(error.reason.into())
        }
    }
}

async fn retain_failure_sample(
    client: &Client,
    slot: &private::Slot,
    job: Uuid,
    scratch: &Path,
) -> Result<()> {
    let sample = scratch.join("upstream-prefix");
    if !sample.is_file() {
        return Ok(());
    }
    let outcome: String = client
        .query_one(
            "SELECT state FROM git_catalog.sync_jobs WHERE job_id=$1",
            &[&job],
        )
        .await?
        .get(0);
    let target = private::incoming(job);
    if outcome != "failed" || target.exists() {
        return Ok(());
    }
    let _quota = private::quota_lock()?;
    slot.check_space(1024 * 1024)?;
    fs::create_dir(&target)?;
    candidate::copy_regular(
        &sample,
        &target.join("upstream-prefix"),
        upstream::SAMPLE_LIMIT as u64,
        true,
    )?;
    fs::File::open(target)?.sync_all()?;
    fs::File::open(private::INCOMING)?.sync_all()?;
    Ok(())
}

async fn disconnect(client: &Client) {
    while !client.is_closed() {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn cleanup(client: &Client, repo: RepositoryId) -> Result<()> {
    let rows = client
        .query(
            "SELECT j.job_id, p.state, j.outcome FROM git_catalog.sync_jobs j
         LEFT JOIN git_catalog.private_candidates p USING (job_id)
         WHERE j.repository_id=$1 AND j.source_id IS NOT NULL AND j.cleaned_at IS NULL
           AND j.state IN ('succeeded','failed') AND p.state IS DISTINCT FROM 'usable'
         ORDER BY j.finished_at,j.job_id LIMIT 128",
            &[&repo.uuid()],
        )
        .await?;
    let _quota = private::quota_lock()?;
    for row in rows {
        let job = row.get(0);
        let state: Option<&str> = row.get(1);
        let outcome: Option<&str> = row.get(2);
        let path = private::incoming(job);
        if state == Some("retired") || outcome == Some("no_change") {
            private::remove(&path)?;
        } else {
            private::quarantine(&path, job)?;
        }
        client
            .query_one("SELECT git_catalog.confirm_private_cleanup($1)", &[&job])
            .await?;
    }
    Ok(())
}

async fn load_baseline(
    client: &Client,
    repo: RepositoryId,
    source: Uuid,
    version: i64,
) -> Result<Option<Baseline>> {
    Ok(client.query_opt(
        "SELECT p.job_id, p.generation_id, p.pack_digest, g.manifest_digest
         FROM git_catalog.private_candidates p JOIN git_catalog.repository_generations g USING (repository_id,generation_id)
         WHERE p.repository_id=$1 AND g.source_id=$2 AND g.policy_version=$3 AND p.state='usable'
         ORDER BY p.generation_id DESC LIMIT 1",
        &[&repo.uuid(), &source, &version],
    ).await?.map(|row| Baseline {
        directory: private::incoming(row.get(0)), generation: row.get(1), pack: row.get(2), observed: row.get(3),
    }))
}

#[allow(clippy::too_many_arguments)]
async fn prepare_and_seal(
    db: &Client,
    slot: &private::Slot,
    scratch: &Path,
    repo: RepositoryId,
    source: Uuid,
    format: ObjectFormat,
    approval: &Approval,
    job: Uuid,
    fixture: Option<Fixture>,
    deadline: tokio::time::Instant,
    attempts: &mut i32,
) -> std::result::Result<Value, TransferError> {
    let state = |_| TransferError::permanent("candidate_state_unavailable");
    let storage = |_| TransferError::permanent("staging_unavailable");
    cleanup(db, repo).await.map_err(state)?;
    {
        let _quota = private::quota_lock().map_err(state)?;
        slot.check_space(slot.capacity * private::JOB_RESERVATION)
            .map_err(|_| TransferError::permanent("storage_quota"))?;
    }
    fs::create_dir(scratch).map_err(storage)?;
    let mut baseline = load_baseline(db, repo, source, approval.version)
        .await
        .map_err(state)?;
    let mut corrupt = false;
    let mut upstream = Upstream::new(
        db,
        job,
        approval,
        fixture.clone(),
        scratch.join("upstream-prefix"),
    );
    let mut attempt = scratch.join("attempt");
    fs::create_dir(&attempt).map_err(storage)?;
    if let Some(previous) = &baseline
        && let Err(error) = candidate::seed(&attempt, previous, approval, format).await
    {
        if !candidate::baseline_integrity_error(error.as_ref()) {
            return Err(TransferError::permanent("baseline_revalidation"));
        }
        db.query_one(
            "SELECT git_catalog.flag_corrupt_candidate($1,$2)",
            &[&repo.uuid(), &previous.generation],
        )
        .await
        .map_err(|_| TransferError::permanent("baseline_state_unavailable"))?;
        cleanup(db, repo).await.map_err(state)?;
        private::remove(&attempt).map_err(state)?;
        fs::create_dir(&attempt).map_err(storage)?;
        baseline = None;
        corrupt = true;
    }
    let verified = match candidate::acquire(
        &mut upstream,
        &attempt,
        approval,
        format,
        baseline.is_some(),
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            let jitter = Duration::from_millis(u64::from(job.as_bytes()[15]) * 3);
            let delay = error.retry.and_then(|delay| delay.checked_add(jitter));
            let Some(delay) = delay.filter(|delay| {
                !corrupt
                    && *delay <= deadline.saturating_duration_since(tokio::time::Instant::now())
            }) else {
                return Err(error);
            };
            tokio::time::sleep(delay).await;
            *attempts = 2;
            attempt = scratch.join("retry");
            private::remove(&scratch.join("attempt")).map_err(state)?;
            fs::create_dir(&attempt).map_err(storage)?;
            if let Some(previous) = &baseline {
                candidate::seed(&attempt, previous, approval, format)
                    .await
                    .map_err(|_| TransferError::permanent("baseline_revalidation"))?;
            }
            candidate::acquire(
                &mut upstream,
                &attempt,
                approval,
                format,
                baseline.is_some(),
            )
            .await?
        }
    };

    let observed = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let content = json!([
        repo.to_string(),
        source.to_string(),
        format.name(),
        approval.version.to_string(),
        STANDARD.encode(verified.default.bytes()),
        verified.snapshot
    ]);
    let manifest = gh_docs_core::git_manifest::encode(
        content,
        gh_docs_core::git_manifest::Check::HttpsGit,
        &observed,
    );
    let logical = manifest.logical;
    let observation = manifest.observed;
    let previous = baseline.as_ref().map(|previous| previous.generation);
    let changes = ref_changes(db, repo, previous, &attempt, &verified, format)
        .await
        .map_err(state)?;
    let proof = json!({
        "source":source,"policy_version":approval.version,"coverage":"full_closure","observed_at":observed,
        "objects":verified.objects,"inflated_bytes":verified.inflated,"response_bytes":upstream.received(),
        "baseline_generation":previous.map(|id| id.to_string()),"baseline_corrupt":corrupt,
        "changes":changes,"logical_digest":hex(&logical),"pack_digest":hex(&verified.pack)
    });
    let target = private::incoming(job);
    let next: i64 = db
        .query_one(
            "SELECT next_generation FROM git_catalog.repositories WHERE repository_id=$1",
            &[&repo.uuid()],
        )
        .await
        .map_err(|_| TransferError::permanent("candidate_state_unavailable"))?
        .get(0);
    store::ensure_lock(repo, LockScope::Generation(next)).map_err(storage)?;
    {
        let _quota = private::quota_lock().map_err(state)?;
        slot.check_space(private::JOB_RESERVATION)
            .map_err(|_| TransferError::permanent("storage_quota"))?;
        candidate::install(
            &verified,
            &target,
            manifest.json.as_bytes(),
            approval,
            format,
        )
        .map_err(state)?;
    }
    if let Some(fixture) = &fixture {
        fixture.terminate_at("sealed");
    }
    let generation: Option<i64> = db
        .query_one(
            "SELECT git_catalog.seal_candidate($1,$2,$3,$4,$5,$6,$7,$8,$9)",
            &[
                &job,
                &&logical[..],
                &&observation[..],
                &&verified.pack[..],
                &verified.default.bytes(),
                &verified.snapshot,
                &proof,
                &observed,
                attempts,
            ],
        )
        .await
        .map_err(|_| TransferError::permanent("seal_unconfirmed"))?
        .get(0);
    if let Some(fixture) = &fixture {
        fixture.terminate_at("committed");
    }
    if generation.is_none() {
        let _quota = private::quota_lock().map_err(state)?;
        private::remove(&target).map_err(state)?;
    }
    Ok(
        json!({"job":job,"outcome":if generation.is_some() {"verified"} else {"no_change"},
        "generation":generation.or(previous).map(|id| id.to_string()),"observation_digest":hex(&observation),
        "evidence":proof}),
    )
}

async fn ref_changes(
    db: &Client,
    repo: RepositoryId,
    baseline: Option<i64>,
    scratch: &Path,
    verified: &Verified,
    format: ObjectFormat,
) -> Result<Value> {
    let Some(baseline) = baseline else {
        return Ok(json!({"initial":true}));
    };
    let rows = db
        .query(
            "SELECT ref_name,target_oid FROM git_catalog.generation_refs
        WHERE repository_id=$1 AND generation_id=$2",
            &[&repo.uuid(), &baseline],
        )
        .await?;
    let mut old = BTreeMap::new();
    for row in rows {
        let name: Vec<u8> = row.get(0);
        let oid: Vec<u8> = row.get(1);
        old.insert(name, Oid::from_bytes(format, &oid)?);
    }
    let mut added = 0;
    let mut advanced = 0;
    let mut forced = 0;
    let mut tags = 0;
    for (index, entry) in verified
        .snapshot
        .as_array()
        .ok_or("invalid verified ref map")?
        .iter()
        .enumerate()
    {
        let name = STANDARD.decode(entry[0].as_str().ok_or("invalid verified ref")?)?;
        let now = Oid::parse(
            format,
            entry[2]
                .as_str()
                .ok_or("invalid verified target")?
                .as_bytes(),
        )?;
        let Some(previous) = old.remove(&name) else {
            added += 1;
            continue;
        };
        if now != previous {
            if name.starts_with(b"refs/tags/") {
                tags += 1;
            } else if verified.commits.contains(&previous) {
                let args = [
                    "for-each-ref".into(),
                    format!("--contains={previous}").into(),
                    "--format=%(refname)".into(),
                    format!("refs/gh-docs/fetch/{index}").into(),
                ];
                if worker::local(scratch, &verified.directory, &args, 128)
                    .await?
                    .is_empty()
                {
                    forced += 1;
                } else {
                    advanced += 1;
                }
            } else {
                forced += 1;
            }
        }
    }
    Ok(
        json!({"added":added,"deleted":old.len(),"advanced":advanced,"force_pushed":forced,"tags_moved":tags}),
    )
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("format to string");
    }
    output
}
