//! A supervised child process that answers one JSON line per request.
//!
//! The program is handed to its interpreter on the command line rather than
//! read from disk, so the source cannot drift from the code that talks to it.
//! Replies arrive on their own thread, which is what lets a caller give up:
//! killing the child ends the read, so cancelling never waits for the work.
//!
//! The frames carry no meaning here. What a reply says, and which failures are
//! recoverable, belongs to whoever speaks the protocol on top.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::POLL_INTERVAL;
use super::child::ChildProcess;
use crate::file_inspection::{InspectedFile, InspectionError};
use crate::user_home::{self, UserHomeError};

// A reply past this is a broken child, not a long answer, and reading on
// would be unbounded memory.
const MAX_REPLY_BYTES: u64 = 64 * 1024;

/// What to run, and the name its log lines and threads carry.
pub struct Script<'a> {
    pub name: &'static str,
    pub interpreter: &'a Path,
    pub source: &'a str,
    pub argument: &'a str,
    /// Variables copied from the parent. The child also receives HOME and
    /// PYTHONSAFEPATH; the remaining environment is cleared.
    pub inherited_variables: &'a [&'a str],
}

pub struct Sidecar {
    name: &'static str,
    process: ChildProcess,
    requests: ChildStdin,
    replies: Receiver<io::Result<String>>,
    /// A stopped sidecar is never reused, even when stopping it failed.
    stopped: bool,
}

impl Sidecar {
    pub fn start(script: &Script<'_>) -> Result<Self, SidecarError> {
        let mut command = Command::new(script.interpreter);
        command
            .arg("-c")
            .arg(script.source)
            .arg(script.argument)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        restrict_environment(&mut command, script.inherited_variables)?;

        let mut process = ChildProcess::spawn(&mut command).map_err(SidecarError::Spawn)?;
        let (Some(requests), Some(stdout), Some(stderr)) = (
            process.take_stdin(),
            process.take_stdout(),
            process.take_stderr(),
        ) else {
            return Err(SidecarError::Pipes);
        };
        log_stderr(script.name, stderr);

        Ok(Self {
            name: script.name,
            process,
            requests,
            replies: read_replies(script.name, stdout),
            stopped: false,
        })
    }

    pub fn send(&mut self, request: &impl Serialize) -> Result<(), SidecarError> {
        let line = serde_json::to_string(request).map_err(SidecarError::Encode)?;
        writeln!(self.requests, "{line}")
            .and_then(|()| self.requests.flush())
            .map_err(SidecarError::Write)
    }

    /// Reads one reply, killing the child if the caller gives up first.
    pub fn receive<R: DeserializeOwned>(
        &mut self,
        should_cancel: &(impl Fn() -> bool + Sync),
    ) -> Result<R, SidecarError> {
        let line = self.read_line(should_cancel)?;
        serde_json::from_str(line.trim()).map_err(SidecarError::Decode)
    }

    /// Whether the child is still there to serve another request.
    pub fn is_running(&mut self) -> bool {
        !self.stopped && matches!(self.process.try_wait(), Ok(None))
    }

    fn read_line(
        &mut self,
        should_cancel: &(impl Fn() -> bool + Sync),
    ) -> Result<String, SidecarError> {
        loop {
            match self.replies.recv_timeout(POLL_INTERVAL) {
                Ok(reply) => return reply.map_err(SidecarError::Read),
                Err(RecvTimeoutError::Disconnected) => return Err(SidecarError::Closed),
                Err(RecvTimeoutError::Timeout) => {}
            }
            if should_cancel() {
                self.stop();
                return Err(SidecarError::Cancelled);
            }
        }
    }

    fn stop(&mut self) {
        self.stopped = true;
        if let Err(error) = self.process.terminate_in_place() {
            tracing::error!(?error, sidecar = self.name, "failed to stop the sidecar");
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("the sidecar interpreter cannot be used")]
    Interpreter(#[source] InspectionError),

    #[error("a sidecar needs HOME to start")]
    Home(#[source] UserHomeError),

    #[error("failed to start the sidecar process")]
    Spawn(#[source] io::Error),

    #[error("the sidecar was started without all three of its pipes")]
    Pipes,

    #[error("failed to encode a request")]
    Encode(#[source] serde_json::Error),

    #[error("failed to send a request")]
    Write(#[source] io::Error),

    #[error("failed to read a reply")]
    Read(#[source] io::Error),

    #[error("the sidecar sent a reply that could not be understood")]
    Decode(#[source] serde_json::Error),

    #[error("the sidecar stopped before replying")]
    Closed,

    #[error("the caller gave up waiting")]
    Cancelled,
}

/// Checks the interpreter the path resolves to, but keeps the configured path
/// to run: a virtual environment is only itself when entered through its own
/// `bin/python3`, and the canonical target is the bare interpreter.
pub fn validate_interpreter(path: PathBuf) -> Result<PathBuf, SidecarError> {
    InspectedFile::inspect(path.clone())
        .and_then(InspectedFile::require_executable)
        .and_then(InspectedFile::require_trusted)
        .map_err(SidecarError::Interpreter)?;
    Ok(path)
}

/// The child inherits nothing but `HOME` and the variables its caller named.
fn restrict_environment(
    command: &mut Command,
    inherited_variables: &[&str],
) -> Result<(), SidecarError> {
    let home = user_home::resolve().map_err(SidecarError::Home)?;
    // `python -c` puts the working directory on sys.path, so a file there
    // named like an imported package would shadow it.
    command
        .env_clear()
        .env("HOME", home)
        .env("PYTHONSAFEPATH", "1");
    for name in inherited_variables {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    Ok(())
}

/// Reads replies on their own thread so a blocked read never pins the caller.
/// The thread ends when the child closes its output, which killing it does.
fn read_replies(name: &'static str, stdout: ChildStdout) -> Receiver<io::Result<String>> {
    let (sender, receiver) = mpsc::channel();
    spawn_helper(format!("{name}-replies"), move || {
        let mut reader = BufReader::new(stdout);
        while let Some(reply) = next_reply(&mut reader) {
            let failed = reply.is_err();
            if sender.send(reply).is_err() || failed {
                return;
            }
        }
    });
    receiver
}

/// One reply, or `None` once the child closes its output. Blank lines carry no
/// frame, so they are skipped rather than delivered.
fn next_reply(reader: &mut BufReader<ChildStdout>) -> Option<io::Result<String>> {
    loop {
        let mut line = String::new();
        match reader.by_ref().take(MAX_REPLY_BYTES).read_line(&mut line) {
            Ok(0) => return None,
            Ok(size) if size as u64 == MAX_REPLY_BYTES => {
                return Some(Err(io::Error::other(
                    "sidecar reply exceeded the maximum frame size",
                )));
            }
            Ok(_size) if line.trim().is_empty() => {}
            Ok(_size) => return Some(Ok(line)),
            Err(error) => return Some(Err(error)),
        }
    }
}

/// The child's own output is diagnostics, not protocol, so it joins the daemon log.
fn log_stderr(name: &'static str, stderr: ChildStderr) {
    spawn_helper(format!("{name}-stderr"), move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                tracing::warn!(sidecar = name, "{line}");
            }
        }
    });
}

/// Both helper threads are best-effort. One that cannot start is logged, and
/// the caller meets the missing output as a sidecar that stopped replying.
fn spawn_helper(name: String, body: impl FnOnce() + Send + 'static) {
    thread::Builder::new()
        .name(name.clone())
        .spawn(body)
        .map(|_detached| ())
        .unwrap_or_else(|error| {
            tracing::error!(?error, thread = name, "failed to start a sidecar thread");
        });
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[derive(Serialize)]
    struct Ping {
        op: &'static str,
    }

    #[derive(serde::Deserialize)]
    struct Pong {
        text: String,
    }

    // /bin/sh takes its script from -c the same way a Python child does, so
    // the transport can be exercised without Python or a model.
    fn stub(source: &str) -> Sidecar {
        Sidecar::start(&Script {
            name: "stub",
            interpreter: Path::new("/bin/sh"),
            source,
            argument: "unused",
            inherited_variables: &[],
        })
        .expect("the stub should start")
    }

    fn ping(sidecar: &mut Sidecar) -> Result<Pong, SidecarError> {
        sidecar.send(&Ping { op: "ping" })?;
        sidecar.receive(&|| false)
    }

    // The child exits on its own schedule, so poll instead of checking once.
    fn stops_running(sidecar: &mut Sidecar) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !sidecar.is_running() {
                return true;
            }
            thread::sleep(POLL_INTERVAL);
        }
        false
    }

    #[test]
    fn a_reply_is_read_back_from_the_child() {
        let mut sidecar = stub(r#"read line; echo '{"text":"hello there"}'"#);

        let pong = ping(&mut sidecar).expect("the stub should answer");

        assert_eq!(pong.text, "hello there");
    }

    #[test]
    fn the_child_is_told_to_keep_cwd_off_the_python_path() {
        let mut sidecar = stub(r#"read line; echo "{\"text\":\"$PYTHONSAFEPATH\"}""#);

        let pong = ping(&mut sidecar).expect("the stub should answer");

        assert_eq!(pong.text, "1");
    }

    #[test]
    fn blank_lines_are_not_replies() {
        let mut sidecar = stub(r#"read line; echo; echo; echo '{"text":"after the blanks"}'"#);

        let pong = ping(&mut sidecar).expect("the stub should answer past the blank lines");

        assert_eq!(pong.text, "after the blanks");
    }

    #[test]
    fn a_child_that_keeps_serving_is_still_running() {
        let mut sidecar = stub(r#"while read line; do echo '{"text":"again"}'; done"#);

        ping(&mut sidecar).expect("the stub should answer");

        assert!(sidecar.is_running());
    }

    #[test]
    fn a_cancelled_wait_stops_the_child_instead_of_blocking() {
        let mut sidecar = stub("sleep 600");

        sidecar.send(&Ping { op: "ping" }).expect("the send lands");
        let outcome = sidecar.receive::<Pong>(&|| true);

        assert!(matches!(outcome, Err(SidecarError::Cancelled)));
        assert!(
            !sidecar.is_running(),
            "a cancelled sidecar was killed and must not be reused"
        );
    }

    #[test]
    fn a_child_that_exits_is_reported_rather_than_waited_on() {
        let mut sidecar = stub("exit 0");

        let outcome = ping(&mut sidecar);

        assert!(
            matches!(
                outcome,
                Err(SidecarError::Closed) | Err(SidecarError::Write(_))
            ),
            "a dead sidecar must surface as an error"
        );
        assert!(
            stops_running(&mut sidecar),
            "a dead sidecar must stop reporting itself as running"
        );
        assert!(
            !sidecar.is_running(),
            "a dead sidecar must stay dead on a second look"
        );
    }

    #[test]
    fn a_reply_that_is_not_json_is_a_loud_error() {
        let mut sidecar = stub("read line; echo 'not json'");

        let outcome = ping(&mut sidecar);

        assert!(matches!(outcome, Err(SidecarError::Decode(_))));
    }

    #[test]
    fn an_oversized_reply_is_refused_rather_than_buffered() {
        let mut sidecar = stub(&format!(
            "read line; head -c {} /dev/zero | tr '\\0' 'a'; echo",
            MAX_REPLY_BYTES + 1
        ));

        let outcome = ping(&mut sidecar);

        assert!(matches!(outcome, Err(SidecarError::Read(_))));
    }

    #[test]
    fn a_relative_interpreter_is_rejected() {
        let error = validate_interpreter(PathBuf::from("relative/python3"))
            .expect_err("a relative interpreter must be rejected");

        assert!(matches!(
            error,
            SidecarError::Interpreter(InspectionError::NotAbsolute { .. })
        ));
    }

    #[test]
    fn a_missing_interpreter_is_rejected() {
        let error = validate_interpreter(PathBuf::from("/nonexistent/python3"))
            .expect_err("a missing interpreter must be rejected");

        assert!(matches!(
            error,
            SidecarError::Interpreter(InspectionError::Inspect { .. })
        ));
    }

    #[test]
    fn a_file_that_cannot_be_run_is_not_an_interpreter() {
        let error = validate_interpreter(PathBuf::from("/etc/hosts"))
            .expect_err("a data file must be rejected");

        assert!(matches!(
            error,
            SidecarError::Interpreter(InspectionError::NotExecutable { .. })
        ));
    }
}
