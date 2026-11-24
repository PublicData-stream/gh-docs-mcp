//! Recovery and guarded physical reclamation for one repository.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File},
};

use chrono::{Duration, SecondsFormat, Utc};
use gh_docs_core::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Context, Generation, PackFile, args, odb, report};
use crate::{candidate, private, worker};

const RETIRE_BATCH: i64 = 32;
const MAX_PROTECTED: usize = 512;

#[derive(Deserialize, Serialize)]
struct Repack {
    old: Vec<PackFile>,
    new: Vec<PackFile>,
}

impl Context<'_> {
    pub(super) async fn recover(&self) -> Result<Value> {
        let outcome = self.recover_inner().await;
        if outcome.is_err() {
            self.incident(None, "canonical_recovery_blocked", true)
                .await?;
        }
        outcome
    }

    async fn recover_inner(&self) -> Result<Value> {
        self.db
            .query_one(
                "SELECT git_catalog.recover_reconcile_jobs($1)",
                &[&self.repo.id.uuid()],
            )
            .await?;
        if let Some(work) = self.work().await? {
            let fence = match work.generation {
                Some(id) => Some((id, self.generation_fence(id).await?)),
                None => None,
            };
            match (work.action.as_str(), work.generation) {
                ("prepare", Some(id)) => self.resume_prepare(&work, id).await?,
                ("retire" | "discard", Some(id)) => {
                    self.remove_generation_files(&self.generation(id).await?)?;
                    self.finish(work.id, true).await?;
                }
                ("repack", None) => {
                    let repack = serde_json::from_value(work.details)?;
                    let protected = self.protected().await?;
                    let fences = self.protected_fences(&protected).await?;
                    self.apply_repack(&repack, &protected).await?;
                    self.finish(work.id, true).await?;
                    self.release_fences(fences).await?;
                }
                _ => return Err("unknown publication work record".into()),
            }
            if let Some((id, guard)) = fence {
                self.unlock_generation(id).await?;
                drop(guard);
            }
        }
        // Staging is always private and must precede its SQL operation record.
        // A stage with no matching work has never mutated the canonical
        // store, or belongs to an already-completed operation.
        self.repo.remove_stage()?;
        self.verify_repository(&self.protected().await?, None)
            .await?;
        private::ensure_locks()?;
        crate::cleanup(self.db, self.repo.id).await?;
        self.db
            .query_one(
                "SELECT git_catalog.confirm_repository_health($1)",
                &[&self.repo.id.uuid()],
            )
            .await?;
        let unregistered = self.unregistered_private().await?;
        Ok(
            json!({"repository":self.repo.id.to_string(), "outcome":"recovered",
                  "unregistered_private_entries":unregistered}),
        )
    }

    async fn unregistered_private(&self) -> Result<usize> {
        let entries = odb::names(std::path::Path::new(private::INCOMING), 4096)?;
        let jobs: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry.to_str()?.parse::<uuid::Uuid>().ok())
            .collect();
        let count: i64 = self
            .db
            .query_one(
                "SELECT count(*) FROM git_catalog.sync_jobs WHERE job_id = ANY($1)",
                &[&jobs],
            )
            .await?
            .get(0);
        Ok(entries.len().saturating_sub(usize::try_from(count)?))
    }

    pub(super) async fn discard(&self, id: i64) -> Result<Value> {
        if self.work().await?.is_some() || odb::exists(&self.repo.stage)? {
            return Err("repository has pending publication work, run --recover".into());
        }
        let _guard = self.generation_fence(id).await?;
        let target = self.generation(id).await?;
        if target.state == "discarded" {
            return Ok(report(self.repo.id, Some(id), "discarded"));
        }
        if !matches!(target.state.as_str(), "ready" | "verified") {
            return Err("generation cannot be discarded".into());
        }
        self.retire(&target, "discard").await?;
        private::ensure_locks()?;
        crate::cleanup(self.db, self.repo.id).await?;
        Ok(report(self.repo.id, Some(id), "discarded"))
    }

    pub(super) async fn gc(&self) -> Result<Value> {
        self.recover().await?;
        let rows = self.db.query(
            "SELECT g.generation_id FROM git_catalog.repository_generations g
             WHERE g.repository_id = $1 AND g.state = 'retained'
              AND g.demoted_at <= clock_timestamp() - interval '24 hours'
              AND g.generation_id NOT IN (SELECT recent.generation_id FROM git_catalog.repository_generations recent
                  WHERE recent.repository_id = $1 AND recent.state = 'retained'
                  ORDER BY recent.last_publication_sequence DESC LIMIT 3)
             ORDER BY g.generation_id LIMIT $2",
            &[&self.repo.id.uuid(), &RETIRE_BATCH],
        ).await?;
        let mut retired = Vec::new();
        for row in rows {
            let id = row.get(0);
            let guard = self.generation_fence(id).await?;
            self.retire(&self.generation(id).await?, "retire").await?;
            self.unlock_generation(id).await?;
            drop(guard);
            retired.push(id.to_string());
        }
        let protected = self.protected().await?;
        let _protected_fences = self.protected_fences(&protected).await?;
        let mut removed = 0;
        if odb::exists(&self.repo.odb)? {
            self.verify_repository(&protected, None).await?;
            let repack = self.stage_repack(&protected).await?;
            let operation = self
                .begin(None, "repack", &serde_json::to_value(&repack)?)
                .await?;
            self.fault.at("after_repack_intent");
            removed = self.apply_repack(&repack, &protected).await?;
            self.finish(operation, true).await?;
            self.repo.remove_stage()?;
            self.db
                .query_one(
                    "SELECT git_catalog.confirm_repository_health($1)",
                    &[&self.repo.id.uuid()],
                )
                .await?;
        }
        Ok(
            json!({"repository":self.repo.id.to_string(), "outcome":"collected", "retired":retired,
                  "removed_pack_components":removed}),
        )
    }

    async fn retire(&self, target: &Generation, action: &str) -> Result<()> {
        self.repo.begin_stage()?;
        self.fault.at("before_retire");
        let operation = self.begin(Some(target.id), action, &json!({})).await?;
        self.fault.at("after_retire");
        // The SQL state is already unreadable. The operation record is kept
        // until both names have been detached and their parents flushed.
        self.remove_generation_files(target)?;
        self.fault.at("after_root_removal");
        self.finish(operation, true).await?;
        self.repo.remove_stage()
    }

    async fn protected(&self) -> Result<Vec<Generation>> {
        let rows = self
            .db
            .query(
                "SELECT generation_id FROM git_catalog.repository_generations
              WHERE repository_id = $1 AND state IN ('ready','active','retained')
              ORDER BY generation_id LIMIT 513",
                &[&self.repo.id.uuid()],
            )
            .await?;
        if rows.len() > MAX_PROTECTED {
            return Err("repository exceeds the protected generation budget".into());
        }
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            result.push(self.generation(row.get(0)).await?);
        }
        Ok(result)
    }

    async fn protected_fences(
        &self,
        generations: &[Generation],
    ) -> Result<Vec<(i64, gh_docs_git::store::Fence)>> {
        let mut fences = Vec::with_capacity(generations.len());
        for generation in generations {
            fences.push((generation.id, self.generation_fence(generation.id).await?));
        }
        Ok(fences)
    }

    async fn release_fences(&self, fences: Vec<(i64, gh_docs_git::store::Fence)>) -> Result<()> {
        for (id, guard) in fences.into_iter().rev() {
            self.unlock_generation(id).await?;
            drop(guard);
        }
        Ok(())
    }

    async fn verify_repository(
        &self,
        generations: &[Generation],
        replacement: Option<&[PackFile]>,
    ) -> Result<()> {
        let consistent: bool = self.db.query_one(
            "SELECT coalesce((SELECT g.state = 'active' AND g.last_publication_sequence = a.publication_sequence
                      AND EXISTS (SELECT 1 FROM git_catalog.notification_outbox e
                              WHERE e.repository_id = a.repository_id AND e.generation_id = a.generation_id
                                AND e.publication_sequence = a.publication_sequence)
                  FROM git_catalog.active_generations a
                  JOIN git_catalog.repository_generations g USING (repository_id, generation_id)
                  WHERE a.repository_id = $1),
                  NOT EXISTS (SELECT 1 FROM git_catalog.repository_generations
                              WHERE repository_id = $1 AND state = 'active'))",
            &[&self.repo.id.uuid()],
        ).await?.get(0);
        if !consistent {
            return Err("publication pointer and outbox are inconsistent".into());
        }
        self.verify_inventory(replacement).await?;
        let root_names: BTreeSet<_> = generations
            .iter()
            .map(|g| OsString::from(g.id.to_string()))
            .collect();
        let manifest_names = generations
            .iter()
            .map(|g| format!("{}.json", g.id).into())
            .collect();
        if odb::names(&self.repo.manifests, MAX_PROTECTED)? != manifest_names {
            return Err("unexpected canonical manifest inventory".into());
        }
        if !odb::exists(&self.repo.odb)? {
            if generations.is_empty() {
                return Ok(());
            }
            return Err("canonical object store is missing".into());
        }
        self.repo.validate_layout()?;
        if odb::names(&self.repo.odb.join("refs/gh-docs/retention"), MAX_PROTECTED)? != root_names {
            return Err("unexpected canonical retention inventory".into());
        }
        self.verify_native(None).await?;
        for generation in generations {
            self.verify_generation_files(generation)?;
        }
        self.verify_root_types(generations).await
    }

    async fn stage_repack(&self, protected: &[Generation]) -> Result<Repack> {
        let packs = self.repo.pack_dir();
        let names = odb::verify_pack_pairs(&packs, self.repo.format)?;
        let mut old = Vec::new();
        let mut bytes = 0_u64;
        for name in names {
            let path = packs.join(name);
            bytes = bytes.saturating_add(candidate::open_regular(&path)?.metadata()?.len());
            old.push(PackFile::inspect(&path, self.repo.format)?);
        }
        odb::maintenance_admission(&packs, bytes)?;
        self.repo.begin_stage()?;
        let scratch = self.repo.stage.join("repack");
        odb::init_odb(&scratch, self.repo.format)?;
        let output = scratch.join("objects/pack");
        for file in &old {
            odb::copy_file(
                &packs.join(&file.name),
                &output.join(&file.name),
                272 * crate::approval::MIB,
            )?;
            if !file.matches(&output, self.repo.format)? {
                return Err("copied maintenance pack digest mismatch".into());
            }
        }
        let retention = scratch.join("refs/gh-docs/retention");
        for generation in protected {
            let root_dir = retention.join(generation.id.to_string());
            fs::create_dir(&root_dir)?;
            for (index, root) in generation.roots.iter().enumerate() {
                odb::write_file(
                    &root_dir.join(index.to_string()),
                    format!("{}\n", root.target).as_bytes(),
                    0o644,
                )?;
            }
        }

        let cutoff = (Utc::now() - Duration::days(14)).to_rfc3339_opts(SecondsFormat::Secs, true);
        self.fault.at("before_repack");
        worker::local(
            &self.repo.stage,
            &scratch,
            &args(&[
                "-c",
                "pack.writeReverseIndex=false",
                "repack",
                "-n",
                "-d",
                "--cruft",
                &format!("--cruft-expiration={cutoff}"),
                "--max-pack-size=256m",
                "--max-cruft-size=256m",
                "--no-write-bitmap-index",
            ]),
            256 * 1024,
        )
        .await?;
        self.fault.at("after_repack");
        worker::local(
            &self.repo.stage,
            &scratch,
            &args(&["fsck", "--full", "--strict", "--no-dangling"]),
            256 * 1024,
        )
        .await?;
        let mut new = Vec::new();
        for name in odb::verify_pack_pairs(&output, self.repo.format)? {
            let path = output.join(name);
            new.push(PackFile::inspect(&path, self.repo.format)?);
            File::open(path)?.sync_all()?;
        }
        for path in [
            output,
            scratch.join("objects"),
            scratch,
            self.repo.stage.clone(),
        ] {
            File::open(path)?.sync_all()?;
        }
        new.sort_by_key(|file| !file.name.ends_with(".pack"));
        Ok(Repack { old, new })
    }

    async fn apply_repack(&self, repack: &Repack, protected: &[Generation]) -> Result<usize> {
        self.repo.validate_layout()?;
        let output = self.repo.stage.join("repack/objects/pack");
        let packs = self.repo.pack_dir();
        let old: BTreeMap<_, _> = repack
            .old
            .iter()
            .map(|file| (file.name.as_str(), file))
            .collect();
        let new: BTreeSet<_> = repack.new.iter().map(|file| file.name.as_str()).collect();
        if new.len() != repack.new.len() || old.len() != repack.old.len() {
            return Err("duplicate repack operation component".into());
        }
        for file in repack.old.iter().chain(&repack.new) {
            file.validate(self.repo.format)?;
        }
        for entry in odb::names(&packs, 4096)? {
            let name = entry.to_str().ok_or("invalid canonical pack filename")?;
            if !old.contains_key(name) && !new.contains(name) {
                return Err("unrecorded component in canonical repack".into());
            }
        }
        self.fault.at("before_repack_install");
        for file in &repack.new {
            file.install(&self.repo, &output, old.get(file.name.as_str()).copied())?;
        }
        self.fault.at("after_repack_install");
        for file in &repack.new {
            if !file.matches(&packs, self.repo.format)? {
                return Err("repack replacement failed its recorded proof".into());
            }
        }
        // Every replacement is durable and was checked in isolation with the
        // complete protected root set. Remove indexes before their packs.
        let mut obsolete: Vec<_> = repack
            .old
            .iter()
            .filter(|file| !new.contains(file.name.as_str()))
            .collect();
        obsolete.sort_by_key(|file| file.name.ends_with(".pack"));
        let mut removed = 0;
        self.fault.at("before_old_pack_removal");
        for file in obsolete {
            let path = packs.join(&file.name);
            if odb::exists(&path)? {
                if !file.matches(&packs, self.repo.format)? {
                    return Err("obsolete pack differs from the recorded proof".into());
                }
                fs::remove_file(&path)?;
                odb::sync_parent(&path)?;
                removed += 1;
                self.fault.at("during_old_pack_removal");
            }
        }
        self.fault.at("after_old_pack_removal");
        self.verify_repository(protected, Some(&repack.new)).await?;
        Ok(removed)
    }
}
