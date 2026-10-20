//! Drives the daemon binary inside a sandbox with a bounded wait.

use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use aria_primitives::process::{ChildProcess, ChildProcessError, WaitOutcome};

use crate::sandbox::Sandbox;

/// Allows time for model loading in debug builds while bounding the wait.
const EXIT_DEADLINE: Duration = Duration::from_secs(45);
/// Client runs cover a full dictation round-trip, so they get a wider bound.
const CLIENT_EXIT_DEADLINE: Duration = Duration::from_secs(60);
/// Readiness waits on the same cold start as `EXIT_DEADLINE`.
const READY_DEADLINE: Duration = Duration::from_secs(45);
const DAEMON_SUBCOMMAND: &str = "daemon";

/// Runs the daemon and waits for it to exit on its own.
pub(crate) fn run_to_exit(binary: &Path, sandbox: &Sandbox) -> Result<Output, DaemonProcessError> {
    spawn(binary, &[DAEMON_SUBCOMMAND], sandbox)?.wait(EXIT_DEADLINE)
}

/// Starts the daemon and waits until it can serve. A daemon that exits before
/// becoming ready surfaces as `Exited` with its output for the caller to judge.
pub fn start_service(binary: &Path, sandbox: &Sandbox) -> Result<ServiceStart, DaemonProcessError> {
    let mut daemon = spawn(binary, &[DAEMON_SUBCOMMAND], sandbox)?;
    let log = sandbox.log_path();
    let ready = || daemon_log(&log).is_some_and(|written| crate::log::is_ready(&written));
    // Readiness is what ends the wait early, so it arrives as a cancel.
    match daemon
        .process
        .wait_until(Instant::now() + READY_DEADLINE, ready)
        .map_err(DaemonProcessError::Process)?
    {
        WaitOutcome::Cancelled => Ok(ServiceStart::Running(RunningDaemon { daemon })),
        WaitOutcome::Exited(status) => daemon.output(status).map(ServiceStart::Exited),
        WaitOutcome::TimedOut => Err(DaemonProcessError::NotReady {
            deadline: READY_DEADLINE,
            stderr: daemon.kill()?,
        }),
    }
}

pub enum ServiceStart {
    Running(RunningDaemon),
    Exited(Output),
}

impl ServiceStart {
    /// The running daemon, or a typed error when it exited during startup.
    /// For callers that need a live daemon rather than an outcome to judge.
    pub fn into_running(self) -> Result<RunningDaemon, DaemonProcessError> {
        match self {
            Self::Running(daemon) => Ok(daemon),
            Self::Exited(output) => Err(DaemonProcessError::ExitedDuringStartup {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
        }
    }
}

/// A ready daemon serving its socket. `stop` is the graceful path that
/// collects output; dropping without it kills and reaps as a last resort.
pub struct RunningDaemon {
    daemon: Spawned,
}

impl RunningDaemon {
    /// Asks the daemon to shut down with SIGTERM and collects its output.
    pub fn stop(self) -> Result<Output, DaemonProcessError> {
        self.daemon
            .process
            .request_termination()
            .map_err(DaemonProcessError::Process)?;
        self.daemon.wait(EXIT_DEADLINE)
    }
}

/// Runs the binary with `arguments` inside the sandbox and waits for it to
/// exit. Meant for client invocations against a running daemon.
pub(crate) fn run_client(
    binary: &Path,
    arguments: &[&str],
    sandbox: &Sandbox,
) -> Result<Output, DaemonProcessError> {
    spawn(binary, arguments, sandbox)?.wait(CLIENT_EXIT_DEADLINE)
}

/// A child whose output drains from the start, so a chatty one can never
/// fill a pipe and block while nobody waits on it.
struct Spawned {
    process: ChildProcess,
    stdout: JoinHandle<std::io::Result<Vec<u8>>>,
    stderr: JoinHandle<std::io::Result<Vec<u8>>>,
}

fn spawn(
    binary: &Path,
    arguments: &[&str],
    sandbox: &Sandbox,
) -> Result<Spawned, DaemonProcessError> {
    let mut command = Command::new(binary);
    sandbox.apply(&mut command);
    // A command that would prompt on a terminal reads end of input instead.
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = ChildProcess::spawn(&mut command).map_err(DaemonProcessError::Spawn)?;
    let stdout = process.take_stdout().expect("stdout is piped");
    let stderr = process.take_stderr().expect("stderr is piped");
    Ok(Spawned {
        process,
        stdout: thread::spawn(|| drain(stdout)),
        stderr: thread::spawn(|| drain(stderr)),
    })
}

impl Spawned {
    fn wait(mut self, deadline: Duration) -> Result<Output, DaemonProcessError> {
        match self
            .process
            .wait_until(Instant::now() + deadline, || false)
            .map_err(DaemonProcessError::Process)?
        {
            WaitOutcome::Exited(status) => self.output(status),
            WaitOutcome::TimedOut | WaitOutcome::Cancelled => {
                Err(DaemonProcessError::StillRunning {
                    deadline,
                    stderr: self.kill()?,
                })
            }
        }
    }

    fn output(self, status: ExitStatus) -> Result<Output, DaemonProcessError> {
        let (stdout, stderr) = self.join()?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Kills the group and returns what it wrote to stderr.
    fn kill(mut self) -> Result<String, DaemonProcessError> {
        self.process
            .terminate_in_place()
            .map_err(DaemonProcessError::Process)?;
        let (_, stderr) = self.join()?;
        Ok(String::from_utf8_lossy(&stderr).into_owned())
    }

    /// Blocks until nothing in the group holds the pipes.
    fn join(self) -> Result<(Vec<u8>, Vec<u8>), DaemonProcessError> {
        let stdout = self
            .stdout
            .join()
            .expect("stdout reader must not panic")
            .map_err(DaemonProcessError::CollectOutput)?;
        let stderr = self
            .stderr
            .join()
            .expect("stderr reader must not panic")
            .map_err(DaemonProcessError::CollectOutput)?;
        Ok((stdout, stderr))
    }
}

fn daemon_log(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn drain(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    pipe.read_to_end(&mut buffer)?;
    Ok(buffer)
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonProcessError {
    #[error("failed to spawn the daemon")]
    Spawn(#[source] std::io::Error),

    #[error("failed to supervise the daemon")]
    Process(#[source] ChildProcessError),

    #[error("failed to collect daemon output")]
    CollectOutput(#[source] std::io::Error),

    #[error("daemon still running after {deadline:?}; stderr:\n{stderr}")]
    StillRunning { deadline: Duration, stderr: String },

    #[error("daemon did not report readiness within {deadline:?}; stderr:\n{stderr}")]
    NotReady { deadline: Duration, stderr: String },

    #[error("daemon exited during startup with {status}; stderr:\n{stderr}")]
    ExitedDuringStartup { status: ExitStatus, stderr: String },
}
