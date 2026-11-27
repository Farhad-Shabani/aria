//! System transcription: the machine's own audio output, captured through a Core
//! Audio process tap and transcribed continuously into a transcript file.

mod event;
mod folders;
mod session;
mod sink;
mod tap;
mod track;
pub mod unsaved;

pub use event::{EVENT_THREAD, SystemTranscriptionEventEmitter};
pub use event::{SystemAudioLevel, SystemTranscriptionEvent, SystemTranscriptionStage};
pub use folders::{FolderError, Kept, SaveHistory, StartError, TranscriptFolders};
pub use session::run_session;
pub use sink::TranscriptFile;
pub use sink::{GAP_BREAK, stamp};
pub use tap::SystemAudioCapture;

use aria_primitives::Cause;
use std::path::PathBuf;

use crate::stt::TranscriptionError;
use aria_settings::rpc::RpcErrorCode;

pub struct SystemTranscriptionStarted {
    pub already_running: bool,
    pub transcript: PathBuf,
    pub saving: TranscriptSaving,
}

/// What keeps a session's transcript once it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptSaving {
    /// The transcripts folder, with a history entry from the start.
    History,
    /// Unsaved, because **Save history** is off.
    HistoryOff,
    /// Unsaved, because Mac Audio history is full.
    HistoryFull,
    /// The file the caller named, which history never names.
    Output,
}

pub struct SystemTranscriptionStopped {
    pub was_running: bool,
    pub transcript: PathBuf,
}

pub struct SystemTranscriptionSnapshot {
    pub running: bool,
    pub transcript: PathBuf,
    /// How the last session ended, when it failed after Start answered.
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SystemTranscriptionError {
    #[error("failed to start the system-audio worker")]
    Spawn(#[source] std::io::Error),

    #[error("system-audio capture requires macOS 14.2 or newer")]
    UnsupportedOs,

    #[error(
        "system-audio recording is not permitted; allow it under \
         System Settings > Privacy & Security > Screen & System Audio Recording"
    )]
    PermissionDenied,

    // Boxed: the native cause is a macOS type, and this enum is cross-platform.
    // No source when the fault is a condition, such as an overflowed queue.
    #[error("system-audio capture failed: {operation}")]
    CaptureFailed {
        operation: &'static str,
        #[source]
        source: Option<Cause>,
    },

    #[error("transcription failed")]
    Transcription(#[source] TranscriptionError),

    #[error("failed to write the transcript file")]
    Transcript(#[source] std::io::Error),
}

/// The category a client sees, whether the failure ends a `stop` call or
/// reaches a follower as an event.
impl From<&SystemTranscriptionError> for RpcErrorCode {
    fn from(error: &SystemTranscriptionError) -> Self {
        match error {
            SystemTranscriptionError::UnsupportedOs => Self::SystemTranscriptionUnsupportedOs,
            SystemTranscriptionError::PermissionDenied => Self::SystemTranscriptionPermissionDenied,
            SystemTranscriptionError::Spawn(_) | SystemTranscriptionError::CaptureFailed { .. } => {
                Self::SystemTranscriptionCaptureFailed
            }
            SystemTranscriptionError::Transcription(_) => Self::TranscriptionFailed,
            SystemTranscriptionError::Transcript(_) => Self::Internal,
        }
    }
}
