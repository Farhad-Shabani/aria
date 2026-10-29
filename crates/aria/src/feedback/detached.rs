//! A fire-and-forget helper: run a command, log how it went, block nobody.

use std::process::Command;
use std::time::{Duration, Instant};

use crate::primitives::process::{ChildProcess, WaitOutcome};

/// Only bounds a helper that hangs; a healthy one finishes in under a second.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs the command on its own thread and forgets it.
///
/// The thread is what lets the caller move on: `ChildProcess` kills its group
/// when it drops, so the handle has to outlive the helper rather than the call.
pub(crate) fn run_detached(purpose: &'static str, mut command: Command) {
    spawn_detached(purpose, move || run(purpose, &mut command));
}

pub(crate) fn spawn_detached(purpose: &'static str, work: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name(format!("aria-{purpose}"))
        .spawn(work);
    if let Err(error) = spawned {
        tracing::warn!(%error, purpose, "failed to start the helper thread");
    }
}

fn run(purpose: &'static str, command: &mut Command) {
    let mut process = match ChildProcess::spawn(command) {
        Ok(process) => process,
        Err(error) => {
            tracing::warn!(%error, purpose, "failed to start the helper");
            return;
        }
    };
    match process.wait_until(Instant::now() + HELPER_TIMEOUT, || false) {
        Ok(WaitOutcome::Exited(status)) if status.success() => {}
        Ok(WaitOutcome::Exited(status)) => {
            tracing::warn!(%status, purpose, "the helper exited unsuccessfully");
        }
        // Cancellation is never asked for here, so only a hung helper arrives
        // without an exit status.
        Ok(WaitOutcome::TimedOut | WaitOutcome::Cancelled) => {
            tracing::warn!(?HELPER_TIMEOUT, purpose, "the helper did not finish");
        }
        Err(error) => tracing::warn!(%error, purpose, "failed to clean up the helper"),
    }
}
