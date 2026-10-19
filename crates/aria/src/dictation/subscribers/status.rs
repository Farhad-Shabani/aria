//! What the daemon is doing, in a file an external indicator can read.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};

use crate::dictation::DictationStage;
use crate::primitives::error_chain;
use crate::primitives::private_fs::{self, PrivateFileError};

/// Every other value is a stage's own label.
const IDLE: &str = "";

pub(super) struct StatusFile {
    path: PathBuf,
}

impl StatusFile {
    pub(super) fn new(path: impl Into<PathBuf>) -> Self {
        let status = Self { path: path.into() };
        // A daemon killed mid-press leaves its last status behind, and only an
        // event clears it. Starting up is when that lie gets corrected.
        status.publish(IDLE);
        status
    }

    fn publish(&self, status: &str) {
        if let Err(error) = write_in_place(&self.path, status) {
            tracing::warn!(
                error = %error_chain(&error),
                path = %self.path.display(),
                "failed to publish the dictation status"
            );
        }
    }

    pub(super) fn observe(&self, stage: &DictationStage) {
        match stage {
            DictationStage::Recording(..)
            | DictationStage::Transcribing
            | DictationStage::Delivering => self.publish(stage.label()),
            // A segment lands while the speaker is still talking.
            DictationStage::SegmentTranscribed(..) | DictationStage::SegmentDelivered => {}
            DictationStage::Delivered { .. }
            | DictationStage::Cancelled
            | DictationStage::Failed(..) => {
                self.publish(IDLE);
            }
        }
    }
}

/// Write first, truncate after — `O_TRUNC` would leave a moment of empty
/// file that a watcher reads as "no status" and blinks its indicator off.
///
/// The daemon owns the file: a drifted mode is repaired, a symlink refused.
fn write_in_place(path: &Path, contents: &str) -> Result<(), StatusFileError> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let descriptor = rustix::fs::open(path, flags, Mode::RUSR | Mode::WUSR)
        .map_err(|source| StatusFileError::Open(source.into()))?;
    let mut file = File::from(descriptor);
    private_fs::prepare_file(&file).map_err(StatusFileError::Unusable)?;
    file.write_all(contents.as_bytes())
        .map_err(StatusFileError::Write)?;
    file.set_len(contents.len() as u64)
        .map_err(StatusFileError::Write)
}

#[derive(Debug, thiserror::Error)]
enum StatusFileError {
    #[error("failed to open the status file")]
    Open(#[source] std::io::Error),

    #[error("the status file is not usable")]
    Unusable(#[source] PrivateFileError),

    #[error("failed to write the status file")]
    Write(#[source] std::io::Error),
}
