//! Running another program: the process itself, and a protocol over one.

use std::time::Duration;

mod child;
mod sidecar;

/// How long a caller waiting on a child goes before it looks at `should_cancel`
/// again. It bounds how late a cancel can land.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

pub use child::{ChildProcess, ChildProcessError, WaitOutcome};
pub use sidecar::{Script, Sidecar, SidecarError, validate_interpreter};
