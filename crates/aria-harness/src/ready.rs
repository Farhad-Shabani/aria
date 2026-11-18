use std::process::ExitStatus;
use std::time::{Duration, Instant};

use aria_primitives::process::{ChildProcess, ChildProcessError, WaitOutcome};

#[derive(Debug, thiserror::Error)]
pub enum ReadyError {
    #[error("the process ended with {0} before it was ready")]
    Exited(ExitStatus),

    #[error("the process was not ready within {0:?}")]
    TimedOut(Duration),

    #[error("failed to wait on the process")]
    Wait(#[source] ChildProcessError),
}

/// Waits until `ready` holds while `process` still runs.
pub fn wait_until_ready(
    process: &mut ChildProcess,
    timeout: Duration,
    ready: impl FnMut() -> bool,
) -> Result<(), ReadyError> {
    match process
        .wait_until(Instant::now() + timeout, ready)
        .map_err(ReadyError::Wait)?
    {
        WaitOutcome::Cancelled => Ok(()),
        WaitOutcome::Exited(status) => Err(ReadyError::Exited(status)),
        WaitOutcome::TimedOut => Err(ReadyError::TimedOut(timeout)),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn a_process_that_ends_before_the_predicate_holds_is_not_ready() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 3"]);
        let mut process = ChildProcess::spawn(&mut command).expect("spawns");

        let error = wait_until_ready(&mut process, Duration::from_secs(30), || false)
            .expect_err("never ready");

        assert!(
            matches!(&error, ReadyError::Exited(status) if status.code() == Some(3)),
            "{error}"
        );
    }

    #[test]
    fn a_running_process_is_ready_when_the_predicate_holds() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut process = ChildProcess::spawn(&mut command).expect("spawns");

        let ready = wait_until_ready(&mut process, Duration::from_secs(30), || true);

        process.terminate_in_place().expect("stops");
        assert!(ready.is_ok(), "{ready:?}");
    }
}
