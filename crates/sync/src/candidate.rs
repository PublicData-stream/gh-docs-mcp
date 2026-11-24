use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use gh_docs_core::{
    Result,
    git::{ObjectFormat, ObjectType, Oid, RefName},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    approval::{Approval, MAX_REFS, MIB},
    upstream::{TransferError, Upstream},
    worker,
};

const MAX_MAP_OUTPUT: usize = 64 * 1024 * 1024;
const MAX_METADATA_OUTPUT: usize = 96 * 1024 * 1024;
const MAX_INFLATED: u64 = 2 * 1024 * MIB;
const MAX_OBJECT: u64 = 128 * MIB;
const TAG_DEPTH: usize = 16;
const TREE_DEPTH: usize = 256;

#[derive(PartialEq, Eq)]
struct Snapshot {
    default: RefName,
    refs: BTreeMap<RefName, Oid>,
}

pub struct Verified {
    pub directory: PathBuf,
    pub default: RefName,
    pub snapshot: Value,
    pub pack: [u8; 32],
    pub objects: usize,
    pub inflated: u64,
    pub commits: BTreeSet<Oid>,
}

pub struct Baseline {
    pub directory: PathBuf,
    pub generation: i64,
    pub pack: Vec<u8>,
    pub observed: Vec<u8>,
}

pub fn init_odb(path: &Path, format: ObjectFormat) -> Result<()> {
    fs::create_dir(path)?;
    fs::create_dir_all(path.join("objects/pack"))?;
    fs::create_dir_all(path.join("refs/gh-docs/fetch"))?;
    fs::write(path.join("config"), gh_docs_git::store::bare_config(format))?;
    fs::write(path.join("HEAD"), b"ref: refs/heads/main\n")?;
    Ok(())
}

fn oid(format: ObjectFormat, bytes: &[u8]) -> Result<Oid> {
    Ok(Oid::parse(format, bytes)?)
}

fn parse_observation(bytes: &[u8], approval: &Approval, format: ObjectFormat) -> Result<Snapshot> {
    let mut default = None;
    let mut head_oid = None;
    let mut refs = BTreeMap::new();
    for line in lines(bytes)? {
        let (target, name) = split_tab(line)?;
        if name == b"HEAD" {
            if let Some(symbolic) = target.strip_prefix(b"ref: ") {
                if default.replace(RefName::new(symbolic.to_vec())?).is_some() {
                    return Err("duplicate symbolic HEAD".into());
                }
            } else if head_oid.replace(oid(format, target)?).is_some() {
                return Err("duplicate HEAD object".into());
            }
        } else if !name.ends_with(b"^{}") {
            let name = RefName::new(name.to_vec())?;
            if approval.allows(&name) && refs.insert(name, oid(format, target)?).is_some() {
                return Err("duplicate approved ref".into());
            }
        }
    }
    let default = default.ok_or("default branch is missing or detached")?;
    if !default.bytes().starts_with(b"refs/heads/")
        || refs.get(&default).copied() != head_oid
        || head_oid.is_none()
        || refs.len() > MAX_REFS
    {
        return Err("default branch or approved ref scope is invalid".into());
    }
    for required in &approval.required_refs {
        if !refs.contains_key(&RefName::new(required.bytes().to_vec())?) {
            return Err("required approved ref is missing".into());
        }
    }
    Ok(Snapshot { default, refs })
}

fn split_tab(line: &[u8]) -> Result<(&[u8], &[u8])> {
    let separator = line
        .iter()
        .position(|&b| b == b'\t')
        .ok_or("missing Git ref separator")?;
    Ok((&line[..separator], &line[separator + 1..]))
}

fn lines(bytes: &[u8]) -> Result<impl Iterator<Item = &[u8]>> {
    let body = if bytes.is_empty() {
        bytes
    } else {
        bytes
            .strip_suffix(b"\n")
            .ok_or("missing Git output newline")?
    };
    Ok(body
        .split(|&b| b == b'\n')
        .filter(move |_| !bytes.is_empty()))
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

pub async fn acquire(
    upstream: &mut Upstream<'_>,
    attempt: &Path,
    approval: &Approval,
    format: ObjectFormat,
    seed: bool,
) -> std::result::Result<Verified, TransferError> {
    let invalid = || TransferError::permanent("candidate_invalid");
    let working = attempt.join("working");
    if !seed {
        init_odb(&working, format).map_err(|_| invalid())?;
    }
    let ls = args(&[
        "ls-remote",
        "--symref",
        "ghdocs::approved",
        "HEAD",
        "refs/heads/*",
        "refs/tags/*",
    ]);
    let empty = BTreeSet::new();
    let before = upstream
        .git(attempt, &working, &ls, 4 * MIB as usize, &empty)
        .await?;
    let before = parse_observation(&before, approval, format)
        .map_err(|_| TransferError::permanent("ref_policy"))?;
    let mut fetch = args(&[
        "fetch",
        "--atomic",
        "--no-tags",
        "--no-recurse-submodules",
        "--no-auto-maintenance",
        "--no-write-fetch-head",
        "ghdocs::approved",
    ]);
    for (index, name) in before.refs.keys().enumerate() {
        let mut refspec = OsString::from("+");
        refspec.push(OsStr::from_bytes(name.bytes()));
        refspec.push(format!(":refs/gh-docs/fetch/{index}"));
        fetch.push(refspec);
    }
    let wants = before.refs.values().map(ToString::to_string).collect();
    upstream.git(attempt, &working, &fetch, 0, &wants).await?;
    let after = upstream
        .git(attempt, &working, &ls, 4 * MIB as usize, &empty)
        .await?;
    if parse_observation(&after, approval, format).map_err(|_| invalid())? != before {
        return Err(TransferError::transient(
            "ref_drift",
            Duration::from_secs(2),
        ));
    }
    for (index, expected) in before.refs.values().enumerate() {
        if read_ref(&working.join(format!("refs/gh-docs/fetch/{index}")), format)
            .map_err(|_| invalid())?
            != *expected
        {
            return Err(TransferError::transient(
                "ref_drift",
                Duration::from_secs(2),
            ));
        }
    }
    let old = working.join("refs/gh-docs/seed");
    if old.exists() {
        fs::remove_dir_all(old).map_err(|_| invalid())?;
    }
    inventory(attempt, &working, approval, format, false)
        .await
        .map_err(|_| invalid())?;
    let directory = attempt.join("verified");
    init_odb(&directory, format).map_err(|_| invalid())?;
    let mut command = args(&[
        "pack-objects",
        "--all",
        "--revs",
        "--quiet",
        "--threads=1",
        "--depth=50",
    ]);
    command.push(directory.join("objects/pack/pack").into_os_string());
    worker::local(attempt, &working, &command, 128)
        .await
        .map_err(|_| invalid())?;
    for (index, value) in before.refs.values().enumerate() {
        fs::write(
            directory.join(format!("refs/gh-docs/fetch/{index}")),
            format!("{value}\n"),
        )
        .map_err(|_| invalid())?;
    }
    verify(attempt, directory, approval, format, before)
        .await
        .map_err(|_| invalid())
}

fn read_ref(path: &Path, format: ObjectFormat) -> Result<Oid> {
    let mut result = Vec::with_capacity(65);
    open_regular(path)?.take(66).read_to_end(&mut result)?;
    oid(
        format,
        result
            .strip_suffix(b"\n")
            .ok_or("invalid private ref contents")?,
    )
}

pub async fn seed(
    attempt: &Path,
    baseline: &Baseline,
    approval: &Approval,
    format: ObjectFormat,
) -> Result<()> {
    let manifest = read_manifest(&baseline.directory.join("manifest.json"))?;
    if gh_docs_core::git_manifest::observation_digest(&manifest) != baseline.observed.as_slice() {
        return Err("baseline manifest digest mismatch".into());
    }
    let decoded: Value = serde_json::from_slice(&manifest)?;
    let list = decoded[2][5]
        .as_array()
        .ok_or("invalid baseline ref snapshot")?;
    let default =
        RefName::new(STANDARD.decode(decoded[2][4].as_str().ok_or("invalid baseline default")?)?)?;
    let mut refs = BTreeMap::new();
    for item in list {
        let name = RefName::new(STANDARD.decode(item[0].as_str().ok_or("invalid baseline ref")?)?)?;
        let target = oid(
            format,
            item[2]
                .as_str()
                .ok_or("invalid baseline target")?
                .as_bytes(),
        )?;
        if !approval.allows(&name) || refs.insert(name, target).is_some() || refs.len() > MAX_REFS {
            return Err("invalid baseline approved ref map".into());
        }
    }
    let working = attempt.join("working");
    init_odb(&working, format)?;
    let packs = pack_files(
        &baseline.directory.join("odb"),
        format,
        approval.limits.pack_bytes,
    )?;
    let [pack] = packs.as_slice() else {
        return Err("invalid baseline pack inventory".into());
    };
    let copy = working
        .join("objects/pack")
        .join(pack.file_name().ok_or("invalid baseline filename")?);
    copy_regular(pack, &copy, approval.limits.pack_bytes, false)?;
    copy_regular(
        &pack.with_extension("idx"),
        &copy.with_extension("idx"),
        64 * MIB,
        false,
    )?;
    if pack_digest(&copy, approval.limits.pack_bytes)? != baseline.pack.as_slice() {
        return Err("baseline pack digest mismatch".into());
    }
    for (index, target) in refs.values().enumerate() {
        fs::write(
            working.join(format!("refs/gh-docs/fetch/{index}")),
            format!("{target}\n"),
        )?;
    }
    let verified = verify(
        attempt,
        working.clone(),
        approval,
        format,
        Snapshot { default, refs },
    )
    .await?;
    if verified.snapshot != decoded[2][5] {
        return Err("baseline manifest ref provenance mismatch".into());
    }
    // Keep the original roots for Git's negotiation, in a separate namespace.
    fs::rename(
        working.join("refs/gh-docs/fetch"),
        working.join("refs/gh-docs/seed"),
    )?;
    fs::create_dir(working.join("refs/gh-docs/fetch"))?;
    Ok(())
}

pub fn baseline_integrity_error(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    if let Some(io) = error.downcast_ref::<std::io::Error>() {
        return matches!(
            io.kind(),
            std::io::ErrorKind::InvalidData
                | std::io::ErrorKind::NotFound
                | std::io::ErrorKind::UnexpectedEof
        );
    }
    if let Some(git) = error.downcast_ref::<worker::CommandFailure>() {
        return git.possibly_invalid_input();
    }
    // Structural, digest, JSON and native Git identity errors are definitive.
    // A subprocess deadline or supervision failure does not corrupt bytes.
    !error.is::<tokio::time::error::Elapsed>() && !error.is::<tokio::task::JoinError>()
}

async fn verify(
    scratch: &Path,
    directory: PathBuf,
    approval: &Approval,
    format: ObjectFormat,
    observation: Snapshot,
) -> Result<Verified> {
    let approved_refs = directory.join("refs/gh-docs/fetch");
    for (index, expected) in observation.refs.values().enumerate() {
        if read_ref(&approved_refs.join(index.to_string()), format)? != *expected {
            return Err("verified ref content differs from observation".into());
        }
    }
    if fs::read_dir(directory.join("refs"))?.count() != 1
        || fs::read_dir(directory.join("refs/gh-docs"))?.count() != 1
        || fs::read_dir(&approved_refs)?.count() != observation.refs.len()
    {
        return Err("unexpected private reference roots".into());
    }
    let map = inventory(scratch, &directory, approval, format, true).await?;
    let reachable = worker::local(
        scratch,
        &directory,
        &args(&["rev-list", "--objects", "--no-object-names", "--all"]),
        MAX_MAP_OUTPUT,
    )
    .await?;
    let mut closure = BTreeSet::new();
    for line in lines(&reachable)? {
        closure.insert(oid(format, line)?);
    }
    if !map.keys().eq(closure.iter()) {
        return Err("candidate has missing or unreachable objects".into());
    }
    let (trees, tags) = metadata(scratch, &directory, format, &map).await?;
    verify_tree_depth(&trees)?;
    let mut entries = Vec::new();
    for (reference, target) in observation.refs {
        let (kind, _) = *map.get(&target).ok_or("missing ref target")?;
        let mut current = target;
        for _ in 0..TAG_DEPTH {
            let Some(&(next, expected_kind)) = tags.get(&current) else {
                break;
            };
            if map.get(&next).map(|item| item.0) != Some(expected_kind) {
                return Err("annotated tag target type mismatch".into());
            }
            current = next;
        }
        if map.get(&current).map(|item| item.0) != Some(ObjectType::Commit)
            || (reference.bytes().starts_with(b"refs/heads/") && kind != ObjectType::Commit)
        {
            return Err("unsupported noncommit ref or excessive tag depth".into());
        }
        entries.push(json!([
            STANDARD.encode(reference.bytes()),
            kind.name(),
            target.to_string(),
            current.to_string()
        ]));
    }
    let packs = pack_files(&directory, format, approval.limits.pack_bytes)?;
    let [pack] = packs.as_slice() else {
        return Err("candidate must contain exactly one pack".into());
    };
    Ok(Verified {
        pack: pack_digest(pack, approval.limits.pack_bytes)?,
        directory,
        default: observation.default,
        snapshot: Value::Array(entries),
        objects: map.len(),
        inflated: map.values().map(|entry| entry.1).sum(),
        commits: map
            .iter()
            .filter_map(|(&oid, &(kind, _))| (kind == ObjectType::Commit).then_some(oid))
            .collect(),
    })
}

fn pack_files(directory: &Path, format: ObjectFormat, limit: u64) -> Result<Vec<PathBuf>> {
    let mut packs = Vec::new();
    let mut indexes = BTreeSet::new();
    let mut size = 0_u64;
    for entry in fs::read_dir(directory.join("objects/pack"))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("invalid pack filename")?;
        let (hash, extension) = name
            .strip_prefix("pack-")
            .and_then(|s| s.rsplit_once('.'))
            .ok_or("unexpected pack entry")?;
        oid(format, hash.as_bytes())?;
        let file = open_regular(&entry.path())?;
        let length = file.metadata()?.len();
        match extension {
            "pack" => {
                size = size.checked_add(length).ok_or("pack size overflow")?;
                if size > limit || packs.len() >= 8 {
                    return Err("pack storage budget exceeded".into());
                }
                packs.push(entry.path());
            }
            "idx" if length <= 64 * MIB => {
                indexes.insert(entry.path());
            }
            "rev" if length <= 8 * MIB => (),
            _ => return Err("unexpected native pack artifact".into()),
        }
    }
    if packs
        .iter()
        .map(|pack| pack.with_extension("idx"))
        .collect::<BTreeSet<_>>()
        != indexes
    {
        return Err("pack index inventory mismatch".into());
    }
    Ok(packs)
}

async fn inventory(
    scratch: &Path,
    odb: &Path,
    approval: &Approval,
    format: ObjectFormat,
    final_pack: bool,
) -> Result<BTreeMap<Oid, (ObjectType, u64)>> {
    let permitted = if final_pack {
        approval.limits.pack_bytes
    } else {
        2 * approval.limits.pack_bytes
    };
    let packs = pack_files(odb, format, permitted)?;
    let mut count = 0_usize;
    for pack in packs {
        let mut header = [0; 12];
        open_regular(&pack)?.read_exact(&mut header)?;
        if &header[..4] != b"PACK" || !matches!(header[7], 2 | 3) || header[4..7] != [0, 0, 0] {
            return Err("invalid pack header".into());
        }
        count += u32::from_be_bytes(header[8..12].try_into()?) as usize;
        if count > approval.limits.objects * if final_pack { 1 } else { 2 } {
            return Err("pack object count exceeded".into());
        }
        let mut command = args(&["verify-pack", "-v"]);
        command.push(pack.with_extension("idx").into_os_string());
        let report = worker::local(scratch, odb, &command, 128 * MIB as usize).await?;
        for row in lines(&report)? {
            let fields: Vec<_> = row
                .split(|&b| b == b' ')
                .filter(|piece| !piece.is_empty())
                .collect();
            if fields.len() == 7 && std::str::from_utf8(fields[5])?.parse::<usize>()? > 50 {
                return Err("pack delta depth exceeded".into());
            }
        }
    }
    let output = worker::local(
        scratch,
        odb,
        &args(&[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ]),
        MAX_MAP_OUTPUT,
    )
    .await?;
    let mut map = BTreeMap::new();
    let mut inflated = 0_u64;
    for row in lines(&output)? {
        let (key, kind, size) = parse_header(row, format)?;
        inflated = inflated.saturating_add(size);
        if size > MAX_OBJECT
            || inflated > MAX_INFLATED
            || map.len() >= approval.limits.objects
            || map.insert(key, (kind, size)).is_some()
        {
            return Err("inflation or object inventory budget exceeded".into());
        }
    }
    worker::local(
        scratch,
        odb,
        &args(&["fsck", "--full", "--strict", "--no-dangling"]),
        MAX_MAP_OUTPUT,
    )
    .await?;
    Ok(map)
}

fn parse_header(header: &[u8], format: ObjectFormat) -> Result<(Oid, ObjectType, u64)> {
    let mut parts = header.split(|&b| b == b' ');
    let key = oid(format, parts.next().ok_or("missing object ID")?)?;
    let kind = ObjectType::parse(parts.next().ok_or("missing object type")?)?;
    let size: u64 = std::str::from_utf8(parts.next().ok_or("missing object size")?)?.parse()?;
    if parts.next().is_some() {
        return Err("invalid Git object header".into());
    }
    Ok((key, kind, size))
}

type Trees = BTreeMap<Oid, Vec<Oid>>;
type Tags = BTreeMap<Oid, (Oid, ObjectType)>;

async fn metadata(
    scratch: &Path,
    directory: &Path,
    format: ObjectFormat,
    map: &BTreeMap<Oid, (ObjectType, u64)>,
) -> Result<(Trees, Tags)> {
    let path = scratch.join("object-input");
    let mut input = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    for (oid, (kind, _)) in map {
        if matches!(kind, ObjectType::Tree | ObjectType::Tag) {
            writeln!(input, "{oid}")?;
        }
    }
    drop(input);
    let file = File::open(&path)?;
    fs::remove_file(path)?;
    let result = worker::local_with_input(
        scratch,
        directory,
        &args(&["cat-file", "--batch"]),
        MAX_METADATA_OUTPUT,
        Stdio::from(file),
    )
    .await?;
    let mut reader = BufReader::new(result.as_slice());
    let mut header = Vec::new();
    let mut trees = BTreeMap::new();
    let mut tags = BTreeMap::new();
    while reader.read_until(b'\n', &mut header)? != 0 {
        let (key, kind, size) = parse_header(
            header.strip_suffix(b"\n").ok_or("missing header newline")?,
            format,
        )?;
        if map.get(&key) != Some(&(kind, size)) || size > MAX_METADATA_OUTPUT as u64 {
            return Err("unexpected metadata object".into());
        }
        let mut body = vec![0; size as usize + 1];
        reader.read_exact(&mut body)?;
        if body.pop() != Some(b'\n') {
            return Err("unterminated Git body".into());
        }
        match kind {
            ObjectType::Tree if trees.insert(key, tree_children(&body, format)?).is_none() => (),
            ObjectType::Tag if tags.insert(key, tag_target(&body, format)?).is_none() => (),
            _ => return Err("unexpected metadata object type".into()),
        }
        header.clear();
    }
    if trees.len() + tags.len()
        != map
            .values()
            .filter(|entry| matches!(entry.0, ObjectType::Tree | ObjectType::Tag))
            .count()
    {
        return Err("incomplete metadata traversal".into());
    }
    Ok((trees, tags))
}

fn tree_children(mut body: &[u8], format: ObjectFormat) -> Result<Vec<Oid>> {
    let mut children = Vec::new();
    while !body.is_empty() {
        let split = body
            .iter()
            .position(|&b| b == 0)
            .ok_or("invalid tree entry")?;
        let (entry, rest) = body.split_at(split + 1);
        let (raw, remaining) = rest
            .split_at_checked(format.digest_len())
            .ok_or("truncated tree object ID")?;
        if entry.starts_with(b"40000 ") {
            children.push(Oid::from_bytes(format, raw)?);
        }
        body = remaining;
    }
    Ok(children)
}

fn tag_target(body: &[u8], format: ObjectFormat) -> Result<(Oid, ObjectType)> {
    let mut lines = body.split(|&b| b == b'\n');
    let key = lines
        .next()
        .and_then(|line| line.strip_prefix(b"object "))
        .ok_or("invalid tag target")?;
    let kind = lines
        .next()
        .and_then(|line| line.strip_prefix(b"type "))
        .ok_or("invalid tag type")?;
    Ok((oid(format, key)?, ObjectType::parse(kind)?))
}

fn verify_tree_depth(trees: &Trees) -> Result<()> {
    fn visit(
        key: Oid,
        trees: &Trees,
        depths: &mut BTreeMap<Oid, usize>,
        stack: usize,
    ) -> Result<usize> {
        if stack > TREE_DEPTH {
            return Err("tree depth exceeded".into());
        }
        if let Some(&depth) = depths.get(&key) {
            if stack + depth > TREE_DEPTH + 1 {
                return Err("tree depth exceeded".into());
            }
            return Ok(depth);
        }
        let mut depth = 1;
        for &child in trees.get(&key).ok_or("missing child tree")? {
            depth = depth.max(1 + visit(child, trees, depths, stack + 1)?);
        }
        depths.insert(key, depth);
        Ok(depth)
    }
    let mut depths = BTreeMap::new();
    for &key in trees.keys() {
        visit(key, trees, &mut depths, 1)?;
    }
    Ok(())
}

pub fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 || meta.uid() != unsafe { libc::geteuid() } {
        return Err("unexpected private artifact inode".into());
    }
    Ok(file)
}

pub fn copy_regular(from: &Path, to: &Path, limit: u64, flush: bool) -> Result<()> {
    let mut input = open_regular(from)?.take(limit + 1);
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(to)?;
    if std::io::copy(&mut input, &mut output)? > limit {
        return Err("private artifact exceeds its limit".into());
    }
    if flush {
        output.sync_all()?;
    }
    Ok(())
}

fn pack_digest(path: &Path, limit: u64) -> Result<[u8; 32]> {
    let mut digest = Sha256::new();
    let mut input = open_regular(path)?.take(limit + 1);
    let mut count = 0;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        if count > limit {
            return Err("pack exceeds its limit".into());
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

fn read_manifest(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_regular(path)?
        .take(16 * MIB + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > 16 * MIB {
        return Err("manifest exceeds its limit".into());
    }
    Ok(bytes)
}

pub fn install(
    verified: &Verified,
    target: &Path,
    manifest: &[u8],
    approval: &Approval,
    format: ObjectFormat,
) -> Result<()> {
    fs::create_dir(target)?;
    let odb = target.join("odb");
    init_odb(&odb, format)?;
    let packs = pack_files(&verified.directory, format, approval.limits.pack_bytes)?;
    let [pack] = packs.as_slice() else {
        return Err("verified pack inventory changed".into());
    };
    let installed = odb
        .join("objects/pack")
        .join(pack.file_name().ok_or("invalid pack filename")?);
    copy_regular(pack, &installed, approval.limits.pack_bytes, true)?;
    copy_regular(
        &pack.with_extension("idx"),
        &installed.with_extension("idx"),
        64 * MIB,
        true,
    )?;
    if pack_digest(&installed, approval.limits.pack_bytes)? != verified.pack {
        return Err("installed pack digest mismatch".into());
    }
    let entries = verified
        .snapshot
        .as_array()
        .ok_or("invalid verified snapshot")?;
    for (index, entry) in entries.iter().enumerate() {
        let key = entry[2].as_str().ok_or("invalid verified ref object")?;
        write_synced(
            &odb.join(format!("refs/gh-docs/fetch/{index}")),
            format!("{key}\n").as_bytes(),
        )?;
    }
    write_synced(&target.join("manifest.json"), manifest)?;
    for relative in [
        "config",
        "HEAD",
        "objects/pack",
        "objects",
        "refs/gh-docs/fetch",
        "refs/gh-docs",
        "refs",
        "",
    ] {
        File::open(odb.join(relative))?.sync_all()?;
    }
    File::open(target)?.sync_all()?;
    File::open(target.parent().ok_or("invalid candidate parent")?)?.sync_all()?;
    Ok(())
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}
