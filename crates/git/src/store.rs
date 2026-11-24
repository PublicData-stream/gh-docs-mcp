//! Service-owned bare repositories and stable filesystem fences.

use std::{
    ffi::{CString, OsStr},
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use gh_docs_core::git::{ObjectFormat, RepositoryId};

const SYNC_UID: u32 = 10002;
const SYNC_GID: u32 = 10002;
const MAX_ENTRIES: usize = 50_000;
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy)]
enum StoreRoot {
    Canonical,
    Incoming,
}

impl StoreRoot {
    fn path(self) -> &'static str {
        match self {
            Self::Canonical => "/git-store",
            Self::Incoming => "/git-sync-private",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Location {
    Canonical(RepositoryId),
    Incoming { job: uuid::Uuid },
}

impl Location {
    fn root(self) -> StoreRoot {
        match self {
            Self::Canonical(_) => StoreRoot::Canonical,
            Self::Incoming { .. } => StoreRoot::Incoming,
        }
    }

    fn directory(self) -> io::Result<String> {
        match self {
            Self::Canonical(repo) => Ok(format!("repositories/{repo}")),
            Self::Incoming { job }
                if job.get_version_num() == 7 && job.get_variant() == uuid::Variant::RFC4122 =>
            {
                Ok(format!("incoming/{job}"))
            }
            Self::Incoming { .. } => Err(invalid("invalid incoming job UUID")),
        }
    }
}

pub(crate) struct BareDirectory {
    pub(crate) file: File,
    pub(crate) path: PathBuf,
}

/// Called under the repository filesystem fence for canonical data and under
/// exclusive, process-owned fixture/job coordination for incoming data.
pub(crate) fn open_bare(
    location: Location,
    format: ObjectFormat,
    deadline: Instant,
) -> io::Result<BareDirectory> {
    let root = open_root(location.root())?;
    let relative = location.directory()?;
    let (parent, entry) = relative
        .split_once('/')
        .ok_or_else(|| invalid("invalid store location"))?;
    let parent = directory(&root, OsStr::new(parent))?;
    let dir = directory(&parent, OsStr::new(entry))?;
    let config = regular(&dir, OsStr::new("config"))?;
    let mut actual = Vec::new();
    config.take(1024).read_to_end(&mut actual)?;
    if actual != bare_config(format).as_bytes() {
        return Err(invalid("unexpected service-owned bare Git configuration"));
    }
    let head = regular(&dir, OsStr::new("HEAD"))?;
    let mut actual = Vec::new();
    head.take(128).read_to_end(&mut actual)?;
    if actual != b"ref: refs/heads/main\n" {
        return Err(invalid("unexpected service-owned bare Git HEAD"));
    }
    let _ = directory(&dir, OsStr::new("objects"))?;
    let _ = directory(&dir, OsStr::new("refs"))?;
    check_entries(&dir, format, &mut Vec::new(), &mut 0, deadline)?;
    Ok(BareDirectory {
        file: dir,
        path: PathBuf::from(location.root().path()).join(relative),
    })
}

pub fn bare_config(format: ObjectFormat) -> &'static str {
    match format {
        ObjectFormat::Sha1 => "[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
        ObjectFormat::Sha256 => {
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n[extensions]\n\tobjectformat = sha256\n"
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn open_root(root: StoreRoot) -> io::Result<File> {
    let slash = File::open("/")?;
    // This one lookup crosses onto the role's mount. Descendants may not do so.
    let file = open_relative(
        &slash,
        OsStr::new(root.path().trim_start_matches('/')),
        libc::O_RDONLY | libc::O_DIRECTORY,
        false,
    )?;
    check_owner(&file, true)?;
    if matches!(root, StoreRoot::Incoming) && file.metadata()?.mode() & 0o077 != 0 {
        return Err(invalid("private Git store is accessible to other users"));
    }
    Ok(file)
}

fn directory(parent: &File, name: &OsStr) -> io::Result<File> {
    let file = child(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
    check_owner(&file, true)?;
    Ok(file)
}

fn regular(parent: &File, name: &OsStr) -> io::Result<File> {
    let node = child(parent, name, libc::O_PATH)?;
    check_owner(&node, false)?;
    let file = child(parent, name, libc::O_RDONLY)?;
    check_owner(&file, false)?;
    let before = node.metadata()?;
    let opened = file.metadata()?;
    if (before.dev(), before.ino()) != (opened.dev(), opened.ino()) {
        return Err(invalid("Git store inode changed while opening"));
    }
    Ok(file)
}

pub fn initialize() -> io::Result<()> {
    let canonical = open_root(StoreRoot::Canonical)?;
    for name in ["repositories", "manifests", "locks"] {
        create_directory(&canonical, CString::new(name)?.as_c_str(), 0o755)?;
    }
    let incoming = open_root(StoreRoot::Incoming)?;
    for name in ["incoming", "quarantine"] {
        let directory = create_directory(&incoming, CString::new(name)?.as_c_str(), 0o700)?;
        if directory.metadata()?.mode() & 0o077 != 0 {
            return Err(invalid(
                "private Git subdirectory is accessible to other users",
            ));
        }
    }
    Ok(())
}

fn create_directory(parent: &File, name: &std::ffi::CStr, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: parent and name remain live through mkdirat.
    let status = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) };
    if status != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
        return Err(io::Error::last_os_error());
    }
    directory(parent, OsStr::from_bytes(name.to_bytes()))
}

fn child(parent: &File, name: &OsStr, flags: i32) -> io::Result<File> {
    open_relative(parent, name, flags, true)
}

fn open_relative(parent: &File, name: &OsStr, flags: i32, same_mount: bool) -> io::Result<File> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let name = CString::new(name.as_bytes())?;
    let flags = if flags & libc::O_PATH == 0 {
        flags | libc::O_NONBLOCK
    } else {
        flags
    };
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        // RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | optional RESOLVE_NO_XDEV.
        resolve: 0x08 | 0x04 | u64::from(same_mount),
    };
    // SAFETY: parent and name refer to live objects and the kernel copies how.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent.as_raw_fd(),
            name.as_ptr(),
            &how,
            size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful openat2 returns a fresh owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

fn check_owner(file: &File, directory: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    if metadata.uid() != SYNC_UID
        || metadata.gid() != SYNC_GID
        || metadata.mode() & 0o7022 != 0
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file() || metadata.nlink() != 1
        }
    {
        return Err(invalid(
            "unexpected Git store ownership, mode or entry type",
        ));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn allowed_entry(format: ObjectFormat, parents: &[Vec<u8>], name: &[u8], is_dir: bool) -> bool {
    match parents {
        [] if is_dir => matches!(name, b"objects" | b"refs" | b"info"),
        [] => matches!(name, b"config" | b"HEAD" | b"packed-refs"),
        [root] if root == b"objects" => {
            is_dir && (matches!(name, b"pack" | b"info") || (name.len() == 2 && hex(name)))
        }
        [root, subdir] if root == b"objects" && subdir == b"pack" => {
            !is_dir
                && name.strip_prefix(b"pack-").is_some_and(|name| {
                    [b".pack".as_slice(), b".idx", b".rev", b".bitmap"]
                        .iter()
                        .filter_map(|suffix| name.strip_suffix(*suffix))
                        .any(|digest| digest.len() == format.digest_len() * 2 && hex(digest))
                })
        }
        [root, subdir] if root == b"objects" && subdir.len() == 2 && hex(subdir) => {
            !is_dir && name.len() == format.digest_len() * 2 - 2 && hex(name)
        }
        [root] if root == b"refs" => is_dir && matches!(name, b"heads" | b"tags" | b"gh-docs"),
        [root, _, ..] if root == b"refs" => true,
        _ => false,
    }
}

fn check_entries(
    dir: &File,
    format: ObjectFormat,
    parents: &mut Vec<Vec<u8>>,
    count: &mut usize,
    deadline: Instant,
) -> io::Result<()> {
    if parents.len() > MAX_DEPTH {
        return Err(invalid("Git store directory depth exceeded"));
    }
    for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
        *count += 1;
        if *count > MAX_ENTRIES || Instant::now() >= deadline {
            return Err(invalid("Git store inspection budget exceeded"));
        }
        let entry = entry?;
        let name = entry.file_name();
        // O_PATH avoids opening untrusted device or FIFO types for I/O.
        let file = child(dir, &name, libc::O_PATH)?;
        let is_dir = file.metadata()?.is_dir();
        if !allowed_entry(format, parents, name.as_bytes(), is_dir) {
            return Err(invalid("unexpected bare Git path"));
        }
        check_owner(&file, is_dir)?;
        if is_dir {
            let file = directory(dir, &name)?;
            parents.push(name.as_bytes().to_vec());
            check_entries(&file, format, parents, count, deadline)?;
            parents.pop();
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub enum LockScope {
    Coordinator,
    Repository,
    Generation(i64),
}

impl LockScope {
    fn filename(self) -> io::Result<String> {
        match self {
            Self::Coordinator => Ok("coordinator.lock".into()),
            Self::Repository => Ok("repository.lock".into()),
            Self::Generation(id) if id >= 0 => Ok(format!("generation-{id}.lock")),
            Self::Generation(_) => Err(invalid("invalid generation ID")),
        }
    }
}

pub enum LockMode {
    Shared,
    Exclusive,
}

pub struct Fence {
    _file: File,
}

fn lock_root() -> io::Result<File> {
    directory(&open_root(StoreRoot::Canonical)?, OsStr::new("locks"))
}

/// Creates a stable lock exclusively or validates the existing inode. This
/// method never unlinks a path, including after a partial initialization.
pub fn ensure_lock(repo: RepositoryId, scope: LockScope) -> io::Result<()> {
    let root = lock_root()?;
    let name = CString::new(repo.to_string())?;
    let dir = create_directory(&root, name.as_c_str(), 0o755)?;
    let filename = CString::new(scope.filename()?)?;
    // SAFETY: the single fixed name is created without opening an existing entry.
    let result = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            filename.as_ptr(),
            libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_CREAT | libc::O_EXCL,
            0o644,
        )
    };
    if result >= 0 {
        // SAFETY: openat returned a fresh owned descriptor.
        unsafe { File::from_raw_fd(result) }.sync_all()?;
    } else if io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
        return Err(io::Error::last_os_error());
    }
    let file = child(&dir, OsStr::from_bytes(filename.to_bytes()), libc::O_PATH)?;
    check_owner(&file, false)?;
    dir.sync_all()?;
    root.sync_all()
}

pub fn lock(
    repo: RepositoryId,
    scope: LockScope,
    mode: LockMode,
    deadline: Instant,
) -> io::Result<Fence> {
    let root = lock_root()?;
    let repo = directory(&root, OsStr::new(&repo.to_string()))?;
    let filename = scope.filename()?;
    let file = regular(&repo, OsStr::new(&filename))?;
    let operation = match mode {
        LockMode::Shared => libc::LOCK_SH,
        LockMode::Exclusive => libc::LOCK_EX,
    };
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Git fence deadline",
            ));
        }
        // SAFETY: the descriptor remains owned by the returned fence.
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
            return Ok(Fence { _file: file });
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            return Err(error);
        }
        thread::sleep(Duration::from_millis(10));
    }
}
