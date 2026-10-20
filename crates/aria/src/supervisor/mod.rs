//! The daemon as a login-session service: launchd on macOS, systemd on Linux.
//! `aria service` is the CLI verb over it.

use std::ffi::{OsStr, OsString};
use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};
use std::{fs, io};

use crate::ipc::{ClientError, ConnectError, Liveness, ProbeError, probe_daemon};
use crate::primitives::cancel::CancelToken;
use crate::primitives::local_socket::owner_pid;
use crate::primitives::process::{self, Bounds, Capture, RunError};
use crate::primitives::user_home::{self, UserHomeError};
use crate::runtime::{PreparedRuntimePaths, RUNTIME_DIR_ENV, RuntimeError};

#[cfg(target_os = "macos")]
mod launchd;
#[cfg(target_os = "linux")]
mod systemd;

#[cfg(target_os = "macos")]
use launchd as platform;
#[cfg(target_os = "linux")]
use systemd as platform;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("no service supervisor exists for this target");

/// How long a freshly started daemon gets to bind its socket before
/// `install` and `start` report it as stopped. Covers the login-shell PATH
/// lookup a daemon started by Aria.app's agent runs first.
const BIND_TIMEOUT: Duration = Duration::from_secs(10);
const BIND_POLL: Duration = Duration::from_millis(100);
/// Covers the daemon's shutdown grace.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Unit files select executable code, so they are readable but not writable
/// by anyone else.
const UNIT_FILE_MODE: u32 = 0o644;

pub struct Supervisor {
    paths: PreparedRuntimePaths,
    directory: PathBuf,
    unit: PathBuf,
    spec: UnitSpec,
}

/// What the supervisor needs to run the daemon as this user.
struct UnitSpec {
    exe: PathBuf,
    home: PathBuf,
    /// Variables the installing shell hands to the daemon, in unit order.
    environment: Vec<(&'static str, OsString)>,
}

impl Supervisor {
    /// Reads the environment once: the binary, home, PATH, and runtime dir.
    /// Prepares the runtime directory like every other daemon client.
    pub fn open() -> Result<Self, SupervisorError> {
        // macOS keys the Accessibility grant to the real binary, so a unit
        // naming a symlink on PATH would run an identity the grant misses.
        let exe = std::env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(SupervisorError::CurrentExe)?;
        let home = user_home::resolve().map_err(SupervisorError::Home)?;
        let paths = PreparedRuntimePaths::for_current_user().map_err(SupervisorError::Runtime)?;
        let environment = [("PATH", std::env::var_os("PATH"))]
            .into_iter()
            .chain([(RUNTIME_DIR_ENV, std::env::var_os(RUNTIME_DIR_ENV))])
            .filter_map(|(name, value)| Some((name, value?)))
            .collect();
        let spec = UnitSpec {
            exe,
            home,
            environment,
        };
        spec.validate()?;
        let directory = platform::unit_directory(&spec.home);
        Ok(Self {
            paths,
            unit: directory.join(platform::UNIT_FILE),
            directory,
            spec,
        })
    }

    /// Writes the unit, loads it, and lets the supervisor start the daemon.
    /// Rerunning replaces the unit and restarts. A job Aria.app registered
    /// under the same label is restarted instead, so one supervisor owns the
    /// daemon. Refuses while another process owns the socket; a job unloaded
    /// on that path stays unloaded until the next login or the next install.
    pub fn install(&self) -> Result<Status, SupervisorError> {
        if platform::app_owns_job(&self.unit)? {
            return self.restart();
        }
        platform::unload()?;
        self.refuse_foreign_owner()?;
        self.write_unit()?;
        platform::load(&self.unit)?;
        self.await_bind()
    }

    /// Stops the daemon and removes the unit. Both steps tolerate absence.
    /// Leaves a job Aria.app registered loaded; only this unit file goes.
    pub fn uninstall(&self) -> Result<Status, SupervisorError> {
        if !platform::app_owns_job(&self.unit)? {
            platform::unload()?;
        }
        match fs::remove_file(&self.unit) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(SupervisorError::RemoveUnit {
                    path: self.unit.clone(),
                    source,
                });
            }
        }
        self.status()
    }

    /// Starts the registered job. A job that already has its process is left
    /// alone; any other socket owner is refused, since the supervisor would
    /// otherwise respawn a daemon that keeps losing the socket.
    pub fn start(&self) -> Result<Status, SupervisorError> {
        match platform::job()? {
            Job::Absent => return Err(SupervisorError::NotInstalled),
            Job::Running(_) => return self.status(),
            Job::Idle => {}
        }
        self.refuse_foreign_owner()?;
        platform::start()?;
        self.await_bind()
    }

    /// Stops the registered job's daemon and waits for it to exit.
    /// Idempotent: an idle job is left as is.
    pub fn stop(&self) -> Result<Status, SupervisorError> {
        match platform::job()? {
            Job::Absent => return Err(SupervisorError::NotInstalled),
            Job::Idle => return self.status(),
            Job::Running(_) => platform::stop()?,
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        while matches!(platform::job()?, Job::Running(_)) {
            if Instant::now() >= deadline {
                return Err(SupervisorError::StopTimeout {
                    timeout: STOP_TIMEOUT,
                });
            }
            thread::sleep(BIND_POLL);
        }
        self.status()
    }

    fn restart(&self) -> Result<Status, SupervisorError> {
        self.stop()?;
        self.start()
    }

    pub fn status(&self) -> Result<Status, SupervisorError> {
        Ok(Status {
            unit: self.unit.clone(),
            app_agent: platform::app_owns_job(&self.unit)?,
            job: platform::job()?,
            socket: self.paths.socket_path().to_path_buf(),
            daemon: self.daemon()?,
        })
    }

    fn refuse_foreign_owner(&self) -> Result<(), SupervisorError> {
        match self.daemon()? {
            DaemonState::NotRunning => Ok(()),
            DaemonState::Ready { pid } | DaemonState::Starting { pid } => {
                Err(SupervisorError::SocketOwned {
                    socket: self.paths.socket_path().to_path_buf(),
                    pid,
                })
            }
        }
    }

    /// The supervisor starts the daemon asynchronously; the socket appears
    /// once the daemon binds, well before models load.
    fn await_bind(&self) -> Result<Status, SupervisorError> {
        let deadline = Instant::now() + BIND_TIMEOUT;
        loop {
            if self.daemon()? != DaemonState::NotRunning || Instant::now() >= deadline {
                return self.status();
            }
            thread::sleep(BIND_POLL);
        }
    }

    fn daemon(&self) -> Result<DaemonState, SupervisorError> {
        let pid = || owner_pid(self.paths.lock_path());
        match probe_daemon(self.paths.socket_path()) {
            Ok(Liveness::Ready(_)) => Ok(DaemonState::Ready { pid: pid() }),
            Ok(Liveness::Starting(_)) => Ok(DaemonState::Starting { pid: pid() }),
            Ok(Liveness::NotRunning(_)) => Ok(DaemonState::NotRunning),
            Err(ProbeError::Connect(error)) => Err(SupervisorError::Connect(error)),
            Err(ProbeError::Readiness(error)) => Err(SupervisorError::Readiness(error)),
        }
    }

    /// Lands the unit atomically: a crash mid-write leaves the old unit, and a
    /// symlink at the unit path is replaced rather than followed.
    fn write_unit(&self) -> Result<(), SupervisorError> {
        let error = |action: &'static str| {
            let path = self.unit.clone();
            move |source: io::Error| SupervisorError::WriteUnit {
                action,
                path,
                source,
            }
        };
        fs::create_dir_all(&self.directory).map_err(error("create the directory of"))?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(error("create"))?;
        temporary
            .as_file()
            .set_permissions(Permissions::from_mode(UNIT_FILE_MODE))
            .map_err(error("set permissions on"))?;
        temporary
            .write_all(platform::render(&self.spec).as_bytes())
            .map_err(error("write"))?;
        temporary
            .persist(&self.unit)
            .map_err(|failure| error("replace")(failure.error))?;
        Ok(())
    }
}

impl UnitSpec {
    /// A control character would end a unit directive or break the plist.
    fn validate(&self) -> Result<(), SupervisorError> {
        let values = [("executable", self.exe.as_os_str())].into_iter().chain(
            self.environment
                .iter()
                .map(|(name, value)| (*name, value.as_os_str())),
        );
        for (name, value) in values {
            if value.to_string_lossy().chars().any(char::is_control) {
                return Err(SupervisorError::ControlCharacter { name });
            }
        }
        Ok(())
    }
}

pub struct Status {
    pub unit: PathBuf,
    /// Aria.app's login agent holds the job, not `unit`.
    pub app_agent: bool,
    pub job: Job,
    pub socket: PathBuf,
    pub daemon: DaemonState,
}

/// Removes the Python-era wake agent when one is registered.
pub(crate) fn remove_legacy_wake_agent(home: &Path) -> Result<Option<PathBuf>, SupervisorError> {
    platform::remove_legacy_wake_agent(home)
}

/// What the supervisor knows about its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Not registered.
    Absent,
    /// Registered, no process.
    Idle,
    Running(u32),
}

/// What the socket says, with the pid its owner recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Ready { pid: Option<u32> },
    Starting { pid: Option<u32> },
    NotRunning,
}

fn owner(pid: Option<u32>) -> String {
    pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default()
}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const KEPT_OUTPUT_BYTES: usize = 16 * 1024;

/// Runs a supervisor command to completion and returns its stdout. A
/// non-zero exit carries the command's stderr.
fn run_command(mut command: Command) -> Result<String, SupervisorError> {
    let name = describe(&command);
    let bounds = Bounds {
        timeout: COMMAND_TIMEOUT,
        stdout: Capture::KeepHead(KEPT_OUTPUT_BYTES),
        stderr: Capture::KeepHead(KEPT_OUTPUT_BYTES),
    };
    let output =
        process::run(&mut command, b"", bounds, &CancelToken::never()).map_err(|source| {
            SupervisorError::Run {
                command: name.clone(),
                source,
            }
        })?;
    if !output.status.success() {
        return Err(SupervisorError::Command {
            command: name,
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The program and its first argument, for error text: `launchctl bootout`.
fn describe(command: &Command) -> String {
    let program = Path::new(command.get_program())
        .file_name()
        .unwrap_or_else(|| command.get_program())
        .to_string_lossy();
    match command.get_args().next().map(OsStr::to_string_lossy) {
        Some(verb) => format!("{program} {verb}"),
        None => program.into_owned(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("failed to locate the running aria binary")]
    CurrentExe(#[source] std::io::Error),

    #[error(transparent)]
    Home(UserHomeError),

    #[error(transparent)]
    Runtime(RuntimeError),

    #[error("the {name} contains a control character, which no unit file can carry")]
    ControlCharacter { name: &'static str },

    #[error("another daemon owns {}{}; stop it first", socket.display(), owner(*pid))]
    SocketOwned { socket: PathBuf, pid: Option<u32> },

    #[error("failed to {action} {}", path.display())]
    WriteUnit {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to remove {}", path.display())]
    RemoveUnit {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("the daemon is not registered; run `aria service install`")]
    NotInstalled,

    #[error("the registered daemon did not unload within {}s", timeout.as_secs())]
    UnloadTimeout { timeout: Duration },

    #[error("the registered daemon did not stop within {}s", timeout.as_secs())]
    StopTimeout { timeout: Duration },

    #[error(transparent)]
    Connect(ConnectError),

    #[error("the daemon answered but did not report readiness")]
    Readiness(#[source] ClientError),

    #[error("failed to run {command}")]
    Run {
        command: String,
        #[source]
        source: RunError,
    },

    #[error("{command} exited with {status}{}", stderr_suffix(stderr))]
    Command {
        command: String,
        status: ExitStatus,
        stderr: String,
    },
}

fn stderr_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(exe: &str, environment: &[(&'static str, &str)]) -> UnitSpec {
        UnitSpec {
            exe: PathBuf::from(exe),
            home: PathBuf::from("/Users/tester"),
            environment: environment
                .iter()
                .map(|(name, value)| (*name, OsString::from(value)))
                .collect(),
        }
    }

    #[test]
    fn a_control_character_in_any_unit_value_is_refused() {
        assert!(spec("/opt/aria", &[("PATH", "/bin")]).validate().is_ok());
        assert!(matches!(
            spec("/opt/ar\nia", &[]).validate(),
            Err(SupervisorError::ControlCharacter { name: "executable" })
        ));
        assert!(matches!(
            spec("/opt/aria", &[("PATH", "/bin\n[Service]")]).validate(),
            Err(SupervisorError::ControlCharacter { name: "PATH" })
        ));
    }

    #[test]
    fn a_failing_command_reports_its_status_and_stderr() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo out; echo oops >&2; exit 3"]);

        let error = run_command(command).expect_err("exit 3 should fail");

        let SupervisorError::Command {
            command,
            status,
            stderr,
        } = error
        else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(command, "sh -c");
        assert_eq!(status.code(), Some(3));
        assert_eq!(stderr, "oops");
    }

    #[test]
    fn a_successful_command_returns_its_stdout() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo hello"]);

        assert_eq!(
            run_command(command).expect("exit 0 should succeed"),
            "hello\n"
        );
    }

    #[test]
    fn stderr_is_drained_while_waiting() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "yes error | head -c 200000 >&2; exit 1"]);

        let error = run_command(command).expect_err("exit 1 should fail");

        assert!(matches!(error, SupervisorError::Command { .. }), "{error}");
    }

    #[test]
    fn describe_names_the_program_and_its_verb() {
        let mut command = Command::new("/bin/launchctl");
        command.args(["bootout", "gui/501/x"]);
        assert_eq!(describe(&command), "launchctl bootout");
        assert_eq!(describe(&Command::new("systemctl")), "systemctl");
    }
}
