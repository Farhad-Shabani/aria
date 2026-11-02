use std::path::PathBuf;

/// Only macOS mutes output, so nothing is ever left muted here.
#[derive(Debug)]
pub struct MutedOutputs;

impl MutedOutputs {
    pub fn new(_path: impl Into<PathBuf>) -> Self {
        Self
    }

    pub fn restore(&self) {}
}
