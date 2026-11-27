mod lines;

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashSet},
    ops::Range,
};

use docs_mcp_core::{
    exploration::{ApprovedScope, Policy},
    git::{BytePath, ByteString, Oid},
};
use docs_mcp_git::reader::{CommitLinks, TreeEntry, TreeMode};
use docs_mcp_storage::publication::Generation;
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;
use tokio_postgres::Client;

use super::{
    Blame, ByteRange, Context, Diff, Error, Explorer, Log, Page, Show,
    cursor::{Pagination, Scope},
    metadata,
    page::Entries,
    pinned, require_format, scope, summary,
    tree::{MAX_DEPTH, Tree, mode},
    verify_commit,
};

const COMMITS: usize = 2048;
const PROJECTED_BYTES: usize = 8 * 1024 * 1024;
const EXCERPT_BYTES: usize = 4096;

pub(super) enum Query {
    Log(Log),
    Show(Show),
    Diff(Diff),
    Blame(Blame),
}

struct Request<'a> {
    tool: &'static str,
    context: &'a Context,
    page: &'a Page,
    path: Option<&'a BytePath>,
    options: Value,
}

impl Query {
    fn arguments(&self) -> Request<'_> {
        match self {
            Self::Log(arg) => Request {
                tool: "git.log",
                context: &arg.context,
                page: &arg.page,
                path: arg.path.as_ref(),
                options: Value::Null,
            },
            Self::Show(arg) => Request {
                tool: "git.show",
                context: &arg.context,
                page: &arg.page,
                path: arg.path.as_ref(),
                options: json!([arg.parent, arg.context_lines.unwrap_or(3)]),
            },
            Self::Diff(arg) => Request {
                tool: "git.diff",
                context: &arg.head,
                page: &arg.page,
                path: arg.path.as_ref(),
                options: json!([arg.base, arg.context_lines.unwrap_or(3)]),
            },
            Self::Blame(arg) => Request {
                tool: "git.blame",
                context: &arg.context,
                page: &arg.page,
                path: Some(&arg.path),
                options: json!(arg.line_range),
            },
        }
    }
}

impl Explorer {
    pub(super) async fn history(
        &self,
        client: &mut Client,
        query: Query,
        global: OwnedSemaphorePermit,
    ) -> Result<Value, Error> {
        let args = query.arguments();
        let (repo, generation, format, commit) = args.context.coordinates()?;
        if let Query::Diff(diff) = &query {
            let (base_repo, base_generation, base_format, _) = diff.base.coordinates()?;
            if (base_repo, base_generation, base_format) != (repo, generation, format) {
                return Err(Error::InvalidArgument);
            }
        }
        let path = args
            .path
            .map(|path| path.decode().map_err(|_| Error::InvalidArgument))
            .transpose()?;
        let limit = args.page.limit()?;
        let binding = json!([args.context, args.path, args.options, limit]);
        let pagination = self.pagination(args.tool, &binding, args.page.cursor.as_deref())?;
        pinned(&pagination, repo, Some(generation), Some(commit))?;
        let context = args.context.clone();
        self.fenced(
            client,
            repo,
            Some(generation),
            global,
            move |snapshot, reader| {
                require_format(format, &snapshot)?;
                let meta = metadata(&snapshot)?;
                if !meta.policy.outputs.history {
                    return Err(Error::PolicyDenied);
                }
                if path
                    .as_ref()
                    .is_some_and(|path| !meta.policy.scope.allows_leaf_path(path))
                {
                    return Err(Error::ObjectUnavailable);
                }
                let scope = scope(meta, Some(commit), &pagination);
                let base = json!({"context":context,"source":summary(meta, scope.sequence)});
                let entries = Entries::new(limit, pagination.offset()?, &base)?;
                reader.with_batch(|batch| {
                    verify_commit(&snapshot, batch, commit)?;
                    let history = History {
                        tree: Tree::new(batch, &meta.policy),
                        policy: &meta.policy,
                        entries,
                        work: lines::Work::default(),
                        omitted: 0,
                        examined: 0,
                    };
                    history.run(
                        &snapshot,
                        query,
                        commit,
                        path.as_deref(),
                        base,
                        &pagination,
                        scope,
                    )
                })
            },
        )
        .await
    }
}

struct History<'a, 'reader> {
    tree: Tree<'a, 'reader>,
    policy: &'a Policy,
    entries: Entries,
    work: lines::Work,
    omitted: usize,
    examined: usize,
}

impl History<'_, '_> {
    #[allow(clippy::too_many_arguments)]
    fn run(
        mut self,
        snapshot: &Generation,
        query: Query,
        commit: Oid,
        path: Option<&[u8]>,
        mut base: Value,
        pagination: &Pagination,
        scope: Scope,
    ) -> Result<Value, Error> {
        let result = match query {
            Query::Log(_) => self.log(commit, path),
            Query::Show(args) => {
                let (links, raw) = self.tree.batch().commit_bytes(commit)?;
                base["commit"] = commit_metadata(commit, &links, &raw)?;
                self.entries.reserve(&base["commit"])?;
                let parent = match (args.parent, links.parents.first()) {
                    (None, parent) => parent.copied(),
                    (Some(0), _) => return Err(Error::InvalidArgument),
                    (Some(index), _) => Some(
                        *links
                            .parents
                            .get(index as usize - 1)
                            .ok_or(Error::InvalidArgument)?,
                    ),
                };
                let previous = parent
                    .map(|oid| self.tree.batch().commit(oid))
                    .transpose()?;
                base["parent"] = json!(parent.map(|oid| oid.to_string()));
                self.diff(
                    previous.map(|parent| parent.tree),
                    links.tree,
                    path,
                    context_lines(args.context_lines)?,
                )
            }
            Query::Diff(args) => {
                let (_, _, _, old_commit) = args.base.coordinates()?;
                verify_commit(snapshot, self.tree.batch(), old_commit)?;
                base["base"] = json!(args.base);
                self.entries.reserve(&base["base"])?;
                let old = self.tree.batch().commit(old_commit)?.tree;
                let new = self.tree.batch().commit(commit)?.tree;
                self.diff(Some(old), new, path, context_lines(args.context_lines)?)
            }
            Query::Blame(args) => self.blame(commit, path.ok_or(Error::Internal)?, args.line_range),
        };
        let budget = match result {
            Ok(()) => false,
            Err(Error::Budget) => true,
            Err(error) => return Err(error),
        };
        let mut result = self
            .entries
            .finish(base, pagination, Some(scope), budget, |_| {
                Err(Error::Internal)
            })?;
        result["coverage"]["examined"] = json!(self.examined);
        result["coverage"]["omitted"] = json!(self.omitted);
        if self.omitted > 0 {
            result["coverage"]["complete"] = json!(false);
            if result["next_cursor"].is_null() {
                result["coverage"]["reason"] = json!("narrow_scope");
            }
        }
        Ok(result)
    }

    fn log(&mut self, commit: Oid, path: Option<&[u8]>) -> Result<(), Error> {
        let mut pending = BinaryHeap::from([Reverse((0, commit))]);
        let mut seen = HashSet::from([commit]);
        while let Some(Reverse((distance, oid))) = pending.pop() {
            self.tree.batch().check()?;
            if self.examined == COMMITS {
                return Err(Error::Budget);
            }
            self.examined += 1;
            let (links, raw) = self.tree.batch().commit_bytes(oid)?;
            let projected =
                path.is_some() || matches!(self.policy.scope, ApprovedScope::Paths { .. });
            let visible = if projected {
                let previous = links
                    .parents
                    .first()
                    .map(|oid| self.tree.batch().commit(*oid))
                    .transpose()?;
                self.changed(previous.map(|links| links.tree), links.tree, path)?
            } else {
                true
            };
            if visible && !self.entries.push(commit_metadata(oid, &links, &raw)?)? {
                return Ok(());
            }
            for parent in links.parents {
                if seen.insert(parent) {
                    if seen.len() > 50_000 {
                        return Err(Error::Budget);
                    }
                    pending.push(Reverse((distance + 1, parent)));
                }
            }
        }
        Ok(())
    }

    fn changed(&mut self, old: Option<Oid>, new: Oid, path: Option<&[u8]>) -> Result<bool, Error> {
        if old == Some(new) {
            return Ok(false);
        }
        Ok(self.project(old, path)? != self.project(Some(new), path)?)
    }

    fn approved_leaf(&mut self, tree: Oid, path: &[u8]) -> Result<Option<File>, Error> {
        Ok(self
            .tree
            .approved_leaf(tree, path)?
            .map(|(entry, _)| File::from(entry)))
    }

    fn project(
        &mut self,
        root: Option<Oid>,
        path: Option<&[u8]>,
    ) -> Result<BTreeMap<Vec<u8>, File>, Error> {
        let mut files = BTreeMap::new();
        let Some(root) = root else { return Ok(files) };
        if let Some(path) = path {
            if let Some(file) = self.approved_leaf(root, path)? {
                files.insert(path.to_vec(), file);
            }
            return Ok(files);
        }
        let mut remaining = PROJECTED_BYTES;
        self.tree
            .visit(root, b"", MAX_DEPTH, true, &mut |_, entry, path, _| {
                if entry.mode != TreeMode::Directory {
                    remaining = remaining
                        .checked_sub(path.len() + 128)
                        .ok_or(Error::Budget)?;
                    files.insert(path.to_vec(), File::from(entry.clone()));
                }
                Ok(true)
            })?;
        Ok(files)
    }

    fn diff(
        &mut self,
        old: Option<Oid>,
        new: Oid,
        path: Option<&[u8]>,
        context: usize,
    ) -> Result<(), Error> {
        if old == Some(new) {
            return Ok(());
        }
        let old_files = self.project(old, path)?;
        let new_files = self.project(Some(new), path)?;
        let mut paths = old_files.keys().chain(new_files.keys()).collect::<Vec<_>>();
        paths.sort_unstable();
        paths.dedup();
        for path in paths {
            self.tree.batch().check()?;
            let old = old_files.get(path);
            let new = new_files.get(path);
            if old == new {
                continue;
            }
            self.examined += 1;
            let mode = &new.or(old).ok_or(Error::Internal)?.mode;
            let notice = self.tree.notice(path, mode).ok_or(Error::Internal)?;
            let base = json!({
                "path":BytePath::encode(path).map_err(|_| Error::UnsupportedEncoding)?,
                "notice":notice, "old":self.file_metadata(old)?, "new":self.file_metadata(new)?,
            });
            if !self.file_changes(base, old, new, context)? {
                break;
            }
        }
        Ok(())
    }

    fn file_metadata(&mut self, file: Option<&File>) -> Result<Value, Error> {
        let Some(file) = file else {
            return Ok(Value::Null);
        };
        let (mode, kind) = mode(&file.mode);
        let bytes = if file.mode == TreeMode::Gitlink {
            None
        } else {
            Some(
                self.tree
                    .batch()
                    .info(file.oid)?
                    .ok_or(Error::Internal)?
                    .size,
            )
        };
        Ok(json!({"mode":mode,"kind":kind,"oid":file.oid.to_string(),"size":bytes}))
    }

    fn file_changes(
        &mut self,
        base: Value,
        old: Option<&File>,
        new: Option<&File>,
        context: usize,
    ) -> Result<bool, Error> {
        let files = [old, new];
        if files.into_iter().flatten().any(|file| !file.text_mode()) {
            return self.entries.push(tagged(base, "metadata"));
        }
        if matches!((old, new), (Some(old), Some(new)) if old.oid == new.oid) {
            return self.entries.push(tagged(base, "metadata"));
        }
        match self.text_changes(&base, old, new, context) {
            Err(Error::Budget) => {
                self.omitted += 1;
                self.entries.push(tagged(base, "omitted_budget"))
            }
            result => result,
        }
    }

    fn text_changes(
        &mut self,
        base: &Value,
        old: Option<&File>,
        new: Option<&File>,
        context: usize,
    ) -> Result<bool, Error> {
        let old = old
            .map(|file| self.tree.blob(file.oid))
            .transpose()?
            .unwrap_or_default();
        let new = new
            .map(|file| self.tree.blob(file.oid))
            .transpose()?
            .unwrap_or_default();
        if old.contains(&0) || new.contains(&0) {
            return self.entries.push(tagged(base.clone(), "binary"));
        }
        let old_lines = lines::ranges(&old)?;
        let new_lines = lines::ranges(&new)?;
        let mut pairs = lines::matching(
            &old,
            &old_lines,
            &new,
            &new_lines,
            &mut self.work,
            self.tree.batch(),
        )?;
        pairs.push((old_lines.len(), new_lines.len()));
        let (mut last_old, mut last_new) = (0, 0);
        let mut hunks: Vec<(Range<usize>, Range<usize>, Vec<_>)> = Vec::new();
        for (old_line, new_line) in pairs {
            if old_line != last_old || new_line != last_new {
                let change = [last_old, old_line, last_new, new_line];
                let old_range =
                    last_old.saturating_sub(context)..(old_line + context).min(old_lines.len());
                let new_range =
                    last_new.saturating_sub(context)..(new_line + context).min(new_lines.len());
                if let Some((o, n, changes)) = hunks
                    .last_mut()
                    .filter(|(o, _, _)| o.end >= old_range.start)
                {
                    o.end = old_range.end;
                    n.end = new_range.end;
                    changes.push(change);
                } else {
                    hunks.push((old_range, new_range, vec![change]));
                }
            }
            (last_old, last_new) = (old_line + 1, new_line + 1);
        }
        if hunks.is_empty() {
            return self.entries.push(tagged(base.clone(), "metadata"));
        }
        for (old_range, new_range, changes) in hunks {
            // Keep a single hunk small enough for the duplicated wire envelope.
            let mut item = tagged(base.clone(), "hunk");
            let old_excerpt = excerpt(&old, &old_lines, old_range);
            let new_excerpt = excerpt(&new, &new_lines, new_range);
            let truncated = old_excerpt["truncated"] == true
                || new_excerpt["truncated"] == true
                || changes.len() > 64;
            if truncated {
                self.omitted += 1;
            }
            item["hunk"] = json!({"old":old_excerpt,"new":new_excerpt,"truncated":truncated,
                "changes":changes.iter().take(64).map(|[os,oe,ns,ne]| json!({"old":[os+1,oe+1],"new":[ns+1,ne+1]})).collect::<Vec<_>>()});
            if !self.entries.push(item)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn blame(&mut self, commit: Oid, path: &[u8], range: Option<ByteRange>) -> Result<(), Error> {
        let (mut links, mut raw_commit) = self.tree.batch().commit_bytes(commit)?;
        let Some((entry, notice)) = self.tree.approved_leaf(links.tree, path)? else {
            return Err(Error::ObjectUnavailable);
        };
        let mut file = File::from(entry);
        if !file.text_mode() {
            return Err(Error::ObjectUnavailable);
        }
        let initial_oid = file.oid.to_string();
        let blob = self.tree.blob(file.oid)?;
        if blob.contains(&0) {
            return Err(Error::UnsupportedEncoding);
        }
        let initial_lines = lines::ranges(&blob)?;
        let mut raw = None;
        let mut prior_ranges = None;
        let range = range.unwrap_or(ByteRange {
            start: 1,
            end: initial_lines.len() as u64 + 1,
        });
        let first = range.start.checked_sub(1).ok_or(Error::InvalidArgument)?;
        let last = range.end.checked_sub(1).ok_or(Error::InvalidArgument)?;
        if first > last || last > initial_lines.len() as u64 {
            return Err(Error::InvalidArgument);
        }
        let page_start = first + self.entries.resume_offset();
        if page_start > last {
            return Err(Error::CursorInvalid);
        }
        let page_end = last.min(page_start + self.entries.limit() as u64 + 1);
        let target = page_start as usize..page_end as usize;
        let mut positions: Vec<_> = target.clone().map(Some).collect();
        let mut origins = vec![Value::Null; positions.len()];
        let mut at = commit;
        for _ in 0..COMMITS {
            self.tree.batch().check()?;
            self.examined += 1;
            let step = self.blame_step(
                &links,
                path,
                &file,
                raw.as_deref().unwrap_or(&blob),
                prior_ranges.as_deref().unwrap_or(initial_lines.as_slice()),
            );
            let next = match step {
                Ok(next) => next,
                Err(Error::Budget) => break,
                Err(error) => return Err(error),
            };
            let ranges = prior_ranges.as_deref().unwrap_or(initial_lines.as_slice());
            let mut metadata = None;
            for (position, origin) in positions.iter_mut().zip(&mut origins) {
                let Some(line) = *position else { continue };
                let found = next.as_ref().and_then(|next| match &next.contents {
                    None => Some(line),
                    Some((_, _, pairs)) => pairs
                        .binary_search_by_key(&line, |&(_, new)| new)
                        .ok()
                        .map(|i| pairs[i].0),
                });
                if found.is_none() {
                    if metadata.is_none() {
                        metadata = Some(commit_metadata(at, &links, &raw_commit)?);
                    }
                    *origin = json!({"commit":metadata,"oid":file.oid.to_string(),"line":line+1,
                        "byte_range":{"start":ranges[line].start,"end":ranges[line].end}});
                }
                *position = found;
            }
            let Some(next) = next.filter(|_| positions.iter().any(Option::is_some)) else {
                break;
            };
            at = next.oid;
            links = next.links;
            raw_commit = next.raw_commit;
            file = next.file;
            if let Some((bytes, ranges, _)) = next.contents {
                raw = Some(bytes);
                prior_ranges = Some(ranges);
            }
        }
        for (line, origin) in target.zip(origins) {
            let content = excerpt(&blob, &initial_lines, line..line + 1);
            if origin.is_null() || content["truncated"] == true {
                self.omitted += 1;
            }
            if !self.entries.push_resumed(json!({"path":BytePath::encode(path).map_err(|_| Error::UnsupportedEncoding)?,
                "oid":initial_oid, "notice":notice, "line":line+1, "content":content, "origin":origin}))? {
                break;
            }
        }
        Ok(())
    }

    fn blame_step(
        &mut self,
        links: &CommitLinks,
        path: &[u8],
        file: &File,
        raw: &[u8],
        ranges: &[Range<usize>],
    ) -> Result<Option<BlameStep>, Error> {
        let Some(&oid) = links.parents.first() else {
            return Ok(None);
        };
        let (links, raw_commit) = self.tree.batch().commit_bytes(oid)?;
        let Some(parent) = self
            .approved_leaf(links.tree, path)?
            .filter(File::text_mode)
        else {
            return Ok(None);
        };
        let contents = if parent.oid == file.oid {
            None
        } else {
            let previous = self.tree.blob(parent.oid)?;
            if previous.contains(&0) {
                return Ok(None);
            }
            let parent_ranges = lines::ranges(&previous)?;
            let matches = lines::matching(
                &previous,
                &parent_ranges,
                raw,
                ranges,
                &mut self.work,
                self.tree.batch(),
            )?;
            Some((previous, parent_ranges, matches))
        };
        Ok(Some(BlameStep {
            oid,
            links,
            raw_commit,
            file: parent,
            contents,
        }))
    }
}

type LineComparison = (Vec<u8>, Vec<Range<usize>>, Vec<(usize, usize)>);

struct BlameStep {
    oid: Oid,
    links: CommitLinks,
    raw_commit: Vec<u8>,
    file: File,
    contents: Option<LineComparison>,
}

#[derive(PartialEq, Eq)]
struct File {
    mode: TreeMode,
    oid: Oid,
}

impl From<TreeEntry> for File {
    fn from(entry: TreeEntry) -> Self {
        Self {
            mode: entry.mode,
            oid: entry.oid,
        }
    }
}

impl File {
    fn text_mode(&self) -> bool {
        matches!(self.mode, TreeMode::Regular | TreeMode::Executable)
    }
}

fn context_lines(value: Option<u8>) -> Result<usize, Error> {
    let value = value.unwrap_or(3);
    if value > 3 {
        return Err(Error::InvalidArgument);
    }
    Ok(usize::from(value))
}

fn tagged(mut base: Value, kind: &str) -> Value {
    base["kind"] = json!(kind);
    base
}

fn excerpt(raw: &[u8], lines: &[Range<usize>], range: Range<usize>) -> Value {
    let offset = |index| {
        lines
            .get(index)
            .map_or(raw.len(), |line: &Range<usize>| line.start)
    };
    let start = offset(range.start);
    let end = offset(range.end);
    let returned_end = end.min(start + EXCERPT_BYTES);
    json!({
        "line_range":{"start":range.start+1,"end":range.end+1},
        "byte_range":{"start":start,"end":end},
        "returned_range":{"start":start,"end":returned_end},
        "truncated":returned_end != end,
        "bytes":ByteString::encode(&raw[start..returned_end]),
    })
}

fn commit_metadata(oid: Oid, links: &CommitLinks, raw: &[u8]) -> Result<Value, Error> {
    let mut author = None;
    let mut committer = None;
    for line in raw
        .split(|&byte| byte == b'\n')
        .take_while(|line| !line.is_empty())
    {
        if let Some(line) = line.strip_prefix(b"author ") {
            author = Some(identity(line)?);
        } else if let Some(line) = line.strip_prefix(b"committer ") {
            committer = Some(identity(line)?);
        }
    }
    Ok(json!({"oid":oid.to_string(),
        "parents":links.parents.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "author":author.ok_or(Error::Internal)?,"committer":committer.ok_or(Error::Internal)?}))
}

fn identity(line: &[u8]) -> Result<Value, Error> {
    let close = line
        .iter()
        .rposition(|&byte| byte == b'>')
        .ok_or(Error::Internal)?;
    let open = line[..close]
        .iter()
        .rposition(|&byte| byte == b'<')
        .ok_or(Error::Internal)?;
    let name = line[..open].strip_suffix(b" ").unwrap_or(&line[..open]);
    let email = &line[open + 1..close];
    let time = std::str::from_utf8(
        line[close + 1..]
            .strip_prefix(b" ")
            .ok_or(Error::Internal)?,
    )
    .map_err(|_| Error::Internal)?;
    let (seconds, timezone) = time.split_once(' ').ok_or(Error::Internal)?;
    let seconds: i64 = seconds.parse().map_err(|_| Error::Internal)?;
    if timezone.len() != 5
        || !matches!(timezone.as_bytes()[0], b'+' | b'-')
        || !timezone.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(Error::Internal);
    }
    Ok(
        json!({"name":ByteString::encode(name),"email":ByteString::encode(email),
        "unix_seconds":seconds.to_string(),"timezone":timezone}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_header_preserves_raw_names_and_ignores_message() {
        let oid = Oid::Sha1([0; 20]);
        let links = CommitLinks {
            tree: oid,
            parents: vec![],
        };
        let raw = b"tree test\nauthor Jos\xff <a@example.test> 100 +0230\ncommitter C <c@example.test> 101 -0100\n\nauthor hidden <hidden@example.test> 44 +0000";
        let result = commit_metadata(oid, &links, raw).unwrap();
        assert_eq!(result["author"]["name"]["encoding"], "base64");
        assert_eq!(result["author"]["timezone"], "+0230");
        assert_eq!(result["committer"]["unix_seconds"], "101");
        assert!(!result.to_string().contains("hidden"));
    }
}
