//! Running another program: the process itself, and a protocol over one.

use std::time::Duration;

mod child;
mod run;
mod sidecar;

/// How long a caller waiting on a child goes before it looks at `should_cancel`
/// again. It bounds how late a cancel can land.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

pub use child::{ChildProcess, ChildProcessError, WaitOutcome};
pub use run::{Bounds, Capture, Pipe, RunError, run, run_with_listener};
pub use sidecar::{Script, Sidecar, SidecarError, validate_interpreter};

#[cfg(test)]
mod testing {
    use std::process::Command;
    use std::time::Duration;

    pub(super) const PATIENCE: Duration = Duration::from_secs(5);

    pub(super) fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }
}
