//! Typed, bounded reads from one validated bare Git object store.

use std::{
    collections::HashSet,
    io::{self, BufRead, Read},
    os::unix::process::CommandExt,
    process::Command,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use gh_docs_core::git::{ObjectFormat, ObjectType, Oid, RepositoryId, validate_component};

use crate::{
    process::{Process, ensure_time},
    sandbox,
    store::{self, Fence, Location, LockMode, LockScope},
};

const READER: &str = "/usr/local/bin/gh-docs-git-reader";
const BLOB_BYTES: usize = 8 * 1024 * 1024;
const TOTAL_BYTES: usize = 128 * 1024 * 1024;
const TOTAL_OBJECTS: usize = 50_000;
const TAG_DEPTH: usize = 16;
const WALL_LIMIT: Duration = Duration::from_secs(15);

pub struct Reader {
    location: Location,
    format: ObjectFormat,
    deadline: Instant,
    force_failure: bool,
    _fences: Vec<Fence>,
    cancellation: Option<Arc<AtomicBool>>,
}

impl Reader {
    pub fn open(location: Location, format: ObjectFormat) -> io::Result<Self> {
        let deadline = Instant::now() + WALL_LIMIT;
        let fence = match location {
            Location::Canonical(repo) => Some(store::lock(
                repo,
                LockScope::Repository,
                LockMode::Shared,
                deadline,
            )?),
            Location::Incoming { .. } => None,
        };
        Ok(Self {
            location,
            format,
            deadline,
            force_failure: false,
            _fences: fence.into_iter().collect(),
            cancellation: None,
        })
    }

    /// Construct a canonical reader with the matching repository and generation fences.
    pub fn fenced(
        repo: RepositoryId,
        format: ObjectFormat,
        repository: Fence,
        generation: Fence,
        cancellation: Arc<AtomicBool>,
    ) -> Self {
        Self {
            location: Location::Canonical(repo),
            format,
            deadline: Instant::now() + WALL_LIMIT,
            force_failure: false,
            _fences: vec![generation, repository],
            cancellation: Some(cancellation),
        }
    }

    #[doc(hidden)]
    pub fn force_setup_failure_for_probe(mut self) -> Self {
        self.force_failure = true;
        self
    }

    pub fn batch<T>(&self, operation: impl FnOnce(&mut Batch) -> io::Result<T>) -> io::Result<T> {
        let mut batch = Batch {
            process: self.spawn(None)?,
            format: self.format,
            bytes_left: TOTAL_BYTES,
            objects_left: TOTAL_OBJECTS,
        };
        let result = operation(&mut batch)?;
        batch.process.finish()?;
        Ok(result)
    }

    pub fn tree(&self, tree: Oid) -> io::Result<Vec<TreeEntry>> {
        require_format(self.format, tree)?;
        self.batch(|batch| match batch.info(tree)? {
            Some(ObjectInfo {
                kind: ObjectType::Tree,
                ..
            }) => Ok(()),
            _ => Err(invalid("expected a Git tree object")),
        })?;
        let mut process = self.spawn(Some(tree))?;
        let mut output = Vec::new();
        let mut names = HashSet::new();
        let mut bytes_left = TOTAL_BYTES;
        while let Some(record) = frame(&mut process.output, 4250)? {
            bytes_left = bytes_left.checked_sub(record.len()).ok_or_else(budget)?;
            if output.len() >= TOTAL_OBJECTS {
                return Err(budget());
            }
            let entry = parse_tree(self.format, &record)?;
            if !names.insert(entry.name.clone()) {
                return Err(invalid("duplicate Git tree entry"));
            }
            output.push(entry);
        }
        process.finish()?;
        Ok(output)
    }

    fn spawn(&self, tree: Option<Oid>) -> io::Result<Process> {
        let mut command = Command::new(READER);
        command.env_clear();
        match self.location {
            Location::Canonical(repo) => command.args(["canonical", &repo.to_string()]),
            Location::Incoming { job } => command.args(["incoming", &job.to_string()]),
        };
        command.arg(self.format.name());
        if let Some(tree) = tree {
            command.args(["tree", &tree.to_string()]);
        } else {
            command.arg("batch");
        }
        if self.force_failure {
            command.arg("--fail-setup");
        }
        Process::spawn(command, self.deadline, self.cancellation.clone())
    }
}

pub struct Batch {
    process: Process,
    format: ObjectFormat,
    bytes_left: usize,
    objects_left: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectInfo {
    pub kind: ObjectType,
    pub size: usize,
}

impl Batch {
    pub fn info(&mut self, oid: Oid) -> io::Result<Option<ObjectInfo>> {
        require_format(self.format, oid)?;
        self.objects_left = self.objects_left.checked_sub(1).ok_or_else(budget)?;
        self.process.send(format!("info {oid}\0").as_bytes())?;
        self.header(oid)
    }

    pub fn contents(&mut self, oid: Oid, kind: ObjectType) -> io::Result<Vec<u8>> {
        let info = self.info(oid)?.ok_or_else(missing)?;
        if info.kind != kind {
            return Err(invalid("unexpected Git object type"));
        }
        let limit = if kind == ObjectType::Blob {
            BLOB_BYTES
        } else {
            1024 * 1024
        };
        if info.size > limit {
            return Err(budget());
        }
        self.bytes_left = self.bytes_left.checked_sub(info.size).ok_or_else(budget)?;
        self.process.send(format!("contents {oid}\0").as_bytes())?;
        let reported = self.header(oid)?.ok_or_else(missing)?;
        if reported != info {
            return Err(invalid(
                "Git object header changed between info and contents",
            ));
        }
        payload(&mut self.process.output, info.size)
    }

    pub fn commit(&mut self, oid: Oid) -> io::Result<CommitLinks> {
        let raw = self.contents(oid, ObjectType::Commit)?;
        parse_commit(self.format, &raw)
    }

    pub fn peel_commit(&mut self, mut oid: Oid) -> io::Result<Option<Oid>> {
        let mut expected = None;
        for depth in 0..=TAG_DEPTH {
            let kind = self.info(oid)?.ok_or_else(missing)?.kind;
            if expected.is_some_and(|expected| expected != kind) {
                return Err(invalid("annotated tag target type mismatch"));
            }
            match kind {
                ObjectType::Commit => return Ok(Some(oid)),
                ObjectType::Tree | ObjectType::Blob => return Ok(None),
                ObjectType::Tag if depth == TAG_DEPTH => return Err(budget()),
                ObjectType::Tag => {
                    let raw = self.contents(oid, ObjectType::Tag)?;
                    let (target, kind) = parse_tag(self.format, &raw)?;
                    oid = target;
                    expected = Some(kind);
                }
            }
        }
        unreachable!("the loop terminates at its tag depth limit")
    }

    pub fn reachable(&mut self, roots: &[Oid], target: Oid) -> io::Result<Reachability> {
        require_format(self.format, target)?;
        if roots.len() > TOTAL_OBJECTS {
            return Ok(Reachability::Indeterminate);
        }
        let mut seen = HashSet::new();
        let mut pending = Vec::new();
        for &root in roots {
            require_format(self.format, root)?;
            if seen.insert(root) {
                pending.push(root);
            }
        }
        while let Some(oid) = pending.pop() {
            let commit = match self.commit(oid) {
                Ok(commit) => commit,
                Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {
                    return Ok(Reachability::Indeterminate);
                }
                Err(error) => return Err(error),
            };
            if target == oid {
                return Ok(Reachability::Reachable);
            }
            for parent in commit.parents {
                if seen.insert(parent) {
                    if seen.len() > TOTAL_OBJECTS {
                        return Ok(Reachability::Indeterminate);
                    }
                    pending.push(parent);
                }
            }
        }
        Ok(Reachability::NotReachable)
    }

    fn header(&mut self, oid: Oid) -> io::Result<Option<ObjectInfo>> {
        let record = frame(&mut self.process.output, 128)?.ok_or_else(missing)?;
        parse_header(oid, &record)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CommitLinks {
    pub tree: Oid,
    pub parents: Vec<Oid>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reachability {
    Reachable,
    NotReachable,
    Indeterminate,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: Vec<u8>,
    pub oid: Oid,
    pub mode: TreeMode,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TreeMode {
    Directory,
    Regular,
    Executable,
    Symlink,
    Gitlink,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn budget() -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, "Git reader budget exceeded")
}

fn missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "Git object or frame missing")
}

fn require_format(format: ObjectFormat, oid: Oid) -> io::Result<()> {
    if oid.format() != format {
        return Err(invalid("Git OID format differs from repository"));
    }
    Ok(())
}

fn frame(reader: &mut impl BufRead, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    if reader.take(limit + 1).read_until(0, &mut bytes)? == 0 {
        return Ok(None);
    }
    if bytes.len() as u64 > limit || bytes.pop() != Some(0) {
        return Err(invalid("invalid Git NUL-delimited frame"));
    }
    Ok(Some(bytes))
}

fn payload(reader: &mut impl Read, size: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    let mut separator = [0];
    reader.read_exact(&mut separator)?;
    if separator != [0] {
        return Err(invalid("invalid Git payload delimiter"));
    }
    Ok(bytes)
}

fn parse_header(oid: Oid, record: &[u8]) -> io::Result<Option<ObjectInfo>> {
    let mut fields = record.split(|&byte| byte == b' ');
    let response_oid = fields
        .next()
        .ok_or_else(|| invalid("missing Git response OID"))?;
    if Oid::parse(oid.format(), response_oid).map_err(invalid)? != oid {
        return Err(invalid("Git returned a different OID"));
    }
    let kind = fields
        .next()
        .ok_or_else(|| invalid("missing Git response type"))?;
    if kind == b"missing" && fields.next().is_none() {
        return Ok(None);
    }
    let kind = ObjectType::parse(kind).map_err(invalid)?;
    let size = fields
        .next()
        .ok_or_else(|| invalid("missing Git object size"))?;
    if size.is_empty()
        || (size.len() > 1 && size.starts_with(b"0"))
        || !size.iter().all(u8::is_ascii_digit)
        || fields.next().is_some()
    {
        return Err(invalid("invalid Git object size"));
    }
    let size = std::str::from_utf8(size)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid("Git object size overflow"))?;
    Ok(Some(ObjectInfo { kind, size }))
}

fn parse_tree(format: ObjectFormat, record: &[u8]) -> io::Result<TreeEntry> {
    let separator = record
        .iter()
        .position(|&byte| byte == b'\t')
        .ok_or_else(|| invalid("missing Git tree path separator"))?;
    let (header, path) = record.split_at(separator);
    let name = &path[1..];
    validate_component(name).map_err(invalid)?;
    let mut fields = header.split(|&byte| byte == b' ');
    let mode = match (fields.next(), fields.next()) {
        (Some(b"040000"), Some(b"tree")) => TreeMode::Directory,
        (Some(b"100644"), Some(b"blob")) => TreeMode::Regular,
        (Some(b"100755"), Some(b"blob")) => TreeMode::Executable,
        (Some(b"120000"), Some(b"blob")) => TreeMode::Symlink,
        (Some(b"160000"), Some(b"commit")) => TreeMode::Gitlink,
        _ => return Err(invalid("invalid Git tree mode/type combination")),
    };
    let oid = Oid::parse(
        format,
        fields
            .next()
            .ok_or_else(|| invalid("missing Git tree OID"))?,
    )
    .map_err(invalid)?;
    if fields.next().is_some() {
        return Err(invalid("unexpected Git tree header field"));
    }
    Ok(TreeEntry {
        name: name.to_vec(),
        oid,
        mode,
    })
}

fn parse_commit(format: ObjectFormat, raw: &[u8]) -> io::Result<CommitLinks> {
    let mut lines = raw.split(|&byte| byte == b'\n');
    let tree = lines
        .next()
        .and_then(|line| line.strip_prefix(b"tree "))
        .ok_or_else(|| invalid("missing Git commit tree"))?;
    let tree = Oid::parse(format, tree).map_err(invalid)?;
    let mut parents = Vec::new();
    let mut in_parents = true;
    for line in lines {
        if line.is_empty() {
            return Ok(CommitLinks { tree, parents });
        }
        if let Some(oid) = line.strip_prefix(b"parent ") {
            if !in_parents || parents.len() >= TOTAL_OBJECTS {
                return Err(invalid("invalid Git commit parent list"));
            }
            parents.push(Oid::parse(format, oid).map_err(invalid)?);
        } else {
            in_parents = false;
        }
    }
    Err(invalid("missing Git commit header terminator"))
}

fn parse_tag(format: ObjectFormat, raw: &[u8]) -> io::Result<(Oid, ObjectType)> {
    let mut lines = raw.split(|&byte| byte == b'\n');
    let oid = lines
        .next()
        .and_then(|line| line.strip_prefix(b"object "))
        .ok_or_else(|| invalid("missing annotated tag target"))?;
    let kind = lines
        .next()
        .and_then(|line| line.strip_prefix(b"type "))
        .ok_or_else(|| invalid("missing annotated tag type"))?;
    if !lines.any(<[u8]>::is_empty) {
        return Err(invalid("missing annotated tag header terminator"));
    }
    Ok((
        Oid::parse(format, oid).map_err(invalid)?,
        ObjectType::parse(kind).map_err(invalid)?,
    ))
}

/// Private binary entry point. The executable, configuration and operations
/// are fixed. Only a typed store coordinate and tree OID can affect argv.
#[doc(hidden)]
pub fn worker(args: &[String]) -> io::Result<()> {
    let [scope, id, format, rest @ ..] = args else {
        return Err(invalid("invalid Git worker coordinates"));
    };
    let location = match scope.as_str() {
        "canonical" => Location::Canonical(id.parse::<RepositoryId>().map_err(invalid)?),
        "incoming" => {
            let job: uuid::Uuid = id
                .parse()
                .map_err(|_| invalid("invalid incoming job UUID"))?;
            if job.to_string() != *id {
                return Err(invalid("noncanonical incoming job UUID"));
            }
            Location::Incoming { job }
        }
        _ => return Err(invalid("invalid Git worker scope")),
    };
    let format = format.parse::<ObjectFormat>().map_err(invalid)?;
    let (force_failure, rest) = if let Some((flag, rest)) = rest.split_last()
        && flag == "--fail-setup"
    {
        (true, rest)
    } else {
        (false, rest)
    };
    let deadline = Instant::now() + WALL_LIMIT;
    let repo = store::open_bare(location, format, deadline)?;
    let mut command = Command::new(sandbox::GIT);
    command
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .arg("--git-dir")
        .arg(&repo.path)
        .arg("-c")
        .arg(format!("safe.directory={}", repo.path.display()))
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"]);
    match rest {
        [mode] if mode == "batch" => {
            command.args(["cat-file", "--batch-command", "-Z"]);
        }
        [mode, tree] if mode == "tree" => {
            let tree = Oid::parse(format, tree.as_bytes()).map_err(invalid)?;
            command.args(["ls-tree", "-z", &tree.to_string(), "--"]);
        }
        _ => return Err(invalid("unsupported Git worker operation")),
    }
    ensure_time(deadline)?;
    sandbox::confine(sandbox::Profile::Git, Some(repo.file), force_failure)
        .map_err(|error| io::Error::other(format!("sandbox_unavailable: {error}")))?;
    Err(command.exec())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn validates_framing_size_and_opaque_payload() {
        let oid = Oid::Sha1([0; 20]);
        assert_eq!(
            parse_header(oid, format!("{oid} blob 3").as_bytes()).unwrap(),
            Some(ObjectInfo {
                kind: ObjectType::Blob,
                size: 3
            })
        );
        for bad in [
            "blob 03",
            "blob +3",
            "blob 9999999999999999999999",
            "blob 3 extra",
            "unknown 3",
        ] {
            assert!(
                parse_header(oid, format!("{oid} {bad}").as_bytes()).is_err(),
                "{bad}"
            );
        }
        assert!(parse_header(oid, format!("{} blob 3", Oid::Sha1([1; 20])).as_bytes()).is_err());
        assert!(frame(&mut Cursor::new(b"end"), 10).is_err());
        assert!(frame(&mut Cursor::new(b"abcdef\0"), 5).is_err());
        assert!(payload(&mut Cursor::new(b"a\0"), 3).is_err());
        assert!(payload(&mut Cursor::new(b"a\0b\n"), 3).is_err());
        assert_eq!(payload(&mut Cursor::new(b"a\0b\0"), 3).unwrap(), b"a\0b");
    }

    #[test]
    fn parses_gitlinks_without_treating_paths_as_utf8() {
        let oid = Oid::Sha256([1; 32]);
        let mut record = format!("160000 commit {oid}\t").into_bytes();
        record.extend_from_slice(b"sub\t\xff");
        let entry = parse_tree(ObjectFormat::Sha256, &record).unwrap();
        assert_eq!(
            entry,
            TreeEntry {
                name: b"sub\t\xff".to_vec(),
                oid,
                mode: TreeMode::Gitlink
            }
        );
        record.push(b'/');
        assert!(parse_tree(ObjectFormat::Sha256, &record).is_err());
    }
}
