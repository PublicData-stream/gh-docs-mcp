use std::io;

use docs_mcp_core::git::{BytePath, ByteString, ObjectType};
use docs_mcp_git::reader::{Reader, TreeMode};
use docs_mcp_storage::publication::Generation;
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;
use tokio_postgres::Client;

use crate::worker::{BLOB_BYTES, MATCH_LIMIT, PATTERN_BYTES, RECORD_FIELDS};

use super::{
    Error, Explorer, Pattern, RESPONSE_BYTES, Rg, TreeQuery,
    cursor::{Pagination, Position},
    matcher::Matcher,
    metadata,
    page::json_size,
    pinned, require_format, scope, summary,
    tree::{MAX_DEPTH, Tree},
    verify_commit,
};

const BLOBS_PER_PAGE: usize = 32;
const EXCERPT_BYTES: usize = 4096;

impl Explorer {
    pub(super) async fn rg(
        &self,
        client: &mut Client,
        request: Rg,
        global: OwnedSemaphorePermit,
    ) -> Result<Value, Error> {
        let (repo, generation, format, commit) = request.context.coordinates()?;
        if request.pattern.len() > PATTERN_BYTES || request.context_lines > 3 {
            return Err(Error::InvalidArgument);
        }
        let root = TreeQuery::root(request.root.clone(), MAX_DEPTH)?;
        let matcher = request
            .path_glob
            .clone()
            .map(|glob| Matcher::compile(Pattern::Glob(glob)))
            .transpose()?;
        let limit = request.page.limit()?;
        let args = json!([
            request.context,
            request.pattern,
            request.literal,
            request.ignore_case,
            request.root,
            request.path_glob,
            request.context_lines,
            limit
        ]);
        let pagination = self.pagination("git.rg", &args, request.page.cursor.as_deref())?;
        pinned(&pagination, repo, Some(generation), Some(commit))?;
        self.fenced(
            client,
            repo,
            Some(generation),
            global,
            move |snapshot, reader| {
                require_format(format, &snapshot)?;
                search(
                    &snapshot,
                    reader,
                    request,
                    &root,
                    matcher.as_ref(),
                    &pagination,
                    limit,
                )
            },
        )
        .await
    }
}

fn search(
    snapshot: &Generation,
    reader: &Reader,
    request: Rg,
    root: &[u8],
    matcher: Option<&Matcher>,
    pagination: &Pagination,
    limit: usize,
) -> Result<Value, Error> {
    let meta = metadata(snapshot)?;
    if !meta.policy.outputs.search {
        return Err(Error::PolicyDenied);
    }
    let (_, _, _, commit) = request.context.coordinates()?;
    let scope = scope(meta, Some(commit), pagination);
    let (resume_path, resume_byte) = match (&pagination.position, &pagination.scope) {
        (None, None) => (0, 0),
        (Some(Position::Search { path, byte }), Some(_)) => (*path, *byte),
        _ => return Err(Error::CursorInvalid),
    };
    let mut base = json!({"context":request.context, "source":summary(meta, scope.sequence)});
    let mut used = json_size(&base)? + 4096;
    let mut results = Vec::new();
    let mut candidates = 0;
    let mut examined = 0;
    let mut charged = 0;
    let mut binary = 0;
    let mut oversized = 0;
    let mut next = None;
    let mut worker = reader.search_worker()?;
    worker.send(&[u8::from(request.literal) | (u8::from(request.ignore_case) << 1)])?;
    worker.send(&(request.pattern.len() as u32).to_be_bytes())?;
    worker.send(request.pattern.as_bytes())?;
    let mut status = [0];
    worker.read_exact(&mut status).map_err(worker_error)?;
    match status {
        [b'+'] => (),
        [b'-'] => return Err(Error::InvalidRegex),
        _ => return Err(Error::Internal),
    }
    let exhausted = reader.with_batch(|batch| {
        verify_commit(snapshot, batch, commit)?;
        let tree_oid = batch.commit(commit)?.tree;
        let mut tree = Tree::new(batch, &meta.policy);
        let tree_oid = tree.directory(tree_oid, root)?;
        let outcome = tree.visit(
            tree_oid,
            root,
            MAX_DEPTH,
            true,
            &mut |batch, entry, path, notice| {
                if !matches!(entry.mode, TreeMode::Regular | TreeMode::Executable)
                    || matcher.is_some_and(|matcher| !matcher.matches(path))
                {
                    return Ok(true);
                }
                let ordinal = candidates;
                candidates += 1;
                if ordinal < resume_path {
                    return Ok(true);
                }
                let byte = if ordinal == resume_path {
                    resume_byte
                } else {
                    0
                };
                if examined == BLOBS_PER_PAGE {
                    next = Some((ordinal, byte));
                    return Ok(false);
                }
                let info = batch.info(entry.oid)?.ok_or(Error::Internal)?;
                if info.kind != ObjectType::Blob {
                    return Err(Error::Internal);
                }
                if info.size > BLOB_BYTES {
                    examined += 1;
                    oversized += 1;
                    return Ok(true);
                }
                // Stop at a reproducible position before exhausting the shared Git reader budget.
                if charged + info.size > 120 * 1024 * 1024 {
                    next = Some((ordinal, byte));
                    return Ok(false);
                }
                let raw = batch.contents(entry.oid, ObjectType::Blob)?;
                examined += 1;
                charged += raw.len();
                if raw.contains(&0) {
                    binary += 1;
                    return Ok(true);
                }
                worker.send(&[1]).map_err(worker_error)?;
                worker.send(&byte.to_be_bytes()).map_err(worker_error)?;
                worker
                    .send(&(raw.len() as u32).to_be_bytes())
                    .map_err(worker_error)?;
                worker.send(&raw).map_err(worker_error)?;
                let mut count = [0; 4];
                worker.read_exact(&mut count).map_err(worker_error)?;
                let count = u32::from_be_bytes(count) as usize;
                if count > MATCH_LIMIT {
                    return Err(Error::Internal);
                }
                let mut previous = None;
                for _ in 0..count {
                    let mut fields = [0; RECORD_FIELDS];
                    for field in &mut fields {
                        let mut value = [0; 4];
                        worker.read_exact(&mut value).map_err(worker_error)?;
                        *field = u32::from_be_bytes(value) as usize;
                    }
                    let [line, start, end, hit_start, hit_end] = fields;
                    if line == 0
                        || start > hit_start
                        || hit_start > hit_end
                        || hit_end > end
                        || end > raw.len()
                        || hit_start < byte as usize
                        || previous.is_some_and(|last| last >= hit_start)
                    {
                        return Err(Error::Internal);
                    }
                    previous = Some(hit_start);
                    // Drain the bounded frame before asking the worker to exit.
                    if next.is_some() {
                        continue;
                    }
                    let item = match_item(
                        &raw,
                        path,
                        &entry.oid.to_string(),
                        notice.ok_or(Error::Internal)?,
                        fields,
                        request.context_lines,
                    )?;
                    let size = json_size(&item)? + 3;
                    if results.len() == limit || used + size > RESPONSE_BYTES {
                        if results.is_empty() {
                            return Err(Error::Budget);
                        }
                        next = Some((ordinal, hit_start as u32));
                    } else {
                        used += size;
                        results.push(item);
                    }
                }
                Ok(next.is_none())
            },
        );
        match outcome {
            Ok(_) => Ok(false),
            Err(Error::Budget) => Ok(true),
            Err(error) => Err(error),
        }
    })?;
    worker.finish().map_err(worker_error)?;
    if candidates <= resume_path && pagination.position.is_some() && !exhausted {
        return Err(Error::CursorInvalid);
    }
    let next = next.filter(|&state| state > (resume_path, resume_byte));
    let complete = !exhausted && next.is_none() && oversized == 0;
    let scan_complete = complete;
    let reason = if exhausted || (next.is_none() && oversized > 0) {
        Some("narrow_scope")
    } else if next.is_some() {
        Some("next_page")
    } else {
        None
    };
    base["coverage"] = json!({
        "complete":complete, "reason":reason, "returned_count":results.len(),
        "examined_blobs":examined, "examined_bytes":charged,
        "omitted":{"binary":binary, "oversized_blob":oversized},
    });
    base["scan_complete"] = json!(scan_complete);
    base["matches"] = json!(results);
    base["next_cursor"] = json!(
        next.map(|(path, byte)| pagination.encode(Some(scope), Position::Search { path, byte }))
            .transpose()?
    );
    Ok(base)
}

fn worker_error(error: io::Error) -> Error {
    match error.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe => Error::Budget,
        _ => Error::from(error),
    }
}

fn match_item(
    raw: &[u8],
    path: &[u8],
    oid: &str,
    notice: &str,
    [line, start, end, hit_start, hit_end]: [usize; RECORD_FIELDS],
    context: u8,
) -> Result<Value, Error> {
    let (mut from, mut to) = (start, end);
    for _ in 0..context {
        if from > 0 {
            from = raw[..from - 1]
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(0, |index| index + 1);
        }
        if to < raw.len() {
            to = raw[to..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(raw.len(), |index| to + index + 1);
        }
    }
    let excerpt_start = from.max(hit_start.saturating_sub(EXCERPT_BYTES / 2));
    let excerpt_end = to.min(excerpt_start + EXCERPT_BYTES);
    Ok(json!({
        "path":BytePath::encode(path).map_err(|_| Error::UnsupportedEncoding)?,
        "oid":oid, "notice":notice, "line":line,
        "line_range":{"start":start,"end":end},
        "match_range":{"start":hit_start,"end":hit_end},
        "context_range":{"start":from,"end":to},
        "excerpt_range":{"start":excerpt_start,"end":excerpt_end},
        "excerpt":ByteString::encode(&raw[excerpt_start..excerpt_end]),
    }))
}
