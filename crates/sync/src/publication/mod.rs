//! Operator-run publication. PostgreSQL is the visibility boundary.

mod gc;
mod odb;

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File},
    io::Read,
    path::Path,
    process::Stdio,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use gh_docs_core::{
    Result,
    git::{ObjectType, Oid, RefName, RepositoryId},
    git_manifest::{self, Check, Manifest},
};
use gh_docs_git::store::{self, Fence, LockScope};
use gh_docs_storage::publication::{release_generation, writer_fence};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::{approval, candidate, private, worker};
use odb::{PackFile, Repository};

#[derive(Clone, Copy)]
pub enum Command {
    Prepare(i64),
    Publish(i64, i64),
    Rollback(i64, i64),
    Status(Option<i64>),
    Recover,
    Gc,
    Discard(i64),
}

impl Command {
    pub fn parse(flag: &str, args: &[String]) -> Result<Option<Self>> {
        let generation = |args: &[String]| match args {
            [selector, id] if selector == "--generation" => nonnegative(id),
            _ => Err("expected --generation ID".into()),
        };
        let command = match (flag, args) {
            ("--prepare", args) => Self::Prepare(generation(args)?),
            ("--discard", args) => Self::Discard(generation(args)?),
            ("--publish" | "--rollback", [selector, id, expect, sequence])
                if selector == "--generation" && expect == "--expect-sequence" =>
            {
                let id = nonnegative(id)?;
                let sequence = nonnegative(sequence)?;
                if flag == "--publish" {
                    Self::Publish(id, sequence)
                } else {
                    Self::Rollback(id, sequence)
                }
            }
            ("--status", []) => Self::Status(None),
            ("--status", [selector, sequence]) if selector == "--sequence" => {
                Self::Status(Some(nonnegative(sequence)?))
            }
            ("--recover", []) => Self::Recover,
            ("--gc", []) => Self::Gc,
            ("--publish" | "--rollback" | "--status" | "--recover" | "--gc", _) => {
                return Err("invalid publication arguments".into());
            }
            _ => return Ok(None),
        };
        Ok(Some(command))
    }
}

fn nonnegative(value: &str) -> Result<i64> {
    let number: i64 = value.parse()?;
    if number < 0 || number.to_string() != value {
        return Err("expected a canonical nonnegative BIGINT".into());
    }
    Ok(number)
}

#[derive(Default)]
pub struct Fault {
    #[cfg(feature = "fixtures")]
    point: Option<String>,
}

impl Fault {
    #[cfg(feature = "fixtures")]
    pub fn fixture(point: Option<String>) -> Self {
        Self { point }
    }

    fn at(&self, label: &str) {
        #[cfg(feature = "fixtures")]
        if self.point.as_deref() == Some(label) {
            // The fixture runs in a disposable container with synthetic data.
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        }
        let _ = label;
    }
}

pub async fn run(
    client: &Client,
    repo: RepositoryId,
    command: Command,
    fault: &Fault,
) -> Result<Value> {
    let format = client
        .query_one(
            "SELECT object_format FROM git_catalog.repositories WHERE repository_id = $1",
            &[&repo.uuid()],
        )
        .await?
        .get::<_, &str>(0)
        .parse()?;
    if let Command::Status(sequence) = command {
        return status(client, repo, sequence).await;
    }
    // The existing Git supervisor reaps command descendants before these
    // filesystem guards are dropped.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        return Err("could not establish publication process supervision".into());
    }
    store::canonical_root()?;
    store::initialize()?;
    let _coordinator = writer_fence(client, repo, LockScope::Coordinator).await?;
    let _repository = writer_fence(client, repo, LockScope::Repository).await?;
    client.batch_execute("SET synchronous_commit = on").await?;
    private::ensure_locks()?;
    let _admission = private::publication_lock()?;
    let repository = Repository::open(repo, format)?;
    let context = Context {
        db: client,
        repo: repository,
        fault,
    };
    match command {
        Command::Prepare(id) => context.prepare(id).await,
        Command::Publish(id, seq) => context.promote(id, seq, false).await,
        Command::Rollback(id, seq) => context.promote(id, seq, true).await,
        Command::Recover => context.recover().await,
        Command::Gc => context.gc().await,
        Command::Discard(id) => context.discard(id).await,
        Command::Status(_) => unreachable!("status uses no writer fences"),
    }
}

async fn status(client: &Client, repo: RepositoryId, wanted: Option<i64>) -> Result<Value> {
    let row = client
        .query_one(
            "SELECT a.generation_id, a.publication_sequence, b.reason_code, w.action,
                e.event_id, e.generation_id, e.publication_sequence, e.delivered_at IS NOT NULL
          FROM git_catalog.repositories r
          LEFT JOIN git_catalog.active_generations a ON a.repository_id = r.repository_id
          LEFT JOIN git_catalog.publication_blocks b ON b.repository_id = r.repository_id
          LEFT JOIN git_catalog.publication_work w ON w.repository_id = r.repository_id
          LEFT JOIN git_catalog.notification_outbox e ON e.repository_id = r.repository_id
              AND e.publication_sequence = coalesce($2, a.publication_sequence)
          WHERE r.repository_id = $1",
            &[&repo.uuid(), &wanted],
        )
        .await?;
    let event: Option<Uuid> = row.get(4);
    Ok(json!({
        "repository": repo.to_string(),
        "generation": row.get::<_, Option<i64>>(0).map(|id| id.to_string()),
        "sequence": row.get::<_, Option<i64>>(1).unwrap_or(0).to_string(),
        "blocked": row.get::<_, Option<&str>>(2),
        "pending_work": row.get::<_, Option<&str>>(3),
        "event": event.map(|id| json!({
            "id": id,
            "generation": row.get::<_, i64>(5).to_string(),
            "sequence": row.get::<_, i64>(6).to_string(),
            "delivered": row.get::<_, bool>(7)
        }))
    }))
}

struct Context<'a> {
    db: &'a Client,
    repo: Repository,
    fault: &'a Fault,
}

struct Generation {
    id: i64,
    state: String,
    roots: Vec<Root>,
    manifest: Manifest,
}

struct Root {
    target: Oid,
    kind: ObjectType,
    peeled: Oid,
}

#[derive(Deserialize, Serialize)]
struct Preparation {
    files: Vec<PackFile>,
}

struct Work {
    action: String,
    generation: Option<i64>,
    id: Uuid,
    details: Value,
}

impl Context<'_> {
    async fn generation(&self, id: i64) -> Result<Generation> {
        let row = self.db.query_one(
            "SELECT state, source_id, object_format, policy_version, default_ref, manifest_digest,
                    provenance->>'observed_at'
             FROM git_catalog.repository_generations WHERE repository_id = $1 AND generation_id = $2",
            &[&self.repo.id.uuid(), &id],
        ).await?;
        let object_format = self.repo.format;
        if row.get::<_, &str>(2) != object_format.name() {
            return Err("catalog generation object format mismatch".into());
        }
        let source: Uuid = row.get(1);
        let version: i64 = row.get(3);
        let default = RefName::new(
            row.get::<_, Option<Vec<u8>>>(4)
                .ok_or("generation default ref unavailable")?,
        )?;
        let observed: &str = row
            .get::<_, Option<&str>>(6)
            .ok_or("generation observation unavailable")?;
        let digest: &[u8] = row
            .get::<_, Option<&[u8]>>(5)
            .ok_or("generation manifest unavailable")?;
        let rows = self.db.query(
            "SELECT ref_name, target_oid, target_type, peeled_commit FROM git_catalog.generation_refs
             WHERE repository_id = $1 AND generation_id = $2 ORDER BY ref_name LIMIT 1025",
            &[&self.repo.id.uuid(), &id],
        ).await?;
        if rows.is_empty() || rows.len() > approval::MAX_REFS {
            return Err("generation ref snapshot exceeds its limits".into());
        }
        let mut roots = Vec::with_capacity(rows.len());
        let mut snapshot = Vec::with_capacity(rows.len());
        let mut has_default = false;
        for row in rows {
            let name = RefName::new(row.get::<_, Vec<u8>>(0))?;
            let target = Oid::from_bytes(object_format, row.get(1))?;
            let kind = ObjectType::parse(row.get::<_, &str>(2).as_bytes())?;
            let peeled = Oid::from_bytes(
                object_format,
                row.get::<_, Option<&[u8]>>(3)
                    .ok_or("generation ref has no commit root")?,
            )?;
            if kind != ObjectType::Tag && (kind != ObjectType::Commit || peeled != target) {
                return Err("unexpected generation ref target".into());
            }
            has_default |= name.bytes() == default.bytes();
            snapshot.push(json!([
                STANDARD.encode(name.bytes()),
                kind.name(),
                target.to_string(),
                peeled.to_string()
            ]));
            roots.push(Root {
                target,
                kind,
                peeled,
            });
        }
        if !has_default {
            return Err("generation default ref missing from snapshot".into());
        }
        let snapshot = Value::Array(snapshot);
        let manifest = git_manifest::encode(
            json!([
                self.repo.id.to_string(),
                source.to_string(),
                object_format.name(),
                version.to_string(),
                STANDARD.encode(default.bytes()),
                snapshot
            ]),
            Check::HttpsGit,
            observed,
        );
        if manifest.observed != digest {
            return Err("generation manifest does not match the catalog".into());
        }
        Ok(Generation {
            id,
            roots,
            manifest,
            state: row.get(0),
        })
    }

    async fn work(&self) -> Result<Option<Work>> {
        Ok(self.db.query_opt(
            "SELECT action, generation_id, operation_id, details FROM git_catalog.publication_work WHERE repository_id = $1",
            &[&self.repo.id.uuid()],
        ).await?.map(|row| Work { action: row.get(0), generation: row.get(1), id: row.get(2), details: row.get(3) }))
    }

    async fn idle(&self) -> Result<()> {
        if self.work().await?.is_some() {
            return Err("repository has pending publication work, run --recover".into());
        }
        if self
            .db
            .query_opt(
                "SELECT 1 FROM git_catalog.publication_blocks WHERE repository_id = $1",
                &[&self.repo.id.uuid()],
            )
            .await?
            .is_some()
        {
            return Err("repository is blocked, run --recover".into());
        }
        Ok(())
    }

    async fn begin(&self, id: Option<i64>, action: &str, details: &Value) -> Result<Uuid> {
        let row = self
            .db
            .query_one(
                "SELECT git_catalog.begin_publication_work($1, $2, $3, $4)",
                &[&self.repo.id.uuid(), &id, &action, details],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn finish(&self, operation: Uuid, done: bool) -> Result<()> {
        self.db
            .query_one(
                "SELECT git_catalog.finish_publication_work($1, $2, $3)",
                &[&self.repo.id.uuid(), &operation, &done],
            )
            .await?;
        Ok(())
    }

    async fn incident(&self, generation: Option<i64>, reason: &str, blocked: bool) -> Result<()> {
        self.db
            .query_one(
                "SELECT git_catalog.publication_incident($1, $2, $3, $4)",
                &[&self.repo.id.uuid(), &generation, &reason, &blocked],
            )
            .await?;
        Ok(())
    }

    async fn generation_fence(&self, id: i64) -> Result<Fence> {
        writer_fence(self.db, self.repo.id, LockScope::Generation(id)).await
    }

    async fn current_approval(&self, id: i64) -> Result<bool> {
        Ok(self
            .db
            .query_one(
                "SELECT git_catalog.current_generation_approval($1,$2)",
                &[&self.repo.id.uuid(), &id],
            )
            .await?
            .get(0))
    }

    async fn prepare(&self, id: i64) -> Result<Value> {
        self.idle().await?;
        let _generation = self.generation_fence(id).await?;
        let target = self.generation(id).await?;
        if !self.current_approval(id).await? {
            return Err("candidate approval changed".into());
        }
        if target.state == "ready" {
            self.verify_for_promotion(&target).await?;
            return Ok(report(self.repo.id, Some(id), "ready"));
        }
        if target.state != "verified" {
            return Err("generation is not a verified candidate".into());
        }
        if odb::exists(&self.repo.stage)? {
            return Err("repository has a staged operation, run --recover".into());
        }
        if let Err(error) = self.verify_inventory(None).await {
            self.incident(None, "canonical_inventory_mismatch", true)
                .await?;
            return Err(error);
        }
        let row = self
            .db
            .query_one(
                "SELECT job_id, pack_digest, logical_digest FROM git_catalog.private_candidates
             WHERE repository_id = $1 AND generation_id = $2 AND state = 'usable'",
                &[&self.repo.id.uuid(), &id],
            )
            .await?;
        let baseline = candidate::Baseline {
            directory: private::incoming(row.get(0)),
            generation: id,
            pack: row.get(1),
            observed: target.manifest.observed.to_vec(),
        };
        if target.manifest.logical.as_slice() != row.get::<_, &[u8]>(2) {
            self.incident(Some(id), "candidate_manifest_mismatch", false)
                .await?;
            return Err("candidate logical digest does not match the catalog".into());
        }
        let approval = approval::load(self.db, self.repo.id).await?;
        odb::admission(
            &self.repo.pack_dir(),
            3 * approval.limits.pack_bytes + 64 * approval::MIB,
        )?;
        self.repo.begin_stage()?;
        let preparation = self.stage_candidate(&baseline, &target, &approval).await;
        let preparation = match preparation {
            Ok(preparation) => preparation,
            Err(error) => {
                self.incident(Some(id), "candidate_install_verification_failed", false)
                    .await?;
                self.repo.remove_stage()?;
                return Err(error);
            }
        };
        self.fault.at("before_install_intent");
        let operation = self
            .begin(Some(id), "prepare", &serde_json::to_value(&preparation)?)
            .await?;
        self.fault.at("after_install_intent");
        if let Err(error) = self.apply_prepare(&target, &preparation).await {
            self.incident(Some(id), "canonical_installation_failed", true)
                .await?;
            return Err(error);
        }
        self.fault.at("before_ready");
        self.finish(operation, true).await?;
        self.fault.at("after_ready");
        self.repo.remove_stage()?;
        Ok(report(self.repo.id, Some(id), "ready"))
    }

    async fn stage_candidate(
        &self,
        baseline: &candidate::Baseline,
        target: &Generation,
        approval: &approval::Approval,
    ) -> Result<Preparation> {
        let original = candidate::read_manifest(&baseline.directory.join("manifest.json"))?;
        if original != target.manifest.json.as_bytes() {
            return Err("private manifest bytes differ from the SQL snapshot".into());
        }
        candidate::seed(&self.repo.stage, baseline, approval, self.repo.format).await?;
        let working = self.repo.stage.join("working");
        let packs = working.join("objects/pack");
        let mut files = odb::verify_pack_pairs(&packs, self.repo.format)?
            .into_iter()
            .map(|name| PackFile::inspect(&packs.join(name), self.repo.format))
            .collect::<Result<Vec<_>>>()?;
        if files.len() != 2 || files.iter().any(|f| f.name.ends_with(".mtimes")) {
            return Err("private candidate must supply one verified pack and index".into());
        }
        files.sort_by_key(|file| !file.name.ends_with(".pack"));

        let input = self.repo.stage.join("update-ref");
        let mut updates = String::from("start\n");
        for (index, root) in target.roots.iter().enumerate() {
            updates.push_str(&format!(
                "create refs/gh-docs/retention/{}/{index} {}\n",
                target.id, root.target
            ));
        }
        updates.push_str("prepare\ncommit\n");
        odb::write_file(&input, updates.as_bytes(), 0o600)?;
        worker::local_with_input(
            &self.repo.stage,
            &working,
            &args(&["update-ref", "--no-deref", "--stdin"]),
            4096,
            Stdio::from(File::open(&input)?),
        )
        .await?;

        let roots = working.join(format!("refs/gh-docs/retention/{}", target.id));
        self.verify_roots(&roots, target)?;
        for entry in odb::names(&roots, approval::MAX_REFS)? {
            File::open(roots.join(entry))?.sync_all()?;
        }
        for path in [
            &roots,
            &working.join("refs/gh-docs/retention"),
            &working.join("refs/gh-docs"),
            &working.join("refs"),
            &packs,
            &working.join("objects"),
            &working,
        ] {
            File::open(path)?.sync_all()?;
        }
        for pack in &files {
            File::open(packs.join(&pack.name))?.sync_all()?;
        }
        odb::write_file(&self.repo.stage.join("manifest.json"), &original, 0o644)?;
        File::open(&self.repo.stage)?.sync_all()?;
        Ok(Preparation { files })
    }

    fn verify_roots(&self, path: &Path, target: &Generation) -> Result<()> {
        let dir = fs::symlink_metadata(path)?;
        if !dir.is_dir() || odb::names(path, approval::MAX_REFS)?.len() != target.roots.len() {
            return Err("unexpected retention root inventory".into());
        }
        for (index, root) in target.roots.iter().enumerate() {
            let mut actual = Vec::new();
            candidate::open_regular(&path.join(index.to_string()))?
                .take(66)
                .read_to_end(&mut actual)?;
            if actual != format!("{}\n", root.target).as_bytes() {
                return Err("retention root differs from the SQL snapshot".into());
            }
        }
        Ok(())
    }

    async fn apply_prepare(&self, target: &Generation, details: &Preparation) -> Result<()> {
        self.repo.ensure_odb()?;
        let working = self.repo.stage.join("working");
        let packs = working.join("objects/pack");
        self.fault.at("before_pack");
        for file in &details.files {
            file.install(&self.repo, &packs, None)?;
            self.fault.at(if file.name.ends_with(".pack") {
                "after_pack"
            } else {
                "after_index"
            });
        }
        let refs = self.repo.roots(target.id);
        if !odb::exists(&refs)? {
            self.fault.at("before_refs");
            odb::rename_new(
                &working.join(format!("refs/gh-docs/retention/{}", target.id)),
                &refs,
            )?;
            self.fault.at("after_refs");
        }
        self.verify_roots(&refs, target)?;
        let manifest = self.repo.manifest(target.id);
        if !odb::exists(&manifest)? {
            self.fault.at("before_manifest");
            odb::rename_new(&self.repo.stage.join("manifest.json"), &manifest)?;
            self.fault.at("after_manifest");
        }
        self.verify_native(Some(target)).await
    }

    async fn verify_for_promotion(&self, target: &Generation) -> Result<()> {
        let result = async {
            self.verify_inventory(None).await?;
            self.verify_native(Some(target)).await
        }
        .await;
        if result.is_err() {
            self.incident(Some(target.id), "canonical_verification_failed", true)
                .await?;
        }
        result
    }

    async fn verify_inventory(&self, replacement: Option<&[PackFile]>) -> Result<()> {
        let registered;
        let expected = if let Some(replacement) = replacement {
            replacement
        } else {
            registered = self
                .db
                .query(
                    "SELECT filename, checksum FROM git_catalog.publication_packs
                 WHERE repository_id = $1 ORDER BY filename LIMIT 2049",
                    &[&self.repo.id.uuid()],
                )
                .await?
                .into_iter()
                .map(|row| PackFile {
                    name: row.get(0),
                    digest: row.get(1),
                })
                .collect::<Vec<_>>();
            if registered.len() > 2048 {
                return Err("canonical pack registry exceeds its limit".into());
            }
            &registered
        };
        let dir = self.repo.pack_dir();
        if !odb::exists(&dir)? && expected.is_empty() {
            return Ok(());
        }
        let actual = odb::verify_pack_pairs(&dir, self.repo.format)?;
        if actual.len() != expected.len() {
            return Err("canonical pack inventory differs from the operation record".into());
        }
        for file in expected {
            if !file.matches(&dir, self.repo.format)? {
                return Err("canonical pack checksum differs from the operation record".into());
            }
        }
        Ok(())
    }

    async fn verify_native(&self, target: Option<&Generation>) -> Result<()> {
        self.repo.validate_layout()?;
        odb::verify_pack_pairs(&self.repo.pack_dir(), self.repo.format)?;
        if let Some(target) = target {
            self.verify_generation_files(target)?;
        }
        worker::publication_read(
            self.repo.id,
            self.repo.format,
            &args(&["fsck", "--full", "--strict", "--no-dangling"]),
            256 * 1024,
            Stdio::null(),
        )
        .await?;
        if let Some(target) = target {
            self.verify_root_types(std::slice::from_ref(target)).await?;
        }
        Ok(())
    }

    fn verify_generation_files(&self, generation: &Generation) -> Result<()> {
        if candidate::read_manifest(&self.repo.manifest(generation.id))?
            != generation.manifest.json.as_bytes()
        {
            return Err("published manifest differs from the SQL snapshot".into());
        }
        self.verify_roots(&self.repo.roots(generation.id), generation)
    }

    async fn verify_root_types(&self, targets: &[Generation]) -> Result<()> {
        let mut input = BTreeSet::new();
        for root in targets.iter().flat_map(|target| &target.roots) {
            input.insert((
                root.target.to_string(),
                format!("{} {}", root.target, root.kind.name()),
            ));
            if root.kind == ObjectType::Tag {
                input.insert((
                    format!("{}^{{commit}}", root.target),
                    format!("{} commit", root.peeled),
                ));
            }
            if input.len() > 50_000 {
                return Err("canonical root verification budget exceeded".into());
            }
        }
        if input.is_empty() {
            return Ok(());
        }
        let mut oids = String::new();
        let mut expected = String::new();
        for (oid, line) in input {
            oids.push_str(&oid);
            oids.push('\n');
            expected.push_str(&line);
            expected.push('\n');
        }
        let path = self.repo.stage.with_file_name("verify-roots");
        if odb::exists(&path)? {
            let _ = candidate::open_regular(&path)?;
            fs::remove_file(&path)?;
        }
        odb::write_file(&path, oids.as_bytes(), 0o600)?;
        let file = File::open(&path)?;
        fs::remove_file(&path)?;
        let output = worker::publication_read(
            self.repo.id,
            self.repo.format,
            &args(&["cat-file", "--batch-check=%(objectname) %(objecttype)"]),
            expected.len(),
            Stdio::from(file),
        )
        .await?;
        if output != expected.as_bytes() {
            return Err(
                "published root type or peeled target differs from the SQL snapshot".into(),
            );
        }
        Ok(())
    }

    async fn promote(&self, id: i64, expected: i64, rollback: bool) -> Result<Value> {
        self.idle().await?;
        let previous = self.db.query_opt(
            "SELECT generation_id, publication_sequence FROM git_catalog.active_generations WHERE repository_id = $1",
            &[&self.repo.id.uuid()],
        ).await?;
        let sequence = previous.as_ref().map_or(0, |row| row.get::<_, i64>(1));
        if sequence != expected || sequence == i64::MAX {
            return Err("publication sequence mismatch or exhausted".into());
        }
        let mut ids = BTreeSet::from([id]);
        if let Some(row) = previous {
            ids.insert(row.get(0));
        }
        let mut fences = Vec::new();
        for generation in ids {
            fences.push(self.generation_fence(generation).await?);
        }
        let target = self.generation(id).await?;
        if target.state != if rollback { "retained" } else { "ready" }
            || !self.current_approval(id).await?
        {
            return Err("publication target unavailable".into());
        }
        self.verify_for_promotion(&target).await?;
        self.fault.at("before_publish");
        let row = self
            .db
            .query_one(
                "SELECT sequence, notification FROM git_catalog.promote_generation($1,$2,$3,$4)",
                &[&self.repo.id.uuid(), &id, &expected, &rollback],
            )
            .await?;
        self.fault.at("after_publish");
        Ok(json!({
            "repository": self.repo.id.to_string(), "generation": id.to_string(), "outcome": "active",
            "sequence": row.get::<_, i64>(0).to_string(), "event": row.get::<_, Uuid>(1)
        }))
    }

    async fn resume_prepare(&self, work: &Work, id: i64) -> Result<()> {
        let target = self.generation(id).await?;
        let details: Preparation = serde_json::from_value(work.details.clone())?;
        if self.current_approval(id).await? {
            self.apply_prepare(&target, &details).await?;
            self.finish(work.id, true).await
        } else {
            // Complete any partial pack pair before removing the unapproved roots.
            // Verified orphan packs can be reclaimed by a later guarded GC.
            self.apply_prepare(&target, &details).await?;
            self.remove_generation_files(&target)?;
            self.finish(work.id, false).await
        }
    }

    async fn unlock_generation(&self, id: i64) -> Result<()> {
        release_generation(self.db, self.repo.id, id).await
    }

    fn remove_generation_files(&self, generation: &Generation) -> Result<()> {
        let refs = self.repo.roots(generation.id);
        if odb::exists(&refs)? {
            self.verify_roots(&refs, generation)?;
            let detached = self.repo.stage.join("retired-refs");
            odb::rename_new(&refs, &detached)?;
        }
        let manifest = self.repo.manifest(generation.id);
        if odb::exists(&manifest)? {
            if candidate::read_manifest(&manifest)? != generation.manifest.json.as_bytes() {
                return Err("retired manifest failed its recorded digest proof".into());
            }
            odb::rename_new(&manifest, &self.repo.stage.join("retired-manifest.json"))?;
        }
        Ok(())
    }
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn report(repo: RepositoryId, id: Option<i64>, outcome: &str) -> Value {
    json!({"repository":repo.to_string(), "generation":id.map(|id| id.to_string()), "outcome":outcome})
}
