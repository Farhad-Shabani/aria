//! One agent run: spawn, feed the prompt on stdin, collect what came back.

use std::process::{Command, ExitStatus};
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::process::{self, Bounds, Capture, RunError};

use super::AgentError;

/// How much of a failed run's stderr `failure` quotes. Raw stdout is never
/// quoted: it may hold the reply.
const STDERR_QUOTE_BYTES: usize = 1_000;

/// How much stderr is kept at all: diagnostics are only ever quoted from
/// the tail, so the rest is dropped as it arrives.
const STDERR_KEEP_BYTES: usize = 64 * 1024;

/// Caps retained stdout; exceeding it requests termination of the child.
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// The status is the agent's; whether the output counts as a reply is for
/// its parser to say.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Output {
    /// A run that died before writing its reply explains itself on stderr;
    /// one that wrote it is explained by what it wrote.
    pub(crate) fn reply<T>(&self, parsed: Result<T, AgentError>) -> Result<T, AgentError> {
        match parsed {
            Err(AgentError::Malformed(_)) if !self.status.success() => Err(self.failure()),
            parsed => parsed,
        }
    }

    /// The failure a run reports when its output holds no better reason.
    fn failure(&self) -> AgentError {
        let stderr = self.stderr.trim();
        let start = stderr.len().saturating_sub(STDERR_QUOTE_BYTES);
        let quoted = match stderr.get(start..) {
            Some(tail) if !tail.is_empty() => tail,
            _ => "no diagnostics",
        };
        AgentError::Failed(format!("exited with {}: {quoted}", self.status))
    }
}

/// Hands each stdout line to `on_line` as it lands. Bytes that are not UTF-8
/// reach `on_line` and the output as replacement characters.
pub(crate) fn run_cli(
    command: &mut Command,
    stdin_text: &str,
    on_line: impl FnMut(&str) + Send,
    cancel: &CancelToken,
    timeout: Duration,
) -> Result<Output, AgentError> {
    let binary = command.get_program().to_owned();
    let bounds = Bounds {
        timeout,
        stdout: Capture::FailAbove(MAX_OUTPUT_BYTES),
        stderr: Capture::KeepTail(STDERR_KEEP_BYTES),
    };
    let mut lines = Lines {
        pending: Vec::new(),
        on_line,
    };
    let output =
        process::run_with_listener(command, stdin_text.as_bytes(), bounds, cancel, |bytes| {
            lines.push(bytes);
        })
        .map_err(|error| match error {
            RunError::Spawn(source) if source.kind() == std::io::ErrorKind::NotFound => {
                AgentError::Missing {
                    binary: binary.into(),
                }
            }
            RunError::Spawn(source) => AgentError::Spawn(source),
            RunError::Write(source) => AgentError::Input(source),
            RunError::Read(source) => AgentError::Io(source),
            RunError::Wait(source) => AgentError::Wait(source),
            RunError::TimedOut(timeout) => AgentError::Timeout(timeout),
            RunError::Cancelled => AgentError::Cancelled,
            RunError::OutputTooLarge { limit, .. } => AgentError::OutputTooLarge(limit),
        })?;
    lines.finish();
    Ok(Output {
        status: output.status,
        stdout: text(output.stdout),
        stderr: text(output.stderr),
    })
}

/// Converts without copying unless the bytes are not UTF-8, since stdout can
/// run to several megabytes.
fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// `pending` never outgrows the stdout cap, since `run` forwards no more than
/// the cap.
struct Lines<F> {
    pending: Vec<u8>,
    on_line: F,
}

impl<F: FnMut(&str)> Lines<F> {
    fn push(&mut self, mut bytes: &[u8]) {
        while let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
            self.pending.extend_from_slice(&bytes[..end]);
            self.deliver();
            bytes = &bytes[end + 1..];
        }
        self.pending.extend_from_slice(bytes);
    }

    fn finish(mut self) {
        if !self.pending.is_empty() {
            self.deliver();
        }
    }

    fn deliver(&mut self) {
        (self.on_line)(String::from_utf8_lossy(&self.pending).trim_end_matches('\r'));
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Instant;

    use super::*;

    const PATIENCE: Duration = Duration::from_secs(5);

    #[test]
    fn a_missing_binary_is_named() {
        let mut command = Command::new("/nonexistent/aria-agent");
        let error = run_cli(&mut command, "hi", |_| {}, &CancelToken::never(), PATIENCE)
            .expect_err("the spawn should fail");

        assert!(matches!(error, AgentError::Missing { .. }), "{error:?}");
    }

    #[test]
    fn lines_arrive_in_order_while_the_child_runs() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo one; echo two; printf tail"]);
        let mut seen = Vec::new();

        let output = run_cli(
            &mut command,
            "",
            |line| seen.push(line.to_owned()),
            &CancelToken::never(),
            PATIENCE,
        )
        .expect("the run should end");

        assert_eq!(seen, ["one", "two", "tail"]);
        assert_eq!(output.stdout, "one\ntwo\ntail");
    }

    #[test]
    fn a_child_that_writes_past_the_cap_is_refused_and_stopped() {
        let mut command = Command::new("sh");
        // `yes` would run forever: the overflow, not the deadline, ends it.
        command.args(["-c", "yes"]);
        let started = Instant::now();

        let error = run_cli(&mut command, "", |_| {}, &CancelToken::never(), PATIENCE)
            .expect_err("the output is too large");

        assert!(matches!(error, AgentError::OutputTooLarge(_)), "{error:?}");
        assert!(started.elapsed() < PATIENCE);
    }

    #[test]
    fn a_line_cut_by_the_cap_never_reaches_the_listener() {
        let mut command = Command::new("sh");
        let bytes = MAX_OUTPUT_BYTES + 1;
        command.args(["-c", &format!("head -c {bytes} /dev/zero")]);
        let mut lines = 0;

        let error = run_cli(
            &mut command,
            "",
            |_| lines += 1,
            &CancelToken::never(),
            PATIENCE,
        )
        .expect_err("the output is too large");

        assert!(matches!(error, AgentError::OutputTooLarge(_)), "{error:?}");
        assert_eq!(lines, 0);
    }

    #[test]
    fn line_endings_and_bad_bytes_do_not_reach_the_listener() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'a\\r\\nb\\377\\n'"]);
        let mut seen = Vec::new();

        run_cli(
            &mut command,
            "",
            |line: &str| seen.push(line.to_owned()),
            &CancelToken::never(),
            PATIENCE,
        )
        .expect("the run should end");

        assert_eq!(seen, ["a", "b\u{fffd}"]);
    }

    #[test]
    fn only_the_tail_of_stderr_is_kept() {
        let mut command = Command::new("sh");
        command.args(["-c", "yes | head -c 200000 >&2; printf end >&2; exit 1"]);

        let output = run_cli(&mut command, "", |_| {}, &CancelToken::never(), PATIENCE)
            .expect("the run should end");

        assert!(output.stderr.len() <= STDERR_KEEP_BYTES);
        assert!(output.stderr.ends_with("end"));
    }

    #[test]
    fn a_failing_child_hands_back_its_status_and_stderr() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo secret; echo boom >&2; exit 3"]);
        let output = run_cli(&mut command, "", |_| {}, &CancelToken::never(), PATIENCE)
            .expect("the run should end");

        assert!(!output.status.success());
        let message = output.failure().to_string();
        assert!(message.contains("boom"), "{message}");
        assert!(
            !message.contains("secret"),
            "stdout must stay out: {message}"
        );
    }

    #[test]
    fn a_slow_child_is_stopped_at_the_deadline() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();
        let error = run_cli(
            &mut command,
            "",
            |_| {},
            &CancelToken::never(),
            Duration::from_millis(200),
        )
        .expect_err("the child should time out");

        assert!(matches!(error, AgentError::Timeout(_)), "{error:?}");
        assert!(started.elapsed() < PATIENCE);
    }

    #[test]
    fn a_cancelled_run_stops_the_child_and_returns() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let cancel = CancelToken::new();
        let canceller = {
            let cancel = cancel.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                cancel.cancel();
            })
        };
        let started = Instant::now();

        let error =
            run_cli(&mut command, "", |_| {}, &cancel, PATIENCE).expect_err("the run is cancelled");

        canceller.join().expect("the canceller finishes");
        assert!(matches!(error, AgentError::Cancelled), "{error:?}");
        assert!(started.elapsed() < PATIENCE);
    }
}
