use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream as BlockingStream,
    },
    process::ExitStatus,
};

use docs_mcp_core::Result;
use tokio::{io::AsyncReadExt, net::UnixStream, process::Command};

pub const CHILD: &str = "--phase2-supervised";
const LIFELINE: i32 = 3;

pub struct Worker {
    lifeline: Option<BlockingStream>,
    child: tokio::process::Child,
}

impl Worker {
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    pub fn cancel(&mut self) {
        self.lifeline.take();
    }
}

pub fn spawn(args: &[String], quiet: bool) -> Result<Worker> {
    let (lifeline, worker) = BlockingStream::pair()?;
    let fd = worker.as_raw_fd();
    let mut command = Command::new(std::env::current_exe()?);
    command.arg(CHILD).args(args);
    // Install the lifeline at a fixed descriptor in the self-exec child.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, LIFELINE) == -1 || libc::fcntl(LIFELINE, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    if quiet {
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }
    let child = command.spawn()?;
    drop(worker);
    Ok(Worker {
        lifeline: Some(lifeline),
        child,
    })
}

pub async fn run(args: &[String]) -> Result<ExitStatus> {
    let mut child = spawn(args, false)?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = child.wait() => return Ok(result?),
        _ = tokio::signal::ctrl_c() => (),
        _ = term.recv() => (),
    }
    child.cancel();
    Ok(child.wait().await?)
}

pub fn child_lifeline() -> Result<UnixStream> {
    // Validate the inherited descriptor before taking ownership. A caller
    // without the self-exec lifeline must use the outer entry point.
    let mut kind: libc::c_int = 0;
    let mut len = std::mem::size_of_val(&kind) as libc::socklen_t;
    let socket = unsafe {
        libc::getsockopt(
            LIFELINE,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if socket != 0 || kind != libc::SOCK_STREAM {
        return Err("missing synchronization supervisor".into());
    }
    if unsafe { libc::fcntl(LIFELINE, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error().into());
    }
    let lifeline = unsafe { BlockingStream::from_raw_fd(LIFELINE) };
    lifeline.peer_addr()?;
    lifeline.set_nonblocking(true)?;
    Ok(UnixStream::from_std(lifeline)?)
}

pub async fn until_lost(lifeline: &mut UnixStream) {
    let _ = lifeline.read_u8().await;
}
