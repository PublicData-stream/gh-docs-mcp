use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use docs_mcp_core::Result;
use uuid::Uuid;

use crate::approval::MIB;

pub const INCOMING: &str = "/git-sync-private/incoming";
const QUARANTINE: &str = "/git-sync-private/quarantine";
const LOCKS: &str = "/git-sync-private/coordination";
const SCRATCH: &str = "/git-sync-scratch";
const GIB: u64 = 1024 * MIB;
const QUARANTINE_LIMIT: u64 = GIB;
pub const JOB_RESERVATION: u64 = 384 * MIB;

pub struct Slot {
    pub root: PathBuf,
    pub capacity: u64,
    pub quota: u64,
    _lock: File,
}

impl Slot {
    pub fn acquire() -> Result<Option<Self>> {
        let concurrency = concurrency()? as u64;
        let quota = configured(
            "DOCS_MCP_SYNC_QUOTA_BYTES",
            8 * GIB,
            JOB_RESERVATION,
            8 * GIB,
        )?;
        ensure_locks()?;
        for slot in 1..=concurrency {
            if let Some(lock) = lock(&format!("slot-{slot}"), true)? {
                let root = Path::new(SCRATCH).join(slot.to_string());
                let filesystem = filesystem(&root)?;
                if filesystem.f_type as i64 != 0x0102_1994
                    || filesystem.f_blocks.saturating_mul(filesystem.f_bsize as _) > GIB
                {
                    return Err("sync scratch slot must be a bounded tmpfs".into());
                }
                for entry in fs::read_dir(&root)? {
                    let entry = entry?;
                    if let Some(job) = entry
                        .file_name()
                        .to_str()
                        .and_then(|name| Uuid::parse_str(name).ok())
                        && job.get_version_num() == 7
                    {
                        remove(&entry.path())?;
                    }
                }
                return Ok(Some(Self {
                    root,
                    capacity: concurrency,
                    quota,
                    _lock: lock,
                }));
            }
        }
        Ok(None)
    }

    pub fn check_space(&self, reserve: u64) -> Result<()> {
        let used =
            volume_usage(Path::new(INCOMING))?.saturating_add(volume_usage(Path::new(QUARANTINE))?);
        let file_system = filesystem(Path::new(INCOMING))?;
        let free = file_system
            .f_bavail
            .saturating_mul(file_system.f_bsize as _);
        if used.saturating_add(reserve) > self.quota || free < reserve || file_system.f_ffree < 8192
        {
            return Err("sync-private storage quota reached".into());
        }
        Ok(())
    }
}

pub fn concurrency() -> Result<usize> {
    Ok(configured("DOCS_MCP_SYNC_CONCURRENCY", 2, 1, 4)? as usize)
}

fn configured(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = match std::env::var(name) {
        Ok(value) => value.parse()?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => return Err(error.into()),
    };
    if !(min..=max).contains(&value) {
        return Err("invalid sync resource configuration".into());
    }
    Ok(value)
}

fn filesystem(path: &Path) -> Result<libc::statfs> {
    let fd = File::open(path)?;
    let mut value = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(fd.as_raw_fd(), value.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { value.assume_init() })
}

fn lock(name: &str, nonblocking: bool) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(Path::new(LOCKS).join(name))?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.mode() & 0o077 != 0
        || meta.uid() != unsafe { libc::geteuid() }
    {
        return Err("invalid stable sync lock".into());
    }
    let operation = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(Some(file));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error.into());
    }
}

pub fn ensure_locks() -> Result<()> {
    match fs::create_dir(LOCKS) {
        Ok(()) => File::open("/git-sync-private")?.sync_all()?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    if !fs::symlink_metadata(LOCKS)?.is_dir() {
        return Err("invalid sync lock directory".into());
    }
    Ok(())
}

pub fn publication_lock() -> Result<File> {
    lock("publication", true)?.ok_or_else(|| "canonical storage admission busy".into())
}

pub fn quota_lock() -> Result<File> {
    lock("quota", false)?.ok_or_else(|| "unable to hold staging quota".into())
}

pub fn incoming(job: Uuid) -> PathBuf {
    Path::new(INCOMING).join(job.to_string())
}

pub fn remove(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    File::open(path.parent().ok_or("invalid private cleanup path")?)?.sync_all()?;
    Ok(())
}

fn volume_usage(path: &Path) -> Result<u64> {
    let mut pending = vec![(path.to_path_buf(), 0)];
    let mut used = 0_u64;
    let mut entries = 0_u32;
    while let Some((entry, depth)) = pending.pop() {
        entries = entries.checked_add(1).ok_or("staging inventory overflow")?;
        if entries > 100_000 || depth > 24 {
            return Err("staging inventory exceeds its limits".into());
        }
        let meta = fs::symlink_metadata(&entry)?;
        used = used.saturating_add(meta.blocks().saturating_mul(512).max(meta.len()));
        if meta.is_dir() {
            for child in fs::read_dir(entry)? {
                if pending.len() >= 100_000 {
                    return Err("staging inventory exceeds its limits".into());
                }
                pending.push((child?.path(), depth + 1));
            }
        } else if !meta.is_file() || meta.nlink() != 1 {
            return Err("unexpected staging inventory entry".into());
        }
    }
    Ok(used)
}

pub fn quarantine(source: &Path, job: Uuid) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    let root = Path::new(QUARANTINE);
    let size = volume_usage(source)?;
    if size > JOB_RESERVATION {
        return remove(source);
    }
    let mut previous = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .and_then(|name| Uuid::parse_str(name).ok())
            .is_some_and(|id| id.get_version_num() == 7)
        {
            previous.push(entry.path());
        }
    }
    previous.sort_unstable();
    let mut bytes = volume_usage(root)?;
    for entry in previous {
        if bytes.saturating_add(size) <= QUARANTINE_LIMIT {
            break;
        }
        bytes = bytes.saturating_sub(volume_usage(&entry)?);
        remove(&entry)?;
    }
    if bytes.saturating_add(size) > QUARANTINE_LIMIT {
        return remove(source);
    }
    fs::rename(source, root.join(job.to_string()))?;
    File::open(root)?.sync_all()?;
    File::open(INCOMING)?.sync_all()?;
    Ok(())
}

pub struct Scratch(pub PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if remove(&self.0).is_err() {
            eprintln!("sync scratch cleanup pending");
        }
    }
}
