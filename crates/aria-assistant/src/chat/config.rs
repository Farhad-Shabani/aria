//! Where and how long `aria ask` runs the assistant.

use std::path::PathBuf;
use std::time::Duration;

/// Where and how long `aria ask` runs the assistant. Loaded per ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatConfig {
    /// Pins the assistant's working directory; otherwise the caller's.
    pub workdir: Option<PathBuf>,
    /// Directories whose git repos an ask may name; empty turns routing off.
    pub project_roots: Vec<PathBuf>,
    pub timeout: Duration,
    /// A reply delivered like a transcript gets dictation's delivery timeout.
    pub delivery_timeout: Duration,
}
