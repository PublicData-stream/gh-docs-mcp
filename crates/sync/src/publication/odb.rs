//! Filesystem operations on the canonical store, called under an exclusive fence.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CString, OsString},
    fs::{self, File, FileTimes, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
};

use docs_mcp_core::{
    Result,
    git::{ObjectFormat, RepositoryId},
};
use serde::{Deserialize, Serialize};

use crate::{approval::MIB, candidate, hex};

const STORE_BYTES: u64 = 1024 * MIB;
const FREE_RESERVE: u64 = 128 * MIB;
const PACK_FILE_LIMIT: u64 = 272 * MIB;
const MAX_PACK_ENTRIES: usize = 2048;

pub struct Repository {
    pub id: RepositoryId,
    pub format: ObjectFormat,
    pub odb: PathBuf,
    pub stage: PathBuf,
    pub manifests: PathBuf,
}

impl Repository {
    pub fn open(id: RepositoryId, format: ObjectFormat) -> Result<Self> {
        let root = docs_mcp_git::store::canonical_root()?;
        let private = root.join(".publication");
        directory(&private, 0o700)?;
        let workspace = private.join(id.to_string());
        directory(&workspace, 0o700)?;
        let manifests = root.join("manifests").join(id.to_string());
        directory(&manifests, 0o755)?;
        Ok(Self {
            id,
            format,
            odb: root.join("repositories").join(id.to_string()),
            manifests,
            stage: workspace.join("install"),
        })
    }

    pub fn pack_dir(&self) -> PathBuf {
        self.odb.join("objects/pack")
    }

    pub fn manifest(&self, id: i64) -> PathBuf {
        self.manifests.join(format!("{id}.json"))
    }

    pub fn roots(&self, id: i64) -> PathBuf {
        self.odb.join(format!("refs/gh-docs/retention/{id}"))
    }

    pub fn begin_stage(&self) -> Result<()> {
        fs::DirBuilder::new().mode(0o700).create(&self.stage)?;
        sync_parent(&self.stage)
    }

    pub fn remove_stage(&self) -> Result<()> {
        if !exists(&self.stage)? {
            return Ok(());
        }
        let directory = fs::symlink_metadata(&self.stage)?;
        if !directory.is_dir() || directory.uid() != 10002 || directory.mode() & 0o077 != 0 {
            return Err("ambiguous publication workspace identity".into());
        }
        for name in names(&self.stage, 12)? {
            if !matches!(
                name.to_str(),
                Some(
                    "empty-odb"
                        | "working"
                        | "update-ref"
                        | "manifest.json"
                        | "transfer"
                        | "retired-refs"
                        | "retired-manifest.json"
                        | "repack"
                        | "object-input"
                )
            ) {
                return Err("unrecognized publication workspace content".into());
            }
        }
        crate::private::remove(&self.stage)
    }

    pub fn ensure_odb(&self) -> Result<()> {
        if !exists(&self.odb)? {
            let temporary = self.stage.join("empty-odb");
            if exists(&temporary)? {
                crate::private::remove(&temporary)?;
            }
            init_odb(&temporary, self.format)?;
            for name in [
                "objects/pack",
                "objects",
                "refs/gh-docs/retention",
                "refs/gh-docs",
                "refs",
                "",
            ] {
                File::open(temporary.join(name))?.sync_all()?;
            }
            rename_new(&temporary, &self.odb)?;
        }
        self.validate_layout()
    }

    pub fn validate_layout(&self) -> Result<()> {
        docs_mcp_git::store::validate_canonical(self.id, self.format)?;
        exact_names(&self.odb, &["HEAD", "config", "objects", "refs"])?;
        exact_names(&self.odb.join("objects"), &["pack"])?;
        exact_names(&self.odb.join("refs"), &["gh-docs"])?;
        exact_names(&self.odb.join("refs/gh-docs"), &["retention"])?;
        Ok(())
    }
}

pub fn init_odb(path: &Path, format: ObjectFormat) -> Result<()> {
    fs::create_dir(path)?;
    fs::create_dir_all(path.join("objects/pack"))?;
    fs::create_dir_all(path.join("refs/gh-docs/retention"))?;
    write_file(
        &path.join("config"),
        docs_mcp_git::store::bare_config(format).as_bytes(),
        0o644,
    )?;
    write_file(&path.join("HEAD"), b"ref: refs/heads/main\n", 0o644)?;
    Ok(())
}

fn directory(path: &Path, mode: u32) -> Result<()> {
    match fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => sync_parent(path)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    let parent = fs::symlink_metadata(path.parent().ok_or("invalid canonical directory")?)?;
    if !meta.is_dir()
        || meta.uid() != 10002
        || meta.gid() != 10002
        || meta.mode() & 0o777 != mode
        || meta.dev() != parent.dev()
    {
        return Err("unexpected canonical directory identity".into());
    }
    Ok(())
}

pub fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn names(path: &Path, maximum: usize) -> Result<BTreeSet<OsString>> {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(path)? {
        if names.len() == maximum {
            return Err("canonical directory exceeds its entry budget".into());
        }
        names.insert(entry?.file_name());
    }
    Ok(names)
}

fn exact_names(path: &Path, expected: &[&str]) -> Result<()> {
    if names(path, expected.len())? != expected.iter().map(OsString::from).collect() {
        return Err("unexpected canonical Git repository layout".into());
    }
    Ok(())
}

pub fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().ok_or("invalid canonical path")?)?.sync_all()?;
    Ok(())
}

pub fn rename_new(source: &Path, dest: &Path) -> Result<()> {
    let from = CString::new(source.as_os_str().as_bytes())?;
    let to = CString::new(dest.as_os_str().as_bytes())?;
    // SAFETY: both path strings remain alive for the syscall.
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    sync_parent(dest)?;
    sync_parent(source)
}

pub fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

pub fn copy_file(source: &Path, dest: &Path, limit: u64) -> Result<()> {
    let source = candidate::open_regular(source)?;
    let timestamp = source.metadata()?.modified()?;
    let mut input = source.take(limit + 1);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(dest)?;
    if io::copy(&mut input, &mut output)? > limit {
        return Err("canonical copy exceeds its size budget".into());
    }
    output.set_times(FileTimes::new().set_modified(timestamp))?;
    output.sync_all()?;
    Ok(())
}

fn digest(path: &Path) -> Result<String> {
    Ok(hex(&candidate::pack_digest(path, PACK_FILE_LIMIT)?))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackFile {
    pub name: String,
    pub digest: String,
}

impl PackFile {
    pub fn inspect(path: &Path, format: ObjectFormat) -> Result<Self> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("invalid pack path")?;
        Self::parse_name(name, format)?;
        Ok(Self {
            name: name.into(),
            digest: digest(path)?,
        })
    }

    fn parse_name(name: &str, format: ObjectFormat) -> Result<(&str, &str)> {
        let (hash, suffix) = name
            .strip_prefix("pack-")
            .and_then(|name| name.split_once('.'))
            .ok_or("unexpected canonical pack name")?;
        if hash.len() != format.digest_len() * 2
            || !hash
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            || !matches!(suffix, "idx" | "pack" | "mtimes")
        {
            return Err("unexpected canonical pack component".into());
        }
        Ok((hash, suffix))
    }

    pub fn validate(&self, format: ObjectFormat) -> Result<()> {
        Self::parse_name(&self.name, format)?;
        if self.digest.len() != 64
            || !self
                .digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err("invalid operation pack digest".into());
        }
        Ok(())
    }

    pub fn matches(&self, directory: &Path, format: ObjectFormat) -> Result<bool> {
        self.validate(format)?;
        let path = directory.join(&self.name);
        Ok(exists(&path)? && digest(&path)? == self.digest)
    }

    pub fn install(&self, repo: &Repository, source: &Path, replace: Option<&Self>) -> Result<()> {
        let packs = repo.pack_dir();
        if self.matches(&packs, repo.format)? {
            return Ok(());
        }
        if !self.matches(source, repo.format)? {
            return Err("staged pack component failed its digest proof".into());
        }
        let destination = packs.join(&self.name);
        let replacing = exists(&destination)?;
        if replacing
            && (!self.name.ends_with(".mtimes") || replace.is_none_or(|old| old.name != self.name))
        {
            return Err("canonical pack path conflicts with verified bytes".into());
        }
        if replacing
            && !replace
                .expect("checked replacement")
                .matches(&packs, repo.format)?
        {
            return Err("canonical pack metadata changed during maintenance".into());
        }
        let install = repo.stage.join("transfer");
        if exists(&install)? {
            // A prior attempt can only leave this fixed, sync-private copy.
            let _ = candidate::open_regular(&install)?;
            fs::remove_file(&install)?;
        }
        copy_file(&source.join(&self.name), &install, PACK_FILE_LIMIT)?;
        if digest(&install)? != self.digest {
            return Err("canonical transfer checksum mismatch".into());
        }
        if replacing {
            fs::rename(&install, &destination)?;
            sync_parent(&destination)?;
            sync_parent(&install)?;
        } else {
            rename_new(&install, &destination)?;
        }
        Ok(())
    }
}

pub fn verify_pack_pairs(directory: &Path, format: ObjectFormat) -> Result<Vec<OsString>> {
    let entries = names(directory, MAX_PACK_ENTRIES)?;
    let mut components = BTreeMap::new();
    for entry in &entries {
        let (hash, suffix) =
            PackFile::parse_name(entry.to_str().ok_or("invalid pack filename")?, format)?;
        let set = components.entry(hash).or_insert_with(BTreeSet::new);
        set.insert(suffix);
        let _ = candidate::open_regular(&directory.join(entry))?;
    }
    if components
        .values()
        .any(|set| !set.contains("pack") || !set.contains("idx"))
    {
        return Err("canonical pack is missing its companion index or pack".into());
    }
    Ok(entries.into_iter().collect())
}

pub fn admission(directory: &Path, bytes_to_add: u64) -> Result<()> {
    let mut bytes = 0_u64;
    if exists(directory)? {
        for name in names(directory, MAX_PACK_ENTRIES)? {
            let file = candidate::open_regular(&directory.join(name))?;
            bytes = bytes.saturating_add(file.metadata()?.len());
        }
    }
    if bytes.saturating_add(bytes_to_add) > STORE_BYTES {
        return Err("canonical storage admission limit reached".into());
    }
    free_space(directory, bytes_to_add)
}

pub fn maintenance_admission(directory: &Path, bytes: u64) -> Result<()> {
    if bytes > STORE_BYTES {
        return Err("canonical storage exceeds its maintenance limit".into());
    }
    free_space(directory, bytes.saturating_mul(3))
}

fn free_space(directory: &Path, bytes_to_add: u64) -> Result<()> {
    let file = File::open(
        directory
            .parent()
            .filter(|path| path.exists())
            .unwrap_or(Path::new("/git-store")),
    )?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs writes to the provided output on success.
    if unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let filesystem = unsafe { filesystem.assume_init() };
    let free = filesystem.f_bavail.saturating_mul(filesystem.f_bsize as _);
    if free < FREE_RESERVE.saturating_add(bytes_to_add) {
        return Err("canonical storage admission limit reached".into());
    }
    Ok(())
}
