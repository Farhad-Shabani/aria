//! One agent run: spawn, feed the prompt on stdin, collect what came back.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::process::{ChildProcess, WaitOutcome};

use super::AgentError;

/// How much of a failed run's stderr `failure` quotes. Raw stdout is never
/// quoted: it may hold the reply.
const STDERR_QUOTE_BYTES: usize = 1_000;

/// How much stderr is kept at all: diagnostics are only ever quoted from
/// the tail, so the rest is dropped as it arrives.
const STDERR_KEEP_BYTES: usize = 64 * 1024;

/// Caps retained stdout; exceeding it requests termination of the child.
const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;

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

pub(crate) fn run_cli(
    command: &mut Command,
    stdin_text: &str,
    on_line: impl FnMut(&str) + Send,
    cancel: &CancelToken,
    timeout: Duration,
) -> Result<Output, AgentError> {
    let deadline = Instant::now() + timeout;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildProcess::spawn(command).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AgentError::Missing {
                binary: command.get_program().into(),
            }
        } else {
            AgentError::Spawn(error)
        }
    })?;
    let mut stdin = child.take_stdin().ok_or_else(|| not_piped("stdin"))?;
    let stdout = child.take_stdout().ok_or_else(|| not_piped("stdout"))?;
    let stderr = child.take_stderr().ok_or_else(|| not_piped("stderr"))?;

    // Set by the stdout reader once the child has written past the cap; the
    // wait then stops the child instead of letting it run to its deadline.
    let overflowed = AtomicBool::new(false);
    let (waited, stdout, stderr) = thread::scope(|scope| {
        // The pipes are read while the child runs, so a long reply can never
        // fill one and stall it.
        let stdout = scope.spawn(|| read_lines(stdout, on_line, &overflowed));
        let stderr = scope.spawn(move || read_tail(stderr));
        let writer = scope.spawn(move || stdin.write_all(stdin_text.as_bytes()));
        let waited = child.wait_until(deadline, || {
            cancel.is_cancelled() || overflowed.load(Ordering::Relaxed)
        });
        if !matches!(waited, Ok(WaitOutcome::Exited(_))) {
            // Closing the child's pipes is what lets the readers return.
            if let Err(error) = child.terminate_in_place() {
                tracing::warn!(%error, "failed to stop the agent");
            }
        }
        let _ = writer.join();
        let stdout = stdout.join().unwrap_or_else(|_panic| Ok(String::new()));
        let stderr = stderr.join().unwrap_or_else(|_panic| Ok(String::new()));
        (waited, stdout, stderr)
    });

    let stdout = stdout?;
    match waited.map_err(AgentError::Wait)? {
        // The stop above is what ended it, so its status is not the story.
        _ if overflowed.load(Ordering::Relaxed) => {
            Err(AgentError::OutputTooLarge(MAX_OUTPUT_BYTES))
        }
        WaitOutcome::Exited(status) => Ok(Output {
            status,
            stdout,
            stderr: stderr.map_err(AgentError::Io)?,
        }),
        WaitOutcome::TimedOut => Err(AgentError::Timeout(timeout)),
        WaitOutcome::Cancelled => Err(AgentError::Cancelled),
    }
}

/// Hands each line over as it lands and keeps the whole text, up to
/// `MAX_OUTPUT_BYTES`. One byte past that flags the overflow and drains the
/// pipe until the child is stopped, so it never blocks on a full pipe. Bytes
/// that are not UTF-8 are kept as replacement characters rather than ending
/// the read early, which would leave the child blocked the same way.
fn read_lines(
    pipe: impl Read,
    mut on_line: impl FnMut(&str),
    overflowed: &AtomicBool,
) -> Result<String, AgentError> {
    let mut reader = BufReader::new(pipe);
    let mut text = String::new();
    let mut remaining = MAX_OUTPUT_BYTES + 1;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take(remaining)
            .read_until(b'\n', &mut line)
            .map_err(AgentError::Io)?;
        if read == 0 {
            return Ok(text);
        }
        remaining -= read as u64;
        if remaining == 0 {
            overflowed.store(true, Ordering::Relaxed);
            io::copy(&mut reader, &mut io::sink()).map_err(AgentError::Io)?;
            return Err(AgentError::OutputTooLarge(MAX_OUTPUT_BYTES));
        }
        let line = String::from_utf8_lossy(&line);
        on_line(line.trim_end_matches(['\n', '\r']));
        text.push_str(&line);
    }
}

/// Keeps the last `STDERR_KEEP_BYTES` of a pipe and drops the rest as it
/// arrives, so a talkative agent never fails its run over diagnostics.
fn read_tail(pipe: impl Read) -> io::Result<String> {
    let mut reader = BufReader::new(pipe);
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(String::from_utf8_lossy(&kept).into_owned());
        }
        kept.extend_from_slice(&chunk[..read]);
        if kept.len() > STDERR_KEEP_BYTES {
            kept.drain(..kept.len() - STDERR_KEEP_BYTES);
        }
    }
}

fn not_piped(stream: &str) -> AgentError {
    AgentError::Spawn(std::io::Error::other(format!(
        "agent {stream} was not piped"
    )))
}

#[cfg(test)]
mod tests {
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
    fn stdin_reaches_the_child_and_stdout_comes_back() {
        let mut command = Command::new("cat");
        let output = run_cli(
            &mut command,
            "echoed",
            |_| {},
            &CancelToken::never(),
            PATIENCE,
        )
        .expect("cat should echo");

        assert!(output.status.success());
        assert_eq!(output.stdout, "echoed");
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
    fn output_exactly_at_the_cap_is_kept() {
        let mut command = Command::new("sh");
        command.args(["-c", &format!("yes | head -c {MAX_OUTPUT_BYTES}")]);

        let output = run_cli(&mut command, "", |_| {}, &CancelToken::never(), PATIENCE)
            .expect("the output fits");

        assert_eq!(output.stdout.len() as u64, MAX_OUTPUT_BYTES);
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
