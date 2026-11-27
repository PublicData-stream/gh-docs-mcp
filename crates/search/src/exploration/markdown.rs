//! Markdown discovery, filtering, lookup, and search over approved snapshots.

use std::{sync::Arc, time::Instant};

use docs_mcp_core::{git::BytePath, markdown::valid_digest};
use docs_mcp_git::reader::{TreeMode, spawn_search_worker};
use docs_mcp_storage::markdown as store;
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;
use tokio_postgres::{Client, Row};

use crate::worker::{BLOB_BYTES, PATTERN_BYTES};

use super::{
    Context, Error, Explorer, MarkdownDocument, MarkdownFind, MarkdownPattern, MarkdownRead,
    MarkdownSource, MarkdownSources, Pattern, REQUEST_DEADLINE, RESPONSE_BYTES, blob,
    cursor::Position,
    matcher::Matcher,
    metadata,
    page::{coverage, json_size},
    repository, require_format,
    rg::{SearchWorker, match_item},
    scope,
    tree::{MAX_DEPTH, Tree},
    verify_commit,
};

const DOCS_PER_SEARCH: usize = 32;
const ROWS_PER_SCAN: i64 = 128;

impl MarkdownSource {
    fn context(&self) -> Option<&Context> {
        match self {
            Self::GitMarkdown { context } | Self::GitRustdoc { context, .. } => Some(context),
            Self::DocsrsRustdoc { .. } => None,
        }
    }

    fn coordinates(&self) -> Result<(uuid::Uuid, &str), Error> {
        let (source, digest) = match self {
            Self::GitMarkdown { .. } => return Err(Error::Internal),
            Self::GitRustdoc {
                source_id,
                snapshot_digest,
                ..
            }
            | Self::DocsrsRustdoc {
                source_id,
                snapshot_digest,
            } => (source_id, snapshot_digest),
        };
        if !valid_digest(digest) {
            return Err(Error::InvalidArgument);
        }
        Ok((repository(source)?.uuid(), digest))
    }

    fn sort_key(&self) -> (&str, &str, &str) {
        match self {
            Self::GitMarkdown { context } => (&context.repository_id, "git_markdown", ""),
            Self::GitRustdoc {
                source_id,
                snapshot_digest,
                ..
            } => (source_id, "git_rustdoc", snapshot_digest),
            Self::DocsrsRustdoc {
                source_id,
                snapshot_digest,
            } => (source_id, "docsrs_rustdoc", snapshot_digest),
        }
    }
}

struct Scan {
    rows: Vec<Value>,
    used: usize,
    limit: usize,
    cursor: Option<(u64, u32)>,
    search: Option<MarkdownPattern>,
    examined: usize,
    bytes: usize,
    omitted: bool,
}

impl Scan {
    fn doc(
        &mut self,
        ordinal: u64,
        resume: u32,
        doc: Value,
        raw: Option<&[u8]>,
        worker: Option<&mut SearchWorker>,
    ) -> Result<bool, Error> {
        if let Some(worker) = worker {
            let Some(raw) = raw else {
                self.omitted = true;
                return Ok(true);
            };
            if raw.contains(&0) {
                self.omitted = true;
                return Ok(true);
            }
            self.examined += 1;
            self.bytes += raw.len();
            let context = self.search.as_ref().ok_or(Error::Internal)?.context_lines;
            let path: BytePath = serde_json::from_value(doc["document"]["path"].clone())
                .map_err(|_| Error::Internal)?;
            let path = path.decode().map_err(|_| Error::Internal)?;
            for fields in worker.matches(raw, resume)? {
                let mut item = match_item(raw, &path, "", "", fields, context)?;
                let output = item.as_object_mut().ok_or(Error::Internal)?;
                output.remove("oid");
                output.remove("notice");
                output.remove("path");
                output.extend(doc.as_object().ok_or(Error::Internal)?.clone());
                if !self.push(ordinal, fields[3] as u32, item)? {
                    return Ok(false);
                }
            }
            Ok(true)
        } else {
            self.push(ordinal, 0, doc)
        }
    }

    fn push(&mut self, ordinal: u64, byte: u32, value: Value) -> Result<bool, Error> {
        let bytes = json_size(&value)? + 3;
        if self.rows.len() == self.limit || self.used + bytes > RESPONSE_BYTES {
            if self.rows.is_empty() {
                return Err(Error::Budget);
            }
            self.cursor = Some((ordinal, byte));
            return Ok(false);
        }
        self.used += bytes;
        self.rows.push(value);
        Ok(true)
    }

    fn search_limit(&self) -> bool {
        self.search.is_some()
            && (self.examined == DOCS_PER_SEARCH || self.bytes >= 32 * 1024 * 1024)
    }
}

struct Filters {
    source: Option<Matcher>,
    output: Option<Matcher>,
}

impl Filters {
    fn matches(&self, source: &[u8], path: &[u8]) -> bool {
        self.source.as_ref().is_none_or(|glob| glob.matches(source))
            && self.output.as_ref().is_none_or(|glob| glob.matches(path))
    }
}

impl Explorer {
    pub(super) async fn markdown_sources(
        &self,
        client: &Client,
        request: MarkdownSources,
    ) -> Result<Value, Error> {
        let limit = request.page.limit()?;
        let pagination = self.pagination(
            "markdown.sources",
            &json!(limit),
            request.page.cursor.as_deref(),
        )?;
        let offset = pagination.blob_offset(false)?.unwrap_or(0);
        let rows = store::sources(client, offset as i64, (limit + 1) as i64)
            .await
            .map_err(Error::from_storage)?;
        let mut scan = Scan {
            rows: vec![],
            used: 2048,
            limit,
            cursor: None,
            search: None,
            examined: 0,
            bytes: 0,
            omitted: false,
        };
        for row in rows {
            let input: Value = row.get("input");
            let id: uuid::Uuid = row.get("source_id");
            let notice: String = row.get("notice");
            let digest: Option<String> = row.get("digest");
            let repository: Option<uuid::Uuid> = row.get("repository_id");
            let entry = if let Some(digest) = digest {
                let source = if let Some(context) = row_context(&row) {
                    MarkdownSource::GitRustdoc {
                        source_id: id.to_string(),
                        snapshot_digest: digest,
                        context,
                    }
                } else {
                    MarkdownSource::DocsrsRustdoc {
                        source_id: id.to_string(),
                        snapshot_digest: digest,
                    }
                };
                json!({"source":source, "input":input, "notice":notice})
            } else {
                json!({"kind":"git_markdown", "repository_id":repository, "notice":notice,
                    "generation_id":row.get::<_, Option<i64>>("generation_id").map(|id| id.to_string())})
            };
            if !scan.push(offset + scan.rows.len() as u64, 0, entry)? {
                break;
            }
        }
        let count = scan.rows.len();
        let cursor = scan
            .cursor
            .map(|(offset, _)| pagination.encode(None, Position::Offset(offset)))
            .transpose()?;
        Ok(
            json!({"entries":scan.rows, "pagination":"live", "next_cursor":cursor,
            "coverage":coverage(cursor.is_none(), false, count, None)}),
        )
    }

    pub(super) async fn markdown_scan(
        &self,
        client: &mut Client,
        mut request: MarkdownFind,
        pattern: Option<MarkdownPattern>,
        global: OwnedSemaphorePermit,
    ) -> Result<Value, Error> {
        if !(1..=8).contains(&request.sources.len())
            || pattern
                .as_ref()
                .is_some_and(|p| p.pattern.len() > PATTERN_BYTES || p.context_lines > 3)
        {
            return Err(Error::InvalidArgument);
        }
        request
            .sources
            .sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
        if request
            .sources
            .windows(2)
            .any(|pair| pair[0].sort_key() == pair[1].sort_key())
        {
            return Err(Error::InvalidArgument);
        }
        let shared_context = request.sources.iter().find_map(MarkdownSource::context);
        if request
            .sources
            .iter()
            .filter_map(MarkdownSource::context)
            .any(|ctx| Some(ctx) != shared_context)
        {
            return Err(Error::InvalidArgument);
        }
        if let Some(context) = shared_context {
            context.coordinates()?;
        }
        let limit = request.page.limit()?;
        let tool = if pattern.is_some() {
            "markdown.search"
        } else {
            "markdown.find"
        };
        let args = json!([
            request.sources,
            request.source_path_glob,
            request.path_glob,
            pattern,
            limit
        ]);
        let pagination = self.pagination(tool, &args, request.page.cursor.as_deref())?;
        let (start, ordinal, byte) = match (&pagination.position, &pagination.scope) {
            (None, None) => (0, 0, 0),
            (Some(Position::Search { path, byte }), None) => {
                ((path >> 32) as usize, path & u32::MAX as u64, *byte)
            }
            _ => return Err(Error::CursorInvalid),
        };
        if start >= request.sources.len() {
            return Err(Error::CursorInvalid);
        }
        let filters = Arc::new(Filters {
            source: request
                .source_path_glob
                .map(|glob| Matcher::compile(Pattern::Glob(glob)))
                .transpose()?,
            output: request
                .path_glob
                .map(|glob| Matcher::compile(Pattern::Glob(glob)))
                .transpose()?,
        });
        let global = Arc::new(global);
        let mut approvals = Vec::with_capacity(request.sources.len());
        for source in &request.sources {
            approvals.push(match source {
                MarkdownSource::GitMarkdown { context } => {
                    let (repo, generation, format, commit) = context.coordinates()?;
                    let search = pattern.is_some();
                    self.fenced(
                        client,
                        repo,
                        Some(generation),
                        global.clone(),
                        move |snapshot, reader| {
                            require_format(format, &snapshot)?;
                            if search && !metadata(&snapshot)?.policy.outputs.search {
                                return Err(Error::PolicyDenied);
                            }
                            reader.with_batch(|batch| verify_commit(&snapshot, batch, commit))
                        },
                    )
                    .await?;
                    None
                }
                _ => Some(approved_snapshot(client, source, pattern.is_some()).await?),
            });
        }
        let mut scan = Scan {
            rows: vec![],
            used: 2048,
            limit,
            cursor: None,
            search: pattern,
            examined: 0,
            bytes: 0,
            omitted: false,
        };
        for (index, (source, approval)) in request
            .sources
            .iter()
            .zip(approvals)
            .enumerate()
            .skip(start)
        {
            let (after, resume) = if start == index {
                (ordinal, byte)
            } else {
                (0, 0)
            };
            if scan.search_limit() {
                scan.cursor = Some(((index as u64) << 32, 0));
                break;
            }
            let filters = filters.clone();
            let permit = global.clone();
            if let MarkdownSource::GitMarkdown { context } = source {
                let (repo, generation, format, commit) = context.coordinates()?;
                let source = source.clone();
                scan = self
                    .fenced(
                        client,
                        repo,
                        Some(generation),
                        permit,
                        move |snapshot, reader| {
                            require_format(format, &snapshot)?;
                            let meta = metadata(&snapshot)?;
                            if scan.search.is_some() && !meta.policy.outputs.search {
                                return Err(Error::PolicyDenied);
                            }
                            let mut worker = scan
                                .search
                                .as_ref()
                                .map(|p| {
                                    SearchWorker::new(
                                        reader.search_worker()?,
                                        &p.pattern,
                                        p.literal,
                                        p.ignore_case,
                                    )
                                })
                                .transpose()?;
                            let result = reader.with_batch(|batch| {
                                verify_commit(&snapshot, batch, commit)?;
                                let root = batch.commit(commit)?.tree;
                                let mut tree = Tree::new(batch, &meta.policy);
                                let mut ordinal = 0;
                                tree.visit(
                                    root,
                                    b"",
                                    MAX_DEPTH,
                                    true,
                                    &mut |batch, entry, path, notice| {
                                        if !matches!(
                                            entry.mode,
                                            TreeMode::Regular | TreeMode::Executable
                                        ) || !path.ends_with(b".md")
                                            || !filters.matches(path, path)
                                        {
                                            return Ok(true);
                                        }
                                        let current = ordinal;
                                        ordinal += 1;
                                        if current < after {
                                            return Ok(true);
                                        }
                                        let byte = if current == after { resume } else { 0 };
                                        if scan.search_limit() {
                                            scan.cursor = Some((current, byte));
                                            return Ok(false);
                                        }
                                        let doc = git_document(
                                            &source,
                                            path,
                                            entry.oid.to_string(),
                                            notice.ok_or(Error::Internal)?,
                                        )?;
                                        let raw = if worker.is_some()
                                            && batch.info(entry.oid)?.ok_or(Error::Internal)?.size
                                                <= BLOB_BYTES
                                        {
                                            Some(batch.contents(
                                                entry.oid,
                                                docs_mcp_core::git::ObjectType::Blob,
                                            )?)
                                        } else {
                                            None
                                        };
                                        scan.doc(
                                            current,
                                            byte,
                                            doc,
                                            raw.as_deref(),
                                            worker.as_mut(),
                                        )
                                    },
                                )
                            });
                            match result {
                                Err(Error::Budget) => scan.omitted = true,
                                Err(error) => return Err(error),
                                Ok(_) => (),
                            }
                            if let Some(worker) = worker {
                                worker.finish()?;
                            }
                            Ok(scan)
                        },
                    )
                    .await?;
            } else {
                let (id, digest) = source.coordinates()?;
                let notice: String = approval.ok_or(Error::Internal)?.get("notice");
                let limit = if scan.search.is_some() {
                    DOCS_PER_SEARCH as i64
                } else {
                    ROWS_PER_SCAN
                };
                let rows = store::documents(
                    client,
                    id,
                    digest,
                    after as i64,
                    limit + 1,
                    scan.search.is_some(),
                )
                .await
                .map_err(Error::from_storage)?;
                let source = source.clone();
                let force_failure = self.force_worker_failure;
                scan = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let mut worker = scan
                        .search
                        .as_ref()
                        .map(|p| {
                            SearchWorker::new(
                                spawn_search_worker(
                                    Instant::now() + REQUEST_DEADLINE,
                                    None,
                                    force_failure,
                                )?,
                                &p.pattern,
                                p.literal,
                                p.ignore_case,
                            )
                        })
                        .transpose()?;
                    for (offset, row) in rows.into_iter().enumerate() {
                        let ordinal = after + offset as u64;
                        let byte = if offset == 0 { resume } else { 0 };
                        if offset as i64 == limit || scan.search_limit() {
                            scan.cursor = Some((ordinal, byte));
                            break;
                        }
                        let path: &str = row.get("path");
                        let source_path: &str = row.get("source_path");
                        if !filters.matches(source_path.as_bytes(), path.as_bytes()) {
                            continue;
                        }
                        let doc = rustdoc_document(&source, &row, &notice)?;
                        if !scan.doc(ordinal, byte, doc, row.get("body"), worker.as_mut())? {
                            break;
                        }
                    }
                    if let Some(worker) = worker {
                        worker.finish()?;
                    }
                    Ok::<_, Error>(scan)
                })
                .await
                .map_err(|_| Error::Internal)??;
            }
            if let Some((ordinal, byte)) = scan.cursor.as_mut() {
                *ordinal |= (index as u64) << 32;
                if (*ordinal, *byte) <= (((start as u64) << 32) | after, resume) {
                    return Err(Error::Budget);
                }
                break;
            }
        }
        for source in &request.sources {
            if !matches!(source, MarkdownSource::GitMarkdown { .. }) {
                approved_snapshot(client, source, scan.search.is_some()).await?;
            }
        }
        let cursor = scan
            .cursor
            .map(|(path, byte)| pagination.encode(None, Position::Search { path, byte }))
            .transpose()?;
        let count = scan.rows.len();
        let complete = cursor.is_none() && !scan.omitted;
        let mut result = json!({"next_cursor":cursor,
            "coverage":coverage(complete, cursor.is_none() && scan.omitted, count, None)});
        let field = if scan.search.is_some() {
            result["scan_complete"] = json!(complete);
            "matches"
        } else {
            "entries"
        };
        result[field] = json!(scan.rows);
        Ok(result)
    }

    pub(super) async fn markdown_read(
        &self,
        client: &mut Client,
        request: MarkdownRead,
        global: OwnedSemaphorePermit,
    ) -> Result<Value, Error> {
        let path = request
            .document
            .path
            .decode()
            .map_err(|_| Error::InvalidArgument)?;
        if !path.ends_with(b".md") {
            return Err(Error::ObjectUnavailable);
        }
        let args = json!([request.document, request.range, request.encoding]);
        let pagination = self.pagination("markdown.read", &args, request.cursor.as_deref())?;
        let source = &request.document.source;
        if let MarkdownSource::GitMarkdown { context } = source {
            let (repo, generation, format, commit) = context.coordinates()?;
            return self
                .fenced(
                    client,
                    repo,
                    Some(generation),
                    global,
                    move |snapshot, reader| {
                        require_format(format, &snapshot)?;
                        let meta = metadata(&snapshot)?;
                        reader.with_batch(|batch| {
                            verify_commit(&snapshot, batch, commit)?;
                            let root = batch.commit(commit)?.tree;
                            let mut tree = Tree::new(batch, &meta.policy);
                            let (entry, notice) = tree
                                .approved_leaf(root, &path)?
                                .ok_or(Error::ObjectUnavailable)?;
                            if !matches!(entry.mode, TreeMode::Regular | TreeMode::Executable) {
                                return Err(Error::ObjectUnavailable);
                            }
                            let raw = tree.blob(entry.oid)?;
                            let base = git_document(
                                &request.document.source,
                                &path,
                                entry.oid.to_string(),
                                notice,
                            )?;
                            blob::page(
                                base,
                                &raw,
                                request.range,
                                request.encoding,
                                &pagination,
                                Some(scope(meta, Some(commit), &pagination)),
                            )
                        })
                    },
                )
                .await;
        }
        let (id, digest) = source.coordinates()?;
        let snapshot = approved_snapshot(client, source, false).await?;
        let path = std::str::from_utf8(&path).map_err(|_| Error::ObjectUnavailable)?;
        let row = store::document(client, id, digest, path)
            .await
            .map_err(Error::from_storage)?
            .ok_or(Error::ObjectUnavailable)?;
        let base = rustdoc_document(source, &row, row_notice(&snapshot))?;
        let result = blob::page(
            base,
            row.get("body"),
            request.range,
            request.encoding,
            &pagination,
            None,
        )?;
        approved_snapshot(client, source, false).await?;
        Ok(result)
    }
}

fn row_notice(row: &Row) -> &str {
    row.get("notice")
}

fn row_context(row: &Row) -> Option<Context> {
    Some(Context {
        repository_id: row
            .get::<_, Option<uuid::Uuid>>("repository_id")?
            .to_string(),
        generation_id: row.get::<_, Option<i64>>("generation_id")?.to_string(),
        object_format: row.get::<_, Option<String>>("object_format")?,
        commit_oid: row.get::<_, Option<String>>("commit_oid")?,
    })
}

async fn approved_snapshot(
    client: &Client,
    source: &MarkdownSource,
    search: bool,
) -> Result<Row, Error> {
    let (id, digest) = source.coordinates()?;
    let row = store::snapshot(client, id, digest)
        .await
        .map_err(Error::from_storage)?
        .ok_or(Error::ObjectUnavailable)?;
    if row_context(&row).as_ref() != source.context() {
        return Err(Error::ObjectUnavailable);
    }
    if search && !row.get::<_, bool>("search") {
        return Err(Error::PolicyDenied);
    }
    Ok(row)
}

fn git_document(
    source: &MarkdownSource,
    path: &[u8],
    oid: String,
    notice: &str,
) -> Result<Value, Error> {
    let path = BytePath::encode(path).map_err(|_| Error::UnsupportedEncoding)?;
    Ok(
        json!({"document":MarkdownDocument { source:source.clone(), path:path.clone() },
        "source_path":path, "oid":oid, "notice":notice}),
    )
}

fn rustdoc_document(source: &MarkdownSource, row: &Row, notice: &str) -> Result<Value, Error> {
    let path =
        BytePath::encode(row.get::<_, &str>("path").as_bytes()).map_err(|_| Error::Internal)?;
    let origin = BytePath::encode(row.get::<_, &str>("source_path").as_bytes())
        .map_err(|_| Error::Internal)?;
    Ok(
        json!({"document":MarkdownDocument { source:source.clone(), path },
        "source_path":origin, "notice":notice, "item":row.get::<_, &str>("item"),
        "kind":row.get::<_, &str>("kind"), "source_line":row.get::<_, i64>("line")}),
    )
}
