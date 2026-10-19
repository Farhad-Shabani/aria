//! The Accessibility grant, asked from a fresh process.
//!
//! In-process checks answer from a cache that misses later grant changes.
//! A new process always asks tccd.

use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cidre::ax;

use crate::primitives::error_chain;
use crate::primitives::process::{ChildProcess, ChildProcessError, WaitOutcome};

/// A hidden subcommand that exits with [`Trust::exit_code`].
pub const CHECK_SUBCOMMAND: &str = "accessibility-check";
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
/// Matches the hotkey health check, so delivery seldom waits on a check.
const REUSE_ANSWER_FOR: Duration = Duration::from_secs(3);

static LAST_ANSWER: Mutex<Option<Answer>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    Untrusted,
}

impl Trust {
    pub fn in_process() -> Self {
        if ax::is_process_trusted() {
            Self::Trusted
        } else {
            Self::Untrusted
        }
    }

    pub fn check_now() -> Self {
        let started = Instant::now();
        let checked = check_in_fresh_process();
        tracing::debug!(
            check_ms = started.elapsed().as_millis(),
            "Accessibility grant checked"
        );
        let mut last = LAST_ANSWER.lock().expect("lock");
        let previous = *last;
        let trust = checked.unwrap_or_else(|error| fall_back_to_last_answer(&error, previous));
        // Stamped on failure too, so `reuse_or_check` waits before retrying.
        let answer = Answer {
            trust,
            checked_at: Instant::now(),
        };
        if previous.map(|previous| previous.trust) != Some(answer.trust) {
            tracing::info!(
                trusted = answer.trust.is_trusted(),
                "Accessibility trust changed"
            );
        }
        *last = Some(answer);
        answer.trust
    }

    pub fn reuse_or_check() -> Self {
        let last = *LAST_ANSWER.lock().expect("lock");
        match last {
            Some(answer) if answer.checked_at.elapsed() < REUSE_ANSWER_FOR => answer.trust,
            _ => Self::check_now(),
        }
    }

    /// Shows macOS's Accessibility dialog, which also lists this executable
    /// under Accessibility, so the user never has to add it.
    ///
    /// Only the bundled helper asks. macOS charges the dialog to the
    /// responsible process, so a daemon run from a terminal would ask for,
    /// and grant, the terminal.
    pub fn request() {
        if runs_from_bundle() {
            ax::is_process_trusted_with_prompt(true);
        }
    }

    pub fn is_trusted(self) -> bool {
        self == Self::Trusted
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::Trusted => 0,
            Self::Untrusted => 1,
        }
    }

    fn from_exit_status(status: ExitStatus) -> Result<Self, CheckError> {
        match status.code() {
            Some(0) => Ok(Self::Trusted),
            Some(1) => Ok(Self::Untrusted),
            _ => Err(CheckError::Exit(status)),
        }
    }
}

fn runs_from_bundle() -> bool {
    std::env::current_exe().is_ok_and(|exe| is_bundle_executable(&exe))
}

fn is_bundle_executable(exe: &std::path::Path) -> bool {
    let mut ancestors = exe.ancestors().skip(1);
    let macos = ancestors.next().and_then(|dir| dir.file_name());
    let contents = ancestors.next().and_then(|dir| dir.file_name());
    let bundle = ancestors.next().and_then(|dir| dir.extension());
    macos.is_some_and(|name| name == "MacOS")
        && contents.is_some_and(|name| name == "Contents")
        && bundle.is_some_and(|ext| ext == "app")
}

#[derive(Clone, Copy)]
struct Answer {
    trust: Trust,
    checked_at: Instant,
}

#[derive(Debug, thiserror::Error)]
enum CheckError {
    #[error("could not start the check")]
    Spawn(#[source] io::Error),
    #[error("could not wait for the check")]
    Wait(#[source] ChildProcessError),
    #[error("the check did not finish")]
    Unfinished,
    #[error("the check exited with {0}")]
    Exit(ExitStatus),
}

fn check_in_fresh_process() -> Result<Trust, CheckError> {
    let exe = std::env::current_exe().map_err(CheckError::Spawn)?;
    let mut command = Command::new(exe);
    command.arg(CHECK_SUBCOMMAND);
    run_check(&mut command, CHECK_TIMEOUT)
}

/// A binary mid-upgrade must not read as a revoked grant.
fn fall_back_to_last_answer(error: &CheckError, previous: Option<Answer>) -> Trust {
    tracing::warn!(
        error = %error_chain(error),
        "could not re-check the Accessibility grant"
    );
    previous.map_or_else(Trust::in_process, |previous| previous.trust)
}

fn run_check(command: &mut Command, timeout: Duration) -> Result<Trust, CheckError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildProcess::spawn(command).map_err(CheckError::Spawn)?;
    match child
        .wait_until(Instant::now() + timeout, || false)
        .map_err(CheckError::Wait)?
    {
        WaitOutcome::Exited(status) => Trust::from_exit_status(status),
        WaitOutcome::Cancelled | WaitOutcome::TimedOut => Err(CheckError::Unfinished),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn exit_codes_round_trip() {
        for trust in [Trust::Trusted, Trust::Untrusted] {
            let script = format!("exit {}", trust.exit_code());
            assert_eq!(run_check(&mut sh(&script), CHECK_TIMEOUT).unwrap(), trust);
        }
        assert!(matches!(
            run_check(&mut sh("exit 2"), CHECK_TIMEOUT),
            Err(CheckError::Exit(_))
        ));
    }
}
