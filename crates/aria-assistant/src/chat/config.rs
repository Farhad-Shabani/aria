//! Where and how long `aria ask` runs the assistant.

use std::path::PathBuf;
use std::time::Duration;

/// Where and how long `aria ask` runs the assistant. Loaded per chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatConfig {
    /// Pins the assistant's working directory; otherwise the caller's.
    pub workdir: Option<PathBuf>,
    /// Directories whose git repos a question may name; empty turns routing off.
    pub project_roots: Vec<PathBuf>,
    pub timeout: Duration,
    /// A reply delivered like dictated text gets dictation's delivery timeout.
    pub delivery_timeout: Duration,
}
