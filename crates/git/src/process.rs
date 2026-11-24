//! Bounded pipes and process teardown for the fixed Git worker.

use std::{
    io::{self, BufReader, Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::process::CommandExt,
    },
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(crate) struct Process {
    child: Child,
    input: Option<ChildStdin>,
    pub(crate) output: BufReader<Output>,
    deadline: Instant,
    reaped: bool,
}

impl Process {
    pub(crate) fn spawn(mut command: Command, deadline: Instant) -> io::Result<Self> {
        ensure_time(deadline)?;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        let process = Self {
            child,
            input: Some(input),
            output: BufReader::new(Output {
                pipe: output,
                deadline,
            }),
            deadline,
            reaped: false,
        };
        nonblocking(process.input.as_ref().expect("piped stdin").as_raw_fd())?;
        nonblocking(process.output.get_ref().pipe.as_raw_fd())?;
        Ok(process)
    }

    pub(crate) fn send(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        let pipe = self
            .input
            .as_mut()
            .ok_or_else(|| io::Error::other("closed Git stdin"))?;
        while !bytes.is_empty() {
            ready(pipe.as_raw_fd(), libc::POLLOUT, self.deadline)?;
            match pipe.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => bytes = &bytes[written..],
                Err(error) if retry(&error) => (),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.input.take();
        if self.output.read(&mut [0])? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing Git output",
            ));
        }
        loop {
            ensure_time(self.deadline)?;
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other("confined Git worker failed"))
                };
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.reaped {
            // SAFETY: the unreaped child's reserved PID is its process-group ID.
            unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

pub(crate) struct Output {
    pipe: ChildStdout,
    deadline: Instant,
}

impl Read for Output {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            ready(self.pipe.as_raw_fd(), libc::POLLIN, self.deadline)?;
            match self.pipe.read(out) {
                Err(error) if retry(&error) => (),
                result => return result,
            }
        }
    }
}

fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

pub(crate) fn ensure_time(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Git worker deadline exceeded"))
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd is an owned process pipe and fcntl doesn't retain it.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn ready(fd: RawFd, events: i16, deadline: Instant) -> io::Result<()> {
    let mut event = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        let millis = ensure_time(deadline)?.as_millis().saturating_add(1);
        // SAFETY: event is a live single-element pollfd array.
        let result = unsafe { libc::poll(&mut event, 1, millis.min(i32::MAX as u128) as i32) };
        if result > 0 {
            return Ok(());
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_kills_and_reaps_worker() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "while :; do :; done"]);
        let mut worker =
            Process::spawn(command, Instant::now() + Duration::from_millis(75)).unwrap();
        let pid = worker.child.id() as libc::pid_t;
        assert_eq!(
            worker.output.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        drop(worker);
        // SAFETY: waitpid queries the former child PID without waiting.
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
