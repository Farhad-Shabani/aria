//! Bounded waiting and mandatory cleanup for child process groups.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
use std::thread;
use std::time::Instant;

use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};

use super::POLL_INTERVAL;

pub struct ChildProcess {
    child: Option<Child>,
    /// Kept once the child is reaped, so a later look still sees the exit.
    exit_status: Option<ExitStatus>,
    process_group: u32,
    owns_process_group: bool,
}

impl ChildProcess {
    // This is the single process-spawn boundary protected by `clippy.toml`.
    #[allow(clippy::disallowed_methods)]
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        command.process_group(0);
        command.spawn().map(|child| {
            let process_group = child.id();
            Self {
                child: Some(child),
                exit_status: None,
                process_group,
                owns_process_group: true,
            }
        })
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut()?.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    pub fn wait_until(
        &mut self,
        deadline: Instant,
        mut should_cancel: impl FnMut() -> bool,
    ) -> Result<WaitOutcome, ChildProcessError> {
        loop {
            if let Some(status) = self.try_wait()? {
                self.terminate_group()?;
                return Ok(WaitOutcome::Exited(status));
            }
            if should_cancel() {
                return Ok(WaitOutcome::Cancelled);
            }
            if Instant::now() >= deadline {
                return Ok(WaitOutcome::TimedOut);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    #[cfg(target_os = "macos")]
    pub fn terminate(mut self) -> Result<(), ChildProcessError> {
        self.terminate_inner()
    }

    /// Stops the group while keeping ownership, so a caller blocked on the
    /// child's output can unblock and still tidy up afterwards.
    pub fn terminate_in_place(&mut self) -> Result<(), ChildProcessError> {
        self.terminate_inner()
    }

    fn terminate_inner(&mut self) -> Result<(), ChildProcessError> {
        if self.child.is_none() {
            return self.terminate_group();
        }
        let inspect_error = match self.try_wait() {
            Ok(Some(_status)) => {
                self.terminate_group()?;
                return Ok(());
            }
            Ok(None) => None,
            Err(ChildProcessError::Inspect(source)) => Some(source),
            Err(error) => return Err(error),
        };

        if let Err(group_error) = self.terminate_group() {
            if let Some(_status) = self.try_wait()? {
                return Err(group_error);
            }
            if let Err(source) = self.child_mut()?.kill() {
                if let Some(_status) = self.try_wait()? {
                    return Err(group_error);
                }
                return Err(ChildProcessError::TerminateProcess(source));
            }
            self.reap()?;
            return Err(group_error);
        }
        self.reap()?;

        if let Some(source) = inspect_error {
            return Err(ChildProcessError::Inspect(source));
        }
        Ok(())
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, ChildProcessError> {
        if self.child.is_none() {
            return Ok(self.exit_status);
        }
        if !self.child_exited()? {
            return Ok(None);
        }
        // The unreaped leader still holds the group id, so this kill cannot
        // reach a group that reused it.
        self.terminate_group()?;
        let status = self
            .child_mut()?
            .try_wait()
            .map_err(ChildProcessError::Inspect)?;
        if status.is_some() {
            self.child = None;
            self.exit_status = status;
        }
        Ok(status)
    }

    fn reap(&mut self) -> Result<(), ChildProcessError> {
        let status = self.child_mut()?.wait().map_err(ChildProcessError::Reap)?;
        self.child = None;
        self.exit_status = Some(status);
        Ok(())
    }

    /// Asks the group to stop with SIGTERM, so a child with a signal handler
    /// can tidy up. Reaping stays with a later wait or terminate; a group
    /// already gone counts as asked.
    pub fn request_termination(&self) -> Result<(), ChildProcessError> {
        if !self.owns_process_group {
            return Ok(());
        }
        self.signal_group(Signal::TERM)
    }

    fn terminate_group(&mut self) -> Result<(), ChildProcessError> {
        if !self.owns_process_group {
            return Ok(());
        }

        self.signal_group(Signal::KILL)?;
        self.owns_process_group = false;
        Ok(())
    }

    /// A group already gone counts as signalled.
    fn signal_group(&self, signal: Signal) -> Result<(), ChildProcessError> {
        match kill_process_group(self.group_pid()?, signal) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            // macOS answers EPERM, not ESRCH, once every member has exited.
            // It answers the same for a live member it may not signal, which
            // this cannot tell apart once the child itself has exited.
            #[cfg(target_os = "macos")]
            Err(Errno::PERM) if self.child_exited()? => Ok(()),
            Err(source) => Err(ChildProcessError::TerminateGroup(source.into())),
        }
    }

    /// Whether the child has exited, without reaping it, so its status stays
    /// for whoever waits.
    fn child_exited(&self) -> Result<bool, ChildProcessError> {
        use rustix::process::{WaitId, WaitIdOptions, waitid};

        let Some(child) = &self.child else {
            return Ok(true);
        };
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        waitid(WaitId::Pid(Pid::from_child(child)), options)
            .map(|status| status.is_some())
            .map_err(|source| ChildProcessError::Inspect(source.into()))
    }

    fn group_pid(&self) -> Result<Pid, ChildProcessError> {
        let raw_pid = i32::try_from(self.process_group)
            .map_err(|_error| io::Error::other("child process identifier is out of range"))
            .map_err(ChildProcessError::TerminateGroup)?;
        Pid::from_raw(raw_pid)
            .ok_or_else(|| io::Error::other("child process identifier is invalid"))
            .map_err(ChildProcessError::TerminateGroup)
    }

    fn child_mut(&mut self) -> Result<&mut Child, ChildProcessError> {
        self.child.as_mut().ok_or(ChildProcessError::MissingChild)
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if (self.child.is_some() || self.owns_process_group)
            && let Err(error) = self.terminate_inner()
        {
            tracing::error!(?error, "failed to clean up child process before drop");
        }
    }
}

pub enum WaitOutcome {
    Exited(ExitStatus),
    Cancelled,
    TimedOut,
}

#[derive(Debug, thiserror::Error)]
pub enum ChildProcessError {
    #[error("child process ownership ended before cleanup completed")]
    MissingChild,

    #[error("failed to inspect the child process")]
    Inspect(#[source] io::Error),

    #[error("failed to terminate the child process group")]
    TerminateGroup(#[source] io::Error),

    #[error("failed to terminate the child process")]
    TerminateProcess(#[source] io::Error),

    #[error("failed to reap the child process")]
    Reap(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::thread;
    use std::time::{Duration, Instant};

    use rustix::process::{WaitId, WaitIdOptions, test_kill_process, waitid};

    use super::super::POLL_INTERVAL;
    use super::*;

    const PATIENCE: Duration = Duration::from_secs(5);

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    fn spawn(script: &str) -> ChildProcess {
        ChildProcess::spawn(&mut shell(script)).expect("the shell should start")
    }

    // The shell leaves a child running and exits. Only a group signal reaches
    // that orphan, so a pass means the cleanup really killed the group.
    fn spawn_group() -> (ChildProcess, Pid) {
        let mut command = shell("sleep 300 & echo $!");
        command.stdout(Stdio::piped());
        let mut process = ChildProcess::spawn(&mut command).expect("the shell should start");

        let stdout = process.take_stdout().expect("stdout should be piped");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("the shell should report its child");
        let raw = line.trim().parse().expect("the report should be a pid");

        (
            process,
            Pid::from_raw(raw).expect("the pid should be valid"),
        )
    }

    // Processes exit and get reaped on their own schedule, so give the
    // condition a while to come true rather than checking once.
    fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(POLL_INTERVAL);
        }
        false
    }

    fn gone(pid: Pid) -> bool {
        eventually(|| test_kill_process(pid).is_err())
    }

    fn exited(process: &mut ChildProcess) -> bool {
        eventually(|| {
            process
                .try_wait()
                .expect("inspection should succeed")
                .is_some()
        })
    }

    #[test]
    fn dropping_a_process_kills_its_whole_group() {
        let (process, grandchild) = spawn_group();

        drop(process);

        assert!(gone(grandchild), "the grandchild outlived its group");
    }

    #[test]
    fn seeing_the_leader_exit_kills_the_rest_of_its_group() {
        let (mut process, grandchild) = spawn_group();

        assert!(exited(&mut process), "the shell should exit on its own");

        assert!(
            gone(grandchild),
            "the grandchild outlived its reaped leader"
        );
    }

    #[test]
    fn terminating_kills_the_whole_group() {
        let (mut process, grandchild) = spawn_group();

        process
            .terminate_in_place()
            .expect("cleanup should succeed");

        assert!(gone(grandchild), "the grandchild outlived its group");
    }

    #[test]
    fn cleanup_succeeds_once_the_child_has_exited() {
        let mut process = spawn("exit 0");
        assert!(exited(&mut process), "the shell should exit on its own");

        process
            .terminate_in_place()
            .expect("cleanup should succeed");
    }

    #[test]
    fn cleanup_succeeds_when_the_child_died_after_the_last_check() {
        let mut process = spawn("exit 0");
        let leader = process.group_pid().expect("the pid should be valid");
        waitid(
            WaitId::Pid(leader),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        )
        .expect("the shell should exit");

        process
            .terminate_group()
            .expect("a group of exited processes is already gone");
    }

    #[test]
    fn asking_an_exited_group_to_stop_keeps_its_status() {
        let mut process = spawn("exit 3");
        let leader = process.group_pid().expect("the pid should be valid");
        waitid(
            WaitId::Pid(leader),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        )
        .expect("the shell should exit");

        process
            .request_termination()
            .expect("a group of exited processes counts as asked");

        let outcome = process
            .wait_until(Instant::now() + PATIENCE, || false)
            .expect("waiting should succeed");
        let WaitOutcome::Exited(status) = outcome else {
            panic!("the shell should have exited");
        };
        assert_eq!(status.code(), Some(3));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn waiting_reports_the_exit_status() {
        let mut process = spawn("exit 3");

        let outcome = process
            .wait_until(Instant::now() + PATIENCE, || false)
            .expect("waiting should succeed");

        let WaitOutcome::Exited(status) = outcome else {
            panic!("the shell should have exited");
        };
        assert_eq!(status.code(), Some(3));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn waiting_gives_up_at_the_deadline() {
        let mut process = spawn("sleep 300");

        let outcome = process
            .wait_until(Instant::now() + Duration::from_millis(50), || false)
            .expect("waiting should succeed");

        assert!(matches!(outcome, WaitOutcome::TimedOut));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn waiting_stops_when_the_caller_cancels() {
        let mut process = spawn("sleep 300");

        let outcome = process
            .wait_until(Instant::now() + PATIENCE, || true)
            .expect("waiting should succeed");

        assert!(matches!(outcome, WaitOutcome::Cancelled));
    }
}
