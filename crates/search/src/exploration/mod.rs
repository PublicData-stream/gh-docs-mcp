//! Private read-only catalog and Git exploration.

mod blob;
mod cursor;
mod history;
mod matcher;
mod page;
mod request;
mod rg;
mod tree;

pub use cursor::CursorKey;
pub use request::*;

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use docs_mcp_core::git::{ByteString, ObjectFormat, ObjectType, Oid, RefName, RepositoryId};
use docs_mcp_git::reader::{Batch, Reachability, Reader, SandboxUnavailable, TreeMode};
use docs_mcp_storage::{
    catalog::RefSnapshot,
    connect,
    exploration::{self as catalog, Metadata},
    publication::{Generation, GenerationUnavailable, read_exploration},
};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_postgres::Client;

use cursor::{Pagination, Position, Scope};
use matcher::Matcher;
use page::{Entries, coverage, json_size};
use tree::{MAX_DEPTH, Tree};

// Reserve space for the largest valid request ID and the protocol envelope.
const RESPONSE_BYTES: usize = 184 * 1024;
const REQUEST_DEADLINE: Duration = Duration::from_secs(15);
const GLOBAL_SLOTS: usize = 4;
const REPOSITORY_SLOTS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidArgument,
    InvalidRegex,
    PolicyDenied,
    UnsupportedFormat,
    UnsupportedEncoding,
    InvalidBoundary,
    RepositoryUnavailable,
    GenerationUnavailable,
    ObjectUnavailable,
    Budget,
    CursorInvalid,
    CursorExpired,
    SandboxUnavailable,
    Internal,
}

impl Error {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRegex => "invalid_regex",
            Self::PolicyDenied => "policy_denied",
            Self::InvalidArgument => "invalid_argument",
            Self::UnsupportedFormat => "unsupported_object_format",
            Self::UnsupportedEncoding => "unsupported_encoding",
            Self::InvalidBoundary => "invalid_utf8_boundary",
            Self::RepositoryUnavailable => "repository_unavailable",
            Self::GenerationUnavailable => "generation_unavailable",
            Self::ObjectUnavailable => "object_unavailable",
            Self::Budget => "budget_exceeded",
            Self::CursorInvalid => "cursor_invalid",
            Self::CursorExpired => "cursor_expired",
            Self::SandboxUnavailable => "sandbox_unavailable",
            Self::Internal => "internal_error",
        }
    }

    fn from_storage(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        if let Some(error) = error.downcast_ref::<Self>() {
            *error
        } else if error.is::<GenerationUnavailable>() {
            Self::GenerationUnavailable
        } else if let Some(error) = error.downcast_ref::<io::Error>() {
            Self::from_io(error)
        } else {
            Self::Internal
        }
    }

    fn from_io(error: &io::Error) -> Self {
        if error
            .get_ref()
            .is_some_and(|error| error.is::<SandboxUnavailable>())
        {
            return Self::SandboxUnavailable;
        }
        match error.kind() {
            io::ErrorKind::FileTooLarge | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
                Self::Budget
            }
            _ => Self::Internal,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::from_io(&error)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str(self.code())
    }
}

impl std::error::Error for Error {}

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let restart = match self {
            Self::CursorInvalid | Self::CursorExpired | Self::GenerationUnavailable => {
                Some("Restart from the first page or resolve a fresh context.")
            }
            Self::Budget => Some("Retry after current work finishes or narrow the request scope."),
            _ => None,
        };
        json!({"code":self.code(), "guidance":restart}).serialize(ser)
    }
}

pub struct Explorer {
    key: Arc<CursorKey>,
    global: Arc<Semaphore>,
    repositories: Mutex<HashMap<RepositoryId, Weak<Semaphore>>>,
    force_worker_failure: bool,
}

impl Explorer {
    pub fn new(key: CursorKey) -> Self {
        Self {
            key: Arc::new(key),
            global: Arc::new(Semaphore::new(GLOBAL_SLOTS)),
            repositories: Mutex::new(HashMap::new()),
            force_worker_failure: false,
        }
    }

    #[cfg(feature = "fixtures")]
    pub fn force_worker_failure_for_fixture(mut self) -> Self {
        self.force_worker_failure = true;
        self
    }

    /// Admission is scoped to this service instance. Keep one instance per
    /// serving process and share it across requests.
    pub async fn execute(&self, request: Request) -> Result<Value, Error> {
        let permit = Arc::clone(&self.global)
            .try_acquire_owned()
            .map_err(|_| Error::Budget)?;
        let result = tokio::time::timeout(REQUEST_DEADLINE, self.dispatch(request, permit))
            .await
            .map_err(|_| Error::Budget)??;
        if json_size(&result)? > RESPONSE_BYTES {
            return Err(Error::Budget);
        }
        Ok(result)
    }

    /// Call after cancelling the request handlers. Blocking readers keep their
    /// permits until their Git and search workers have exited.
    pub async fn drain(&self) {
        if let Ok(_permits) = self.global.acquire_many(GLOBAL_SLOTS as u32).await {
            self.global.close();
        }
    }

    fn repo_permit(&self, repo: RepositoryId) -> Result<OwnedSemaphorePermit, Error> {
        let mut map = self.repositories.lock().map_err(|_| Error::Internal)?;
        map.retain(|_, slot| slot.strong_count() > 0);
        let slot = map.get(&repo).and_then(Weak::upgrade).unwrap_or_else(|| {
            let slot = Arc::new(Semaphore::new(REPOSITORY_SLOTS));
            map.insert(repo, Arc::downgrade(&slot));
            slot
        });
        slot.try_acquire_owned().map_err(|_| Error::Budget)
    }

    async fn fenced(
        &self,
        client: &mut Client,
        repo: RepositoryId,
        generation: Option<i64>,
        global: OwnedSemaphorePermit,
        operation: impl FnOnce(Generation, &Reader) -> Result<Value, Error> + Send + 'static,
    ) -> Result<Value, Error> {
        // Screen visibility before repository-specific admission. The storage
        // layer checks again under the repository and generation fences.
        if catalog::get(client, repo, generation)
            .await
            .map_err(Error::from_storage)?
            .is_none()
        {
            return Err(Error::GenerationUnavailable);
        }
        let permit = self.repo_permit(repo)?;
        let fail = self.force_worker_failure;
        read_exploration(client, repo, generation, move |generation, reader| {
            let (_global, _repository) = (global, permit);
            if fail {
                reader.fail_setup_for_probe();
            }
            Ok(operation(generation, reader)?)
        })
        .await
        .map_err(Error::from_storage)
    }

    fn pagination(
        &self,
        tool: &'static str,
        args: &Value,
        cursor: Option<&str>,
    ) -> Result<Pagination, Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Internal)?
            .as_secs();
        Pagination::open(&self.key, tool, args, cursor, now)
    }

    async fn dispatch(
        &self,
        request: Request,
        global: OwnedSemaphorePermit,
    ) -> Result<Value, Error> {
        let (mut client, _driver) = connect("gh_docs_serve")
            .await
            .map_err(Error::from_storage)?;
        match request {
            Request::RepositorySearch(request) => self.repositories(&client, request).await,
            Request::RepositoryInfo(request) => {
                let repo = repository(&request.repository_id)?;
                let metadata = catalog::get(&client, repo, None)
                    .await
                    .map_err(Error::from_storage)?
                    .ok_or(Error::RepositoryUnavailable)?;
                Ok(summary(&metadata, metadata.sequence))
            }
            Request::Refs(request) => {
                let repo = repository(&request.repository_id)?;
                let requested = request
                    .generation_id
                    .as_deref()
                    .map(generation)
                    .transpose()?;
                let limit = request.page.limit()?;
                let args = json!([repo.to_string(), requested, limit]);
                let pagination =
                    self.pagination("git.refs", &args, request.page.cursor.as_deref())?;
                let generation = pinned(&pagination, repo, requested, None)?;
                self.fenced(&mut client, repo, generation, global, move |snapshot, _| {
                    refs_page(&snapshot, &pagination, limit)
                })
                .await
            }
            Request::Resolve(request) => {
                let repo = repository(&request.repository_id)?;
                let requested = request
                    .generation_id
                    .as_deref()
                    .map(generation)
                    .transpose()?;
                self.fenced(
                    &mut client,
                    repo,
                    requested,
                    global,
                    move |snapshot, reader| resolve(&snapshot, reader, request.selector),
                )
                .await
            }
            Request::ListTree(request) => {
                let query = TreeQuery::from_list(self, request)?;
                self.tree(&mut client, query, global).await
            }
            Request::Find(request) => {
                let query = TreeQuery::from_find(self, request)?;
                self.tree(&mut client, query, global).await
            }
            Request::Rg(request) => self.rg(&mut client, request, global).await,
            Request::Log(request) => {
                self.history(&mut client, history::Query::Log(request), global)
                    .await
            }
            Request::Show(request) => {
                self.history(&mut client, history::Query::Show(request), global)
                    .await
            }
            Request::Diff(request) => {
                self.history(&mut client, history::Query::Diff(request), global)
                    .await
            }
            Request::Blame(request) => {
                self.history(&mut client, history::Query::Blame(request), global)
                    .await
            }
            Request::Read(request) => {
                let (repo, generation, format, commit) = request.context.coordinates()?;
                let path = request.path.decode().map_err(|_| Error::InvalidArgument)?;
                if request.range.is_some_and(|range| range.start > range.end) {
                    return Err(Error::InvalidArgument);
                }
                let args = json!([
                    request.context,
                    request.path,
                    request.range,
                    request.encoding
                ]);
                let pagination = self.pagination("git.read", &args, request.cursor.as_deref())?;
                pinned(&pagination, repo, Some(generation), Some(commit))?;
                self.fenced(
                    &mut client,
                    repo,
                    Some(generation),
                    global,
                    move |snapshot, reader| {
                        require_format(format, &snapshot)?;
                        read_blob(&snapshot, reader, commit, request, &path, &pagination)
                    },
                )
                .await
            }
        }
    }

    async fn repositories(
        &self,
        client: &Client,
        request: RepositorySearch,
    ) -> Result<Value, Error> {
        let limit = request.page.limit()?;
        if request.query.len() > 256 || request.query.contains('\0') {
            return Err(Error::InvalidArgument);
        }
        let args = json!([request.query, limit]);
        let pagination =
            self.pagination("repository.search", &args, request.page.cursor.as_deref())?;
        let after = match (&pagination.position, &pagination.scope) {
            (None, None) => None,
            (Some(Position::Repository(repo)), None) => Some(*repo),
            _ => return Err(Error::CursorInvalid),
        };
        let rows = catalog::list(client, &request.query, after, (limit + 1) as i64)
            .await
            .map_err(Error::from_storage)?;
        let base = json!({"pagination":"live"});
        let mut output = Entries::new(limit, None, &base)?;
        for row in rows {
            if !output.push(summary(&row, row.sequence))? {
                break;
            }
        }
        output.finish(base, &pagination, None, false, |last| {
            let repository_id = last["repository_id"].as_str().ok_or(Error::Internal)?;
            Ok(repository(repository_id)?.uuid())
        })
    }

    async fn tree(
        &self,
        client: &mut Client,
        query: TreeQuery,
        global: OwnedSemaphorePermit,
    ) -> Result<Value, Error> {
        let (repo, generation, format, commit) = query.context.coordinates()?;
        pinned(&query.pagination, repo, Some(generation), Some(commit))?;
        self.fenced(
            client,
            repo,
            Some(generation),
            global,
            move |snapshot, reader| {
                require_format(format, &snapshot)?;
                tree_page(&snapshot, reader, commit, query)
            },
        )
        .await
    }
}

fn metadata(snapshot: &Generation) -> Result<&Metadata, Error> {
    snapshot.exploration.as_ref().ok_or(Error::Internal)
}

fn summary(meta: &Metadata, publication_sequence: i64) -> Value {
    json!({
        "repository_id":meta.repository.to_string(),
        "source_id":meta.source,
        "source_identity":meta.identity,
        "object_format":meta.format.name(),
        "generation_id":meta.generation.to_string(),
        "publication_sequence":publication_sequence.to_string(),
        "policy_version":meta.policy_version.to_string(),
        "observed_at":meta.observed_at,
        "approved_ref_scope":{"kind":"verified_snapshot", "count":meta.ref_count},
        "metadata_notice":meta.policy.metadata_notice,
    })
}

fn context(meta: &Metadata, commit: Oid) -> Context {
    Context {
        repository_id: meta.repository.to_string(),
        generation_id: meta.generation.to_string(),
        object_format: meta.format.name().into(),
        commit_oid: commit.to_string(),
    }
}

fn scope(meta: &Metadata, commit: Option<Oid>, pagination: &Pagination) -> Scope {
    Scope {
        repository_id: meta.repository.to_string(),
        generation_id: meta.generation,
        commit_oid: commit.map(|oid| oid.to_string()),
        sequence: pagination
            .scope
            .as_ref()
            .map_or(meta.sequence, |scope| scope.sequence),
    }
}

fn pinned(
    pagination: &Pagination,
    repo: RepositoryId,
    generation: Option<i64>,
    commit: Option<Oid>,
) -> Result<Option<i64>, Error> {
    if let Some(scope) = &pagination.scope {
        if repo.to_string() != scope.repository_id
            || generation.is_some_and(|generation| generation != scope.generation_id)
            || commit.map(|oid| oid.to_string()) != scope.commit_oid
        {
            return Err(Error::CursorInvalid);
        }
        return Ok(Some(scope.generation_id));
    }
    Ok(generation)
}

fn require_format(format: ObjectFormat, snapshot: &Generation) -> Result<(), Error> {
    if format != snapshot.format {
        return Err(Error::ObjectUnavailable);
    }
    Ok(())
}

fn refs_page(snapshot: &Generation, pagination: &Pagination, limit: usize) -> Result<Value, Error> {
    let meta = metadata(snapshot)?;
    let scope = scope(meta, None, pagination);
    let base = json!({
        "source":summary(meta, scope.sequence),
        "default_ref":meta.default_ref.as_ref().map(|reference| ByteString::encode(reference.bytes())),
    });
    let mut output = Entries::new(limit, pagination.offset()?, &base)?;
    for reference in &snapshot.refs {
        let entry = json!({
            "name":ByteString::encode(reference.name.bytes()),
            "target_oid":reference.target.to_string(),
            "target_type":reference.kind.name(),
            "peeled_commit":reference.peeled_commit.map(|oid| oid.to_string()),
        });
        if !output.push(entry)? {
            break;
        }
    }
    output.finish(base, pagination, Some(scope), false, |_| {
        Err(Error::Internal)
    })
}

fn verify_commit(snapshot: &Generation, batch: &mut Batch<'_>, commit: Oid) -> Result<(), Error> {
    match snapshot.reachable_with(batch, commit)? {
        Reachability::Reachable => Ok(()),
        Reachability::NotReachable => Err(Error::ObjectUnavailable),
        Reachability::Indeterminate => Err(Error::Budget),
    }
}

fn peel(batch: &mut Batch<'_>, reference: &RefSnapshot) -> Result<Oid, Error> {
    let expected = reference.peeled_commit.ok_or(Error::ObjectUnavailable)?;
    let actual = batch.info(reference.target)?.ok_or(Error::Internal)?;
    if actual.kind != reference.kind || batch.peel_commit(reference.target)? != Some(expected) {
        return Err(Error::Internal);
    }
    Ok(expected)
}

fn resolve(snapshot: &Generation, reader: &Reader, selector: Selector) -> Result<Value, Error> {
    let meta = metadata(snapshot)?;
    let reference = match selector {
        Selector::Ref(name) => Some(
            RefName::new(name.decode(4096).map_err(|_| Error::InvalidArgument)?)
                .map_err(|_| Error::InvalidArgument)?,
        ),
        Selector::Default => Some(meta.default_ref.clone().ok_or(Error::ObjectUnavailable)?),
        Selector::Oid(oid) => {
            let oid =
                Oid::parse(snapshot.format, oid.as_bytes()).map_err(|_| Error::InvalidArgument)?;
            let tag = snapshot
                .refs
                .iter()
                .find(|entry| entry.target == oid && entry.kind == ObjectType::Tag);
            let commit = reader.with_batch::<_, Error>(|batch| {
                if let Some(tag) = tag {
                    peel(batch, tag)
                } else {
                    verify_commit(snapshot, batch, oid)?;
                    Ok(oid)
                }
            })?;
            return Ok(
                json!({"context":context(meta, commit), "source":summary(meta, meta.sequence)}),
            );
        }
    };
    let entry = snapshot
        .refs
        .iter()
        .find(|entry| Some(&entry.name) == reference.as_ref())
        .ok_or(Error::ObjectUnavailable)?;
    let commit = reader.with_batch(|batch| peel(batch, entry))?;
    Ok(json!({
        "context":context(meta, commit),
        "source":summary(meta, meta.sequence),
        "resolved_ref":ByteString::encode(entry.name.bytes()),
    }))
}

struct TreeQuery {
    context: Context,
    root: Vec<u8>,
    depth: u8,
    matcher: Option<Matcher>,
    limit: usize,
    pagination: Pagination,
}

impl TreeQuery {
    fn from_list(explorer: &Explorer, request: ListTree) -> Result<Self, Error> {
        let depth = request.depth.unwrap_or(1);
        let limit = request.page.limit()?;
        let args = json!([request.context, request.root, depth, limit]);
        Ok(Self {
            root: Self::root(request.root, depth)?,
            context: request.context,
            depth,
            matcher: None,
            limit,
            pagination: explorer.pagination(
                "git.ls_tree",
                &args,
                request.page.cursor.as_deref(),
            )?,
        })
    }

    fn from_find(explorer: &Explorer, request: Find) -> Result<Self, Error> {
        let depth = request.depth.unwrap_or(MAX_DEPTH);
        let limit = request.page.limit()?;
        let args = json!([request.context, request.root, depth, request.pattern, limit]);
        Ok(Self {
            root: Self::root(request.root, depth)?,
            context: request.context,
            depth,
            matcher: Some(Matcher::compile(request.pattern)?),
            limit,
            pagination: explorer.pagination("git.find", &args, request.page.cursor.as_deref())?,
        })
    }

    fn root(root: Option<docs_mcp_core::git::BytePath>, depth: u8) -> Result<Vec<u8>, Error> {
        if !(1..=MAX_DEPTH).contains(&depth) {
            return Err(Error::InvalidArgument);
        }
        root.map_or(Ok(Vec::new()), |path| {
            path.decode().map_err(|_| Error::InvalidArgument)
        })
    }
}

fn tree_page(
    snapshot: &Generation,
    reader: &Reader,
    commit: Oid,
    query: TreeQuery,
) -> Result<Value, Error> {
    let meta = metadata(snapshot)?;
    let scope = scope(meta, Some(commit), &query.pagination);
    let base = json!({"context":query.context, "source":summary(meta, scope.sequence)});
    let mut output = Entries::new(query.limit, query.pagination.offset()?, &base)?;
    reader.with_batch(|batch| {
        verify_commit(snapshot, batch, commit)?;
        let commit_tree = batch.commit(commit)?.tree;
        let mut tree = Tree::new(batch, &meta.policy);
        let root = tree.directory(commit_tree, &query.root)?;
        let budget = match tree.walk(
            root,
            &query.root,
            query.depth,
            query.matcher.as_ref(),
            &mut output,
        ) {
            Ok(_) => false,
            Err(Error::Budget) => true,
            Err(error) => return Err(error),
        };
        output.finish(base, &query.pagination, Some(scope), budget, |_| {
            Err(Error::Internal)
        })
    })
}

fn read_blob(
    snapshot: &Generation,
    reader: &Reader,
    commit: Oid,
    request: Read,
    path: &[u8],
    pagination: &Pagination,
) -> Result<Value, Error> {
    let meta = metadata(snapshot)?;
    let scope = scope(meta, Some(commit), pagination);
    reader.with_batch(|batch| {
        verify_commit(snapshot, batch, commit)?;
        let commit_tree = batch.commit(commit)?.tree;
        let mut tree = Tree::new(batch, &meta.policy);
        let (entry, notice) = tree
            .approved_leaf(commit_tree, path)?
            .ok_or(Error::ObjectUnavailable)?;
        let (mode, kind) = tree::mode(&entry.mode);
        let mut base = json!({
            "context":request.context, "source":summary(meta, scope.sequence),
            "path":request.path, "mode":mode, "kind":kind, "oid":entry.oid.to_string(),
            "notice":notice,
        });
        if entry.mode == TreeMode::Gitlink {
            if request.range.is_some() || pagination.position.is_some() {
                return Err(Error::InvalidArgument);
            }
            base["coverage"] = coverage(true, false, 1, None);
            return Ok(base);
        }
        let raw = tree.blob(entry.oid)?;
        blob::page(
            base,
            &raw,
            request.range,
            request.encoding,
            pagination,
            scope,
        )
    })
}
