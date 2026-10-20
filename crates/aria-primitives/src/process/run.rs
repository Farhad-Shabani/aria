use std::fmt;
use std::io::{self, Read, Write};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::{ChildProcess, ChildProcessError, WaitOutcome};
use crate::cancel::CancelToken;

#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub timeout: Duration,
    pub stdout: Capture,
    pub stderr: Capture,
}

/// What to keep of a pipe, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Past the limit, the child is stopped and the run fails.
    FailAbove(usize),
    /// Keeps the first bytes and discards the rest as it arrives.
    KeepHead(usize),
    KeepTail(usize),
}

impl Capture {
    fn limit(self) -> usize {
        match self {
            Self::FailAbove(limit) | Self::KeepHead(limit) | Self::KeepTail(limit) => limit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pipe {
    Stdout,
    Stderr,
}

impl fmt::Display for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("failed to start the child process")]
    Spawn(#[source] io::Error),

    #[error("failed to write the child process's input")]
    Write(#[source] io::Error),

    #[error("failed to read the child process's output")]
    Read(#[source] io::Error),

    #[error("failed to wait for the child process")]
    Wait(#[source] ChildProcessError),

    #[error("the child process timed out after {0:?}")]
    TimedOut(Duration),

    #[error("the child process was cancelled")]
    Cancelled,

    #[error("the child process's {pipe} exceeded the output limit of {limit} bytes")]
    OutputTooLarge { pipe: Pipe, limit: usize },
}

/// Runs `command` to completion and returns what it wrote, as `bounds`
/// allows.
///
/// Both output pipes are drained while the child runs, so it never blocks on
/// a full pipe. An empty `input` gives the child `/dev/null`. A child that
/// exits before reading all of `input` is not a failure; its status says why.
///
/// An overflow outranks every other outcome. The deadline bounds the child's
/// process group; a descendant that left the group and still holds a pipe
/// delays the return until it exits.
pub fn run(
    command: &mut Command,
    input: &[u8],
    bounds: Bounds,
    cancel: &CancelToken,
) -> Result<Output, RunError> {
    run_with_listener(command, input, bounds, cancel, |_| {})
}

/// [`run`], with stdout handed to `on_stdout` as it is read. Under
/// [`Capture::FailAbove`] and [`Capture::KeepHead`] that is only the bytes within
/// the limit; under [`Capture::KeepTail`] it is every byte. It gets nothing
/// past a `FailAbove` limit, nor after a deadline or cancel stops the child.
pub fn run_with_listener(
    command: &mut Command,
    input: &[u8],
    bounds: Bounds,
    cancel: &CancelToken,
    mut on_stdout: impl FnMut(&[u8]) + Send,
) -> Result<Output, RunError> {
    let deadline = Instant::now() + bounds.timeout;
    command
        .stdin(if input.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildProcess::spawn(command).map_err(RunError::Spawn)?;
    let stdin = child.take_stdin();
    let stdout = child.take_stdout().ok_or_else(|| not_piped("stdout"))?;
    let stderr = child.take_stderr().ok_or_else(|| not_piped("stderr"))?;

    // Set when the run can no longer succeed. The wait then ends and the
    // child is stopped, instead of running to its deadline.
    let stop = AtomicBool::new(false);
    let (waited, stdout, stderr, written) = thread::scope(|scope| {
        // Owned in here so its drop stops the group before the scope joins
        // the readers, even while unwinding. A reader can block until the
        // group is gone.
        let mut child = child;
        let stdout = scope.spawn(|| {
            let _unwinding = StopOnUnwind(&stop);
            drain(stdout, bounds.stdout, &stop, |bytes| {
                if !stop.load(Ordering::Relaxed) {
                    on_stdout(bytes);
                }
            })
        });
        let stderr = scope.spawn(|| drain(stderr, bounds.stderr, &stop, |_| {}));
        let writer = stdin.map(|pipe| scope.spawn(|| write_input(pipe, input, &stop)));
        let waited = child.wait_until(deadline, || {
            stop.load(Ordering::Relaxed) || cancel.is_cancelled()
        });
        if !matches!(waited, Ok(WaitOutcome::Exited(_))) {
            stop.store(true, Ordering::Relaxed);
        }
        drop(child);
        let written = writer.map_or(Ok(()), joined);
        (waited, joined(stdout), joined(stderr), written)
    });
    outcome(&bounds, waited, stdout, stderr, written)
}

/// A pipe or input failure raises `stop`, so the wait can end as
/// `Cancelled`. Checking those failures first reports the real cause.
fn outcome(
    bounds: &Bounds,
    waited: Result<WaitOutcome, ChildProcessError>,
    stdout: io::Result<Drained>,
    stderr: io::Result<Drained>,
    written: io::Result<()>,
) -> Result<Output, RunError> {
    for (drained, pipe, capture) in [
        (&stdout, Pipe::Stdout, bounds.stdout),
        (&stderr, Pipe::Stderr, bounds.stderr),
    ] {
        if drained.as_ref().is_ok_and(|drained| drained.overflowed) {
            return Err(RunError::OutputTooLarge {
                pipe,
                limit: capture.limit(),
            });
        }
    }
    let stdout = stdout.map_err(RunError::Read)?.kept;
    let stderr = stderr.map_err(RunError::Read)?.kept;
    written.map_err(RunError::Write)?;
    match waited.map_err(RunError::Wait)? {
        WaitOutcome::Exited(status) => Ok(Output {
            status,
            stdout,
            stderr,
        }),
        WaitOutcome::TimedOut => Err(RunError::TimedOut(bounds.timeout)),
        WaitOutcome::Cancelled => Err(RunError::Cancelled),
    }
}

/// Raises `stop` if the thread unwinds, so a panicking listener ends the
/// wait instead of leaving it to the deadline.
struct StopOnUnwind<'a>(&'a AtomicBool);

impl Drop for StopOnUnwind<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.store(true, Ordering::Relaxed);
        }
    }
}

fn not_piped(stream: &str) -> RunError {
    RunError::Read(io::Error::other(format!(
        "the child's {stream} was not piped"
    )))
}

fn joined<T>(handle: thread::ScopedJoinHandle<'_, T>) -> T {
    handle
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

fn write_input(mut pipe: impl Write, input: &[u8], stop: &AtomicBool) -> io::Result<()> {
    match pipe.write_all(input) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => {
            stop.store(true, Ordering::Relaxed);
            Err(error)
        }
        Ok(()) => Ok(()),
    }
}

struct Drained {
    kept: Vec<u8>,
    overflowed: bool,
}

/// Bytes past the limit are read and dropped, so the child never blocks on
/// the pipe or dies of a closed one.
fn drain(
    mut pipe: impl Read,
    capture: Capture,
    stop: &AtomicBool,
    mut forward: impl FnMut(&[u8]),
) -> io::Result<Drained> {
    let mut kept = Vec::new();
    let mut overflowed = false;
    let mut chunk = [0; 8 * 1024];
    loop {
        let read = match pipe.read(&mut chunk) {
            Ok(0) => return Ok(Drained { kept, overflowed }),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) if overflowed => return Ok(Drained { kept, overflowed }),
            Err(error) => {
                stop.store(true, Ordering::Relaxed);
                return Err(error);
            }
        };
        let chunk = &chunk[..read];
        match capture {
            Capture::FailAbove(limit) | Capture::KeepHead(limit) => {
                let room = limit - kept.len();
                let taken = &chunk[..chunk.len().min(room)];
                kept.extend_from_slice(taken);
                if !taken.is_empty() {
                    forward(taken);
                }
                if chunk.len() > room && matches!(capture, Capture::FailAbove(_)) {
                    overflowed = true;
                    stop.store(true, Ordering::Relaxed);
                }
            }
            Capture::KeepTail(limit) => {
                kept.extend_from_slice(chunk);
                forward(chunk);
                if kept.len() > limit {
                    kept.drain(..kept.len() - limit);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATIENCE: Duration = Duration::from_secs(5);
    const LIMIT: usize = 1024;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    fn bounds(stdout: Capture, stderr: Capture) -> Bounds {
        Bounds {
            timeout: PATIENCE,
            stdout,
            stderr,
        }
    }

    fn failing() -> Bounds {
        bounds(Capture::FailAbove(LIMIT), Capture::FailAbove(LIMIT))
    }

    fn run_script(script: &str, input: &[u8], bounds: Bounds) -> Result<Output, RunError> {
        run(&mut shell(script), input, bounds, &CancelToken::never())
    }

    #[test]
    fn stdin_reaches_the_child_and_stdout_streams_back() {
        let mut streamed = Vec::new();
        let output = run_with_listener(
            &mut shell("cat; printf last"),
            b"one\ntwo\n",
            failing(),
            &CancelToken::never(),
            |bytes| streamed.extend_from_slice(bytes),
        )
        .expect("the run succeeds");

        assert!(output.status.success());
        assert_eq!(output.stdout, b"one\ntwo\nlast");
        assert_eq!(streamed, output.stdout);
    }

    #[test]
    fn an_empty_input_ends_at_once() {
        let output = run_script("cat", b"", failing()).expect("the run succeeds");

        assert!(output.stdout.is_empty());
    }

    #[test]
    fn a_child_past_a_fail_limit_is_stopped() {
        for (script, pipe) in [("yes", Pipe::Stdout), ("yes >&2", Pipe::Stderr)] {
            let started = Instant::now();
            let error = run_script(script, b"", failing()).expect_err("the output overflows");

            assert!(
                matches!(error, RunError::OutputTooLarge { pipe: p, limit: LIMIT } if p == pipe),
                "{error:?}"
            );
            assert!(started.elapsed() < PATIENCE);
        }
    }

    #[test]
    fn one_byte_past_the_limit_fails_even_after_a_clean_exit() {
        let script = format!("yes | head -c {}; exit 0", LIMIT + 1);
        let error = run_script(&script, b"", failing()).expect_err("the output overflows");

        assert!(
            matches!(error, RunError::OutputTooLarge { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn output_exactly_at_the_limit_is_kept() {
        let script = format!("yes | head -c {LIMIT}");
        let output = run_script(&script, b"", failing()).expect("the output fits");

        assert_eq!(output.stdout.len(), LIMIT);
    }

    #[test]
    fn keep_head_truncates_and_lets_the_child_finish() {
        // The shell writes the lines itself, so a closed pipe would kill it
        // before it reached `exit 7`.
        let script = "i=0; while [ $i -lt 2000 ]; do echo 0123456789012345678901234567890123456789; \
                      i=$((i+1)); done; exit 7";
        let output = run_script(
            script,
            b"",
            bounds(Capture::KeepHead(LIMIT), Capture::FailAbove(LIMIT)),
        )
        .expect("the run succeeds");

        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout.len(), LIMIT);
    }

    #[test]
    fn keep_tail_keeps_the_last_bytes() {
        let output = run_script(
            "yes | head -c 100000 >&2; printf END >&2",
            b"",
            bounds(Capture::FailAbove(LIMIT), Capture::KeepTail(3)),
        )
        .expect("the run succeeds");

        assert_eq!(output.stderr, b"END");
    }

    #[test]
    fn on_stdout_gets_what_the_capture_lets_through() {
        let script = "yes | head -c 100000";
        for (capture, expected) in [
            (Capture::KeepHead(LIMIT), LIMIT),
            (Capture::KeepTail(3), 100_000),
        ] {
            let mut streamed = 0;
            run_with_listener(
                &mut shell(script),
                b"",
                bounds(capture, Capture::FailAbove(LIMIT)),
                &CancelToken::never(),
                |bytes| streamed += bytes.len(),
            )
            .expect("the run succeeds");

            assert_eq!(streamed, expected, "{capture:?}");
        }
    }

    #[test]
    fn a_child_that_ignores_its_input_still_succeeds() {
        let output = run_script("exit 0", &[b'x'; 1_000_000], failing()).expect("the run succeeds");

        assert!(output.status.success());
    }

    #[test]
    fn a_background_descendant_does_not_hold_the_run() {
        let started = Instant::now();
        let output = run_script("sleep 30 & exit 0", b"", failing()).expect("the run succeeds");

        assert!(output.status.success());
        assert!(started.elapsed() < PATIENCE);
    }

    #[test]
    fn the_deadline_and_a_cancel_end_the_run_distinctly() {
        let timeout = Duration::from_millis(100);
        let started = Instant::now();
        let error = run_script(
            "sleep 30",
            b"",
            Bounds {
                timeout,
                ..failing()
            },
        )
        .expect_err("the deadline passes");
        assert!(
            matches!(error, RunError::TimedOut(t) if t == timeout),
            "{error:?}"
        );
        assert!(started.elapsed() < PATIENCE);

        let cancel = CancelToken::new();
        cancel.cancel();
        let started = Instant::now();
        let error =
            run(&mut shell("sleep 30"), b"", failing(), &cancel).expect_err("the run is cancelled");
        assert!(matches!(error, RunError::Cancelled), "{error:?}");
        assert!(started.elapsed() < PATIENCE);
    }

    #[test]
    fn a_panicking_cancel_check_still_stops_the_child() {
        let started = Instant::now();
        let unwound = std::panic::catch_unwind(|| {
            run(
                &mut shell("sleep 30"),
                b"",
                failing(),
                &CancelToken::from_fn(|| panic!("the check fails")),
            )
        });

        assert!(unwound.is_err());
        assert!(started.elapsed() < PATIENCE);
    }

    #[test]
    fn a_panicking_listener_still_stops_the_child() {
        let started = Instant::now();
        let unwound = std::panic::catch_unwind(|| {
            run_with_listener(
                &mut shell("echo hi; sleep 30"),
                b"",
                failing(),
                &CancelToken::never(),
                |_| panic!("the listener fails"),
            )
        });

        assert!(unwound.is_err());
        assert!(started.elapsed() < PATIENCE);
    }
}
