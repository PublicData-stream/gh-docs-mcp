use std::{
    fs::File,
    io::{self, BufRead, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream as BlockingStream,
    },
    path::Path,
    process::Stdio,
    time::Duration,
};

use gh_docs_core::Result;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::{Child, Command},
};

pub const WORKER: &str = "/usr/local/bin/gh-docs-git-sync";
const GIT: &str = "/usr/local/bin/git";
const MAX_REQUEST: usize = 256 * 1024;
const MAX_FRAME: usize = 65_520;
const MAX_STDERR: usize = 64 * 1024;

#[derive(Debug)]
pub struct CommandFailure(Option<i32>);

impl CommandFailure {
    pub fn possibly_invalid_input(&self) -> bool {
        matches!(self.0, Some(1 | 128))
    }
}

impl std::fmt::Display for CommandFailure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("confined Git command failed")
    }
}

impl std::error::Error for CommandFailure {}

pub struct GitProcess {
    child: Child,
    group: i32,
}

impl GitProcess {
    pub fn spawn(
        scratch: &Path,
        odb: &Path,
        args: &[std::ffi::OsString],
        broker: Option<&BlockingStream>,
    ) -> Result<Self> {
        Self::spawn_with_input(scratch, odb, args, broker, Stdio::null(), false)
    }

    fn spawn_with_input(
        scratch: &Path,
        odb: &Path,
        args: &[std::ffi::OsString],
        broker: Option<&BlockingStream>,
        input: Stdio,
        readonly: bool,
    ) -> Result<Self> {
        if !readonly {
            validate_odb(odb)?;
        }
        let mut command = Command::new(WORKER);
        command
            .env_clear()
            .arg(if readonly {
                "publication-read"
            } else if broker.is_some() {
                "fetch"
            } else {
                "local"
            })
            .arg(scratch)
            .arg(odb)
            .args(args)
            .stdin(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let fd = broker.map(AsRawFd::as_raw_fd);
        // The pre-exec hook uses only async-signal-safe syscalls. The helper
        // closes every descriptor except stdio and the optional broker channel.
        unsafe {
            command.pre_exec(move || {
                if let Some(fd) = fd
                    && (libc::dup2(fd, 3) == -1 || libc::fcntl(3, libc::F_SETFD, 0) == -1)
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let group = child.id().ok_or("Git worker missing process ID")? as i32;
        Ok(Self { child, group })
    }

    pub async fn output(&mut self, max: usize) -> Result<Vec<u8>> {
        let stdout = self.child.stdout.take().ok_or("missing Git stdout")?;
        let stderr = self.child.stderr.take().ok_or("missing Git stderr")?;
        let (stdout, stderr, status) = tokio::try_join!(
            read_capped(stdout, max),
            read_capped(stderr, MAX_STDERR),
            async {
                // Peek without reaping. The supervisor retains the group
                // identity until it has signalled every descendant.
                self.wait_for_exit().await
            },
        )?;
        if status != Some(0) {
            #[cfg(feature = "fixtures")]
            if std::env::var_os("GH_DOCS_PHASE2_DEBUG").is_some_and(|value| value == "1") {
                eprintln!(
                    "synthetic Git worker status {status:?}: {}",
                    String::from_utf8_lossy(&stderr)
                );
            }
            drop(stderr);
            return Err(CommandFailure(status).into());
        }
        Ok(stdout)
    }

    async fn wait_for_exit(&mut self) -> Result<Option<i32>> {
        let group = self.group;
        let status = tokio::task::spawn_blocking(move || {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            loop {
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        group as u32,
                        &mut info,
                        libc::WEXITED | libc::WNOWAIT,
                    )
                };
                if result == 0 {
                    return Ok(
                        (info.si_code == libc::CLD_EXITED).then(|| unsafe { info.si_status() })
                    );
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        })
        .await??;
        self.stop();
        Ok(status)
    }

    pub fn stop(&mut self) {
        if self.group != 0 {
            let group = self.group;
            unsafe { libc::kill(-group, libc::SIGKILL) };
            // Reap any killed descendants adopted by this process.
            loop {
                let result = unsafe { libc::waitpid(-group, std::ptr::null_mut(), 0) };
                if result == -1 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
            self.group = 0;
        }
    }
}

fn validate_odb(odb: &Path) -> Result<()> {
    use std::fs::{read_dir, symlink_metadata};
    if !symlink_metadata(odb)?.is_dir() {
        return Err("invalid private Git directory".into());
    }
    let read = |name| -> Result<Vec<u8>> {
        let mut data = Vec::new();
        crate::candidate::open_regular(&odb.join(name))?
            .take(1025)
            .read_to_end(&mut data)?;
        Ok(data)
    };
    let config = read("config")?;
    if ![
        gh_docs_core::git::ObjectFormat::Sha1,
        gh_docs_core::git::ObjectFormat::Sha256,
    ]
    .iter()
    .any(|&format| config == gh_docs_git::store::bare_config(format).as_bytes())
        || read("HEAD")? != b"ref: refs/heads/main\n"
    {
        return Err("private Git configuration changed".into());
    }
    for entry in read_dir(odb)? {
        let entry = entry?;
        match (
            entry.file_name().as_encoded_bytes(),
            entry.file_type()?.is_dir(),
        ) {
            (b"config" | b"HEAD", false) | (b"objects" | b"refs", true) => (),
            _ => return Err("unexpected private Git repository entry".into()),
        }
    }
    for entry in read_dir(odb.join("objects"))? {
        let entry = entry?;
        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        if !entry.file_type()?.is_dir()
            || (bytes != b"pack"
                && !(bytes.len() == 2
                    && bytes
                        .iter()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))))
        {
            return Err("unexpected object store dependency".into());
        }
    }
    Ok(())
}

impl Drop for GitProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn read_capped(input: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    input
        .take(limit as u64 + 1)
        .read_to_end(&mut result)
        .await?;
    if result.len() > limit {
        return Err("Git worker output exceeded its limit".into());
    }
    Ok(result)
}

pub async fn local(
    scratch: &Path,
    odb: &Path,
    args: &[std::ffi::OsString],
    max: usize,
) -> Result<Vec<u8>> {
    local_with_input(scratch, odb, args, max, Stdio::null()).await
}

pub async fn local_with_input(
    scratch: &Path,
    odb: &Path,
    args: &[std::ffi::OsString],
    max: usize,
    input: Stdio,
) -> Result<Vec<u8>> {
    let mut child = GitProcess::spawn_with_input(scratch, odb, args, None, input, false)?;
    let result = tokio::time::timeout(Duration::from_secs(180), child.output(max)).await;
    child.stop();
    result?
}

pub async fn publication_read(
    repo: gh_docs_core::git::RepositoryId,
    format: gh_docs_core::git::ObjectFormat,
    args: &[std::ffi::OsString],
    max: usize,
    input: Stdio,
) -> Result<Vec<u8>> {
    let odb = gh_docs_git::store::validate_canonical(repo, format)?;
    let mut child =
        GitProcess::spawn_with_input(Path::new(format.name()), &odb, args, None, input, true)?;
    let result = tokio::time::timeout(Duration::from_secs(180), child.output(max)).await;
    child.stop();
    result?
}

pub fn launch(args: &[std::ffi::OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let [mode, scratch, odb, command @ ..] = args else {
        return Err("invalid Git worker invocation".into());
    };
    let broker = mode == "fetch";
    let readonly = mode == "publication-read";
    if (!broker && !readonly && mode != "local") || command.is_empty() {
        return Err("invalid Git worker mode".into());
    }
    let parent = unsafe { libc::getppid() };
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0
        || parent == 1
        || parent != unsafe { libc::getppid() }
    {
        return Err("Git worker lost its supervisor".into());
    }
    if readonly {
        let source = Path::new(odb);
        let repo = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("invalid canonical path")?
            .parse()?;
        let format = scratch
            .to_str()
            .ok_or("invalid canonical format")?
            .parse()?;
        if gh_docs_git::store::validate_canonical(repo, format)? != source
            || !matches!(
                command.first().and_then(|arg| arg.to_str()),
                Some("fsck" | "cat-file")
            )
        {
            return Err("invalid canonical validation command".into());
        }
        gh_docs_git::sandbox::confine_publication_validation(
            source.to_str().ok_or("invalid canonical path")?,
        )?;
    } else {
        gh_docs_git::sandbox::confine_sync(
            scratch.to_str().ok_or("invalid scratch path")?,
            broker,
        )?;
    }
    let mut git = std::process::Command::new(GIT);
    git.env_clear()
        .env("PATH", "/usr/local/bin")
        .env("GIT_EXEC_PATH", "/usr/local/libexec/gh-docs-git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .current_dir(odb)
        .args([
            "-c",
            "protocol.version=2",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.ghdocs.allow=always",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "fetch.fsckObjects=true",
            "-c",
            "fetch.unpackLimit=1",
            "-c",
            "pack.threads=1",
            "-c",
            "fetch.writeCommitGraph=false",
            "-c",
            "core.logAllRefUpdates=false",
            "-c",
            "core.fsync=all",
            "-c",
            "core.fsyncMethod=fsync",
        ])
        .args(command);
    Err(git.exec().into())
}

pub(crate) fn packet_size(header: &[u8]) -> Result<usize> {
    let length = usize::from_str_radix(std::str::from_utf8(header)?, 16)?;
    if length == 3 || length > MAX_FRAME {
        return Err("invalid Git packet length".into());
    }
    Ok(length)
}

pub fn helper() -> Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    // Treat the inherited channel as a byte stream. File I/O uses read and
    // write and needs no socket syscall in the confined helper.
    let mut channel = unsafe { File::from_raw_fd(3) };
    let mut command = Vec::new();
    for expected in [
        b"capabilities\n".as_slice(),
        b"stateless-connect git-upload-pack\n",
    ] {
        command.clear();
        (&mut input).take(128).read_until(b'\n', &mut command)?;
        if command != expected {
            return Err("unsupported Git helper command".into());
        }
        if expected == b"capabilities\n" {
            output.write_all(b"stateless-connect\n\n")?;
            output.flush()?;
        }
    }
    output.write_all(b"\n")?;
    channel.write_all(&0_u32.to_be_bytes())?;
    forward_response(&mut channel, &mut output)?;
    loop {
        let mut data = Vec::new();
        loop {
            let mut header = [0_u8; 4];
            if input.read(&mut header[..1])? == 0 {
                if data.is_empty() {
                    return Ok(());
                }
                return Err("truncated Git helper request".into());
            }
            input.read_exact(&mut header[1..])?;
            let size = packet_size(&header)?;
            data.extend(header);
            if size >= 4 {
                let start = data.len();
                data.resize(start + size - 4, 0);
                input.read_exact(&mut data[start..])?;
            }
            if data.len() > MAX_REQUEST || size == 2 {
                return Err("invalid Git helper request".into());
            }
            if size == 0 {
                break;
            }
        }
        channel.write_all(&(data.len() as u32).to_be_bytes())?;
        channel.write_all(&data)?;
        forward_response(&mut channel, &mut output)?;
    }
}

fn forward_response(channel: &mut File, output: &mut impl Write) -> Result<()> {
    let mut chunk = vec![0; MAX_FRAME];
    loop {
        let mut header = [0; 4];
        channel.read_exact(&mut header)?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 {
            break;
        }
        if length > MAX_FRAME {
            return Err("invalid broker frame".into());
        }
        channel.read_exact(&mut chunk[..length])?;
        output.write_all(&chunk[..length])?;
        // Git can consume the response before the broker finishes streaming.
        output.flush()?;
    }
    output.flush()?;
    Ok(())
}

pub async fn broker_request(stream: &mut UnixStream) -> Result<Option<Vec<u8>>> {
    let mut header = [0; 4];
    if stream.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    stream.read_exact(&mut header[1..]).await?;
    let size = u32::from_be_bytes(header) as usize;
    if size > MAX_REQUEST {
        return Err("Git broker request exceeded its limit".into());
    }
    let mut data = vec![0; size];
    stream.read_exact(&mut data).await?;
    Ok(Some(data))
}

pub async fn broker_chunk(stream: &mut UnixStream, bytes: &[u8]) -> Result<()> {
    for chunk in bytes.chunks(MAX_FRAME) {
        stream.write_u32(chunk.len() as u32).await?;
        stream.write_all(chunk).await?;
    }
    Ok(())
}

pub async fn broker_end(stream: &mut UnixStream) -> Result<()> {
    stream.write_u32(0).await?;
    Ok(())
}

pub fn validate_post(body: &[u8], wants: &std::collections::BTreeSet<String>) -> Result<bool> {
    let mut remaining = body;
    let mut command = None;
    let mut arguments = false;
    let mut wanted = false;
    while let Some(header) = remaining.get(..4) {
        remaining = &remaining[4..];
        match packet_size(header)? {
            0 if remaining.is_empty() && arguments && (command == Some(false) || wanted) => {
                return Ok(command == Some(true));
            }
            1 if command.is_some() && !arguments => arguments = true,
            size if size > 4 && remaining.len() >= size - 4 => {
                let (line, tail) = remaining.split_at(size - 4);
                remaining = tail;
                let line = line.strip_suffix(b"\n").unwrap_or(line);
                match (command, arguments, line) {
                    (None, false, b"command=ls-refs") => command = Some(false),
                    (None, false, b"command=fetch") if !wants.is_empty() => command = Some(true),
                    (Some(_), false, b"object-format=sha1" | b"object-format=sha256") => (),
                    (Some(_), false, line) if line.starts_with(b"agent=") => (),
                    (Some(false), true, b"peel" | b"symrefs" | b"unborn") => (),
                    (Some(false), true, line) if line.starts_with(b"ref-prefix ") => (),
                    (
                        Some(true),
                        true,
                        b"thin-pack" | b"ofs-delta" | b"no-progress" | b"done" | b"wait-for-done",
                    ) => (),
                    (Some(true), true, line)
                        if line.starts_with(b"have ") && native_hex(&line[5..]) => {}
                    (Some(true), true, line)
                        if line.starts_with(b"want ")
                            && wants.contains(std::str::from_utf8(&line[5..])?) =>
                    {
                        wanted = true;
                    }
                    _ => return Err("unsupported or unapproved Git broker request".into()),
                }
            }
            _ => break,
        }
    }
    Err("invalid Git broker request framing".into())
}

fn native_hex(bytes: &[u8]) -> bool {
    matches!(bytes.len(), 40 | 64)
        && bytes
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}
