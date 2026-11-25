//! Containment shared by the fixed Git and ripgrep workers.
//! Adapted from rfc-mcp-docker bcb4181. See docs/RFC-MCP-DOCKER-LICENSE.

use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    net::{TcpListener, TcpStream},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::process::CommandExt,
    },
    process::Command,
};

const ALLOWED: &str = "/git-store/fixture.txt";
const SECRET: &str = "/run/secrets/db-password";
const SCRATCH: &str = "/tmp/phase0-denied";
const RG: &str = "/usr/local/bin/rg";
pub(crate) const GIT: &str = "/usr/local/bin/git";
const SELECTED: &str = "/git-store/repositories/selected.git";
const UNRELATED: &str = "/git-store/repositories/unrelated.git/HEAD";
const SELECTED_HEAD: &str = "/git-store/repositories/selected.git/HEAD";
const CPU_SECONDS: u64 = 2;

#[derive(Clone, Copy)]
pub enum Profile {
    Git,
    Search,
    FramedSearch,
}

impl Profile {
    fn executable(self) -> Option<&'static str> {
        match self {
            Self::Git => Some(GIT),
            Self::Search => Some(RG),
            Self::FramedSearch => None,
        }
    }
}

/// Exercise the same restrictions installed before the executable transition.
/// Invoke only from a fresh single-threaded process with ordinary stdio.
pub fn probe(profile: Profile, force_failure: bool) -> io::Result<()> {
    let mut denied = vec![UNRELATED, SECRET, "/proc/self/environ"];
    if matches!(profile, Profile::Search) {
        denied.extend([ALLOWED, SELECTED_HEAD]);
        let bytes = fs::read(ALLOWED)?;
        if bytes.len() > 4096 {
            return Err(io::Error::other("search fixture exceeds pipe bound"));
        }
        let (input, mut writer) = io::pipe()?;
        writer.write_all(&bytes)?;
        drop(writer);
        // SAFETY: replace this dedicated probe's stdin with the bounded input pipe.
        checked(unsafe { libc::dup2(input.as_raw_fd(), 0) }.into())?;
    }

    if matches!(profile, Profile::FramedSearch) {
        denied.extend([ALLOWED, SELECTED_HEAD, RG, "/run/secrets/cursor.key"]);
    }

    // Positive controls keep the probe from passing merely because a mount,
    // secret, scratch path, or the socket did not exist before confinement.
    for path in &denied {
        File::open(path)?;
    }
    File::create_new(SCRATCH)?;
    fs::remove_file(SCRATCH)?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    let inherited_fd = TcpStream::connect(address)?.into_raw_fd();
    let listening_fd = listener.into_raw_fd();

    let repository = if matches!(profile, Profile::Git) {
        Some(File::open(SELECTED)?)
    } else {
        None
    };
    let abi = confine(profile, repository, force_failure)
        .map_err(|error| io::Error::other(format!("sandbox_unavailable: {error}")))?;
    for fd in [inherited_fd, listening_fd] {
        // SAFETY: F_GETFD only queries the descriptor number.
        let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if result != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
            return Err(io::Error::other("inherited socket was not closed"));
        }
    }
    for path in denied {
        require_denial(File::open(path), path)?;
    }
    require_denial(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(SCRATCH),
        "file creation",
    )?;
    require_denial(TcpStream::connect(address), "network socket")?;
    require_denial(std::os::unix::net::UnixStream::pair(), "Unix sockets")?;
    // SAFETY: this fork-style clone either fails or creates a short-lived
    // process without a signal handler or shared mutable memory.
    let result = unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) };
    if result == 0 {
        // SAFETY: terminate the unexpected child immediately.
        unsafe { libc::_exit(1) };
    }
    if result > 0 {
        // SAFETY: reap the child identified by successful clone.
        unsafe { libc::waitpid(result as i32, std::ptr::null_mut(), 0) };
        return Err(io::Error::other("child creation unexpectedly allowed"));
    }
    require_denial(Err::<(), _>(io::Error::last_os_error()), "child creation")?;
    let exec_error = Command::new("/bin/true").env_clear().exec();
    require_denial(Err::<(), _>(exec_error), "unapproved executable")?;
    if Vec::<u8>::new()
        .try_reserve_exact(512 * 1024 * 1024)
        .is_ok()
    {
        return Err(io::Error::other("worker exceeded its address-space limit"));
    }
    eprintln!("Landlock ABI {abi}, seccomp, inherited descriptors and denial controls passed");

    match profile {
        Profile::FramedSearch => Ok(()),
        Profile::Search => exec_rg("phase-zero", true, true),
        Profile::Git => Err(Command::new(GIT)
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1")
            .arg(format!("--git-dir={SELECTED}"))
            .args([
                "-c",
                "maintenance.auto=false",
                "-c",
                "gc.auto=0",
                "-c",
                "safe.directory=/git-store/repositories/selected.git",
                "cat-file",
                "blob",
                "HEAD:fixture.txt",
            ])
            .exec()),
    }
}

pub(crate) fn confine(
    profile: Profile,
    repository: Option<File>,
    force_failure: bool,
) -> io::Result<libc::c_long> {
    std::env::set_current_dir("/")?;
    no_new_privileges()?;
    let abi = install_landlock(profile, repository.as_ref(), force_failure)?;
    drop(repository);
    // SAFETY: the dedicated helper only needs stdin, stdout and stderr.
    checked(unsafe { libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32) })?;
    set_limit(libc::RLIMIT_CPU, CPU_SECONDS as libc::rlim_t)?;
    let memory_mib = if matches!(profile, Profile::FramedSearch) {
        64
    } else {
        256
    };
    set_limit(libc::RLIMIT_AS, memory_mib * 1024 * 1024)?;
    set_limit(libc::RLIMIT_FSIZE, 0)?;
    set_limit(libc::RLIMIT_CORE, 0)?;
    install_seccomp(false)?;
    Ok(abi)
}

fn exec_rg(pattern: &str, literal: bool, case_sensitive: bool) -> io::Result<()> {
    let mut command = Command::new(RG);
    command.env_clear().args([
        "--json",
        "--no-config",
        "--color",
        "never",
        "--encoding",
        "none",
        "--text",
        "--no-mmap",
        "--threads",
        "1",
        "--engine",
        "default",
    ]);
    if literal {
        command.arg("--fixed-strings");
    }
    command.arg(if case_sensitive {
        "--case-sensitive"
    } else {
        "--ignore-case"
    });
    Err(command.args(["-e", pattern, "--", "-"]).exec())
}

fn require_denial<T>(result: io::Result<T>, operation: &str) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == ErrorKind::PermissionDenied => Ok(()),
        Err(error) => Err(io::Error::other(format!("{operation}: unexpected {error}"))),
        Ok(_) => Err(io::Error::other(format!(
            "{operation}: unexpectedly allowed"
        ))),
    }
}

fn checked(result: libc::c_long) -> io::Result<libc::c_long> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn no_new_privileges() -> io::Result<()> {
    // SAFETY: the fixed prctl arguments set only the calling process attribute.
    checked(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }.into())?;
    Ok(())
}

#[cfg(target_env = "gnu")]
type Resource = libc::__rlimit_resource_t;
#[cfg(not(target_env = "gnu"))]
type Resource = libc::c_int;

fn set_limit(resource: Resource, limit: libc::rlim_t) -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: limit,
        rlim_max: limit,
    };
    // SAFETY: the rlimit pointer lives for the call.
    checked(unsafe { libc::setrlimit(resource, &limit) }.into())?;
    Ok(())
}

#[repr(C, packed)]
struct PathBeneath {
    allowed_access: u64,
    parent_fd: i32,
}

fn install_landlock(
    profile: Profile,
    repository: Option<&File>,
    force_failure: bool,
) -> io::Result<libc::c_long> {
    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    // SAFETY: the ABI query expects a null attribute, size zero and VERSION flag.
    let abi = checked(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0_usize,
            1_u32,
        )
    })?;
    if abi < 3 {
        return Err(io::Error::other(format!(
            "Landlock ABI {abi} lacks truncate mediation"
        )));
    }

    // Filesystem rights through TRUNCATE (ABI 3) and IOCTL_DEV (ABI 5).
    let handled_fs: u64 = (1 << if abi >= 5 { 16 } else { 15 }) - 1;
    // SAFETY: the first u64 of landlock_ruleset_attr is handled_access_fs.
    let ruleset = checked(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &handled_fs,
            size_of::<u64>(),
            0_u32,
        )
    })?;
    // SAFETY: a successful create call returned a new owned descriptor.
    let ruleset = unsafe { File::from_raw_fd(ruleset as i32) };
    if let Some(executable) = profile.executable() {
        add_landlock_rule(&ruleset, executable, READ_FILE | EXECUTE)?;
    }
    if matches!(profile, Profile::Git) {
        let repository = repository.ok_or_else(|| io::Error::other("missing Git sandbox root"))?;
        add_landlock_anchor(&ruleset, repository, READ_FILE | READ_DIR)?;
        // Git opens this exact device O_RDWR while sanitizing stdio.
        add_landlock_rule(&ruleset, "/dev/null", READ_FILE | WRITE_FILE)?;
    }
    let restriction = if force_failure {
        -1
    } else {
        ruleset.as_raw_fd()
    };
    // SAFETY: the live ruleset descriptor is owned through the call. The
    // forced failure passes an invalid descriptor to the real setup syscall.
    checked(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, restriction, 0_u32) })?;
    Ok(abi)
}

fn add_landlock_rule(ruleset: &File, path: &str, access: u64) -> io::Result<()> {
    let path = CString::new(path)?;
    // SAFETY: path is a valid C string and open creates an owned descriptor.
    let fd = checked(
        unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        }
        .into(),
    )?;
    // SAFETY: this is a fresh descriptor returned by open above.
    let anchor = unsafe { File::from_raw_fd(fd as i32) };
    add_landlock_anchor(ruleset, &anchor, access)
}

fn add_landlock_anchor(ruleset: &File, anchor: &File, access: u64) -> io::Result<()> {
    let rule = PathBeneath {
        allowed_access: access,
        parent_fd: anchor.as_raw_fd(),
    };
    // SAFETY: the packed argument matches landlock_path_beneath_attr.
    checked(unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            1_u32,
            &rule,
            0_u32,
        )
    })?;
    Ok(())
}

fn install_seccomp(sync: bool) -> io::Result<()> {
    // Audit architectures and seccomp return actions from Linux UAPI.
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7;
    const ALLOW: u32 = 0x7fff_0000;
    const KILL_PROCESS: u32 = 0x8000_0000;
    const DENY: u32 = 0x0005_0000 | libc::EPERM as u32;

    // Only process start-up, local file inspection, memory, signals and stdio
    // are needed by the two static executables. Unknown syscalls fail.
    let syscalls = [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_close,
        libc::SYS_ppoll,
        libc::SYS_openat,
        libc::SYS_newfstatat,
        libc::SYS_fstat,
        libc::SYS_statx,
        libc::SYS_getdents64,
        libc::SYS_readlinkat,
        libc::SYS_faccessat,
        libc::SYS_faccessat2,
        libc::SYS_lseek,
        libc::SYS_pread64,
        libc::SYS_fcntl,
        libc::SYS_ioctl,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_madvise,
        libc::SYS_brk,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_set_tid_address,
        libc::SYS_set_robust_list,
        libc::SYS_futex,
        libc::SYS_rseq,
        libc::SYS_sched_getaffinity,
        libc::SYS_prlimit64,
        libc::SYS_getrandom,
        libc::SYS_clock_gettime,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getcwd,
        libc::SYS_chdir,
        libc::SYS_execve,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ];
    #[cfg(target_arch = "x86_64")]
    let syscalls = syscalls.into_iter().chain([
        libc::SYS_arch_prctl,
        libc::SYS_poll,
        libc::SYS_open,
        libc::SYS_stat,
        libc::SYS_lstat,
        libc::SYS_access,
        libc::SYS_readlink,
    ]);

    let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let mut filter = vec![
        instruction(0x20, 0, 0, 4), // seccomp_data.arch
        instruction(0x15, 1, 0, ARCH),
        instruction(0x06, 0, 0, KILL_PROCESS),
        instruction(0x20, 0, 0, 0), // seccomp_data.nr
    ];
    for syscall in syscalls {
        filter.push(instruction(0x15, 0, 1, syscall as u32));
        filter.push(instruction(0x06, 0, 0, ALLOW));
    }
    if sync {
        for syscall in [
            libc::SYS_wait4,
            libc::SYS_pipe2,
            libc::SYS_dup,
            libc::SYS_dup3,
            libc::SYS_fsync,
            libc::SYS_fdatasync,
            libc::SYS_fchmod,
            libc::SYS_fcntl,
            libc::SYS_mkdirat,
            libc::SYS_unlinkat,
            libc::SYS_renameat,
            libc::SYS_renameat2,
            libc::SYS_linkat,
            libc::SYS_getppid,
            libc::SYS_getuid,
            libc::SYS_geteuid,
            libc::SYS_getgid,
            libc::SYS_getegid,
            libc::SYS_umask,
            libc::SYS_gettimeofday,
            libc::SYS_times,
            libc::SYS_getrlimit,
            libc::SYS_getrusage,
            libc::SYS_uname,
            libc::SYS_ftruncate,
            libc::SYS_writev,
            libc::SYS_nanosleep,
            libc::SYS_clock_nanosleep,
            libc::SYS_getpgid,
            libc::SYS_sched_yield,
        ] {
            filter.push(instruction(0x15, 0, 1, syscall as u32));
            filter.push(instruction(0x06, 0, 0, ALLOW));
        }
        #[cfg(target_arch = "x86_64")]
        for syscall in [
            libc::SYS_pipe,
            libc::SYS_dup2,
            libc::SYS_mkdir,
            libc::SYS_unlink,
            libc::SYS_rename,
            libc::SYS_link,
            libc::SYS_fork,
            libc::SYS_vfork,
        ] {
            filter.push(instruction(0x15, 0, 1, syscall as u32));
            filter.push(instruction(0x06, 0, 0, ALLOW));
        }
        // Git and musl may fork or create threads. Keep namespaces, process
        // group changes and children reparented outside the worker denied.
        let clone_flags = 0xff
            | libc::CLONE_VM
            | libc::CLONE_FS
            | libc::CLONE_FILES
            | libc::CLONE_SIGHAND
            | libc::CLONE_THREAD
            | libc::CLONE_SYSVSEM
            | libc::CLONE_SETTLS
            | libc::CLONE_PARENT_SETTID
            | libc::CLONE_CHILD_SETTID
            | libc::CLONE_CHILD_CLEARTID
            | libc::CLONE_DETACHED
            | libc::CLONE_VFORK;
        filter.extend([
            instruction(0x15, 0, 3, libc::SYS_clone as u32),
            instruction(0x20, 0, 0, 16),
            instruction(0x45, 1, 0, !clone_flags as u32),
            instruction(0x06, 0, 0, ALLOW),
        ]);
    }
    filter.push(instruction(0x06, 0, 0, DENY));
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).expect("fixed seccomp program fits"),
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: the kernel copies the initialized BPF program during this call.
    checked(unsafe { libc::syscall(libc::SYS_seccomp, 1_u32, 0_u32, &program) })?;
    Ok(())
}

pub fn confine_sync(scratch: &str, broker: bool) -> io::Result<()> {
    confine_sync_scope(scratch, broker, true)
}

pub fn confine_publication_validation(odb: &str) -> io::Result<()> {
    confine_sync_scope(odb, false, false)
}

fn confine_sync_scope(scratch: &str, broker: bool, write: bool) -> io::Result<()> {
    const EXECUTE: u64 = 1;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    let abi = checked(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0,
            1,
        )
    })?;
    if abi < 6 {
        return Err(io::Error::other("sync requires Landlock ABI 6"));
    }
    no_new_privileges()?;
    let attributes = [(1_u64 << 16) - 1, 0, 1_u64 << 1];
    // Scope signals as well as filesystem access. Unix/TCP socket creation
    // is denied by seccomp, including for Git's helper descendants.
    let ruleset = checked(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            attributes.as_ptr(),
            24,
            0,
        )
    })?;
    let ruleset = unsafe { File::from_raw_fd(ruleset as i32) };
    add_landlock_rule(&ruleset, GIT, EXECUTE | READ_FILE)?;
    if broker {
        add_landlock_rule(
            &ruleset,
            "/usr/local/libexec/gh-docs-git/git-remote-ghdocs",
            EXECUTE | READ_FILE,
        )?;
    }
    add_landlock_rule(&ruleset, "/dev/null", READ_FILE | WRITE_FILE)?;
    // Scratch can contain regular files and directories. Deny executable,
    // symlink, socket and special-file creation even within the slot.
    let mut scratch_rights = READ_FILE | READ_DIR;
    if write {
        scratch_rights |=
            WRITE_FILE | (1 << 4) | (1 << 5) | (1 << 7) | (1 << 8) | (1 << 13) | (1 << 14);
    }
    add_landlock_rule(&ruleset, scratch, scratch_rights)?;
    checked(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0) })?;
    drop(ruleset);
    let first = if broker { 4_u32 } else { 3_u32 };
    checked(unsafe { libc::syscall(libc::SYS_close_range, first, u32::MAX, 0_u32) })?;
    set_limit(libc::RLIMIT_CPU, 180)?;
    set_limit(libc::RLIMIT_AS, 768 * 1024 * 1024)?;
    set_limit(libc::RLIMIT_FSIZE, 272 * 1024 * 1024)?;
    set_limit(libc::RLIMIT_CORE, 0)?;
    install_seccomp(true)
}

/// Install the framed worker policy in a freshly executed, single-threaded process.
pub fn confine_framed_search(force_failure: bool) -> io::Result<()> {
    confine(Profile::FramedSearch, None, force_failure).map(|_| ())
}
