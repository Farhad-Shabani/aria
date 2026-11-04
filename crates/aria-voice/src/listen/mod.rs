//! Listen mode: the machine's own audio output, captured through a Core
//! Audio process tap and transcribed continuously into a transcript file.

mod event;
mod session;
mod sink;
mod tap;
mod track;

pub use event::ListenStage;
pub use event::{EVENT_THREAD, ListenEventEmitter};
pub use session::spawn_session;
pub use sink::TranscriptFile;
pub use sink::{GAP_BREAK, stamp};
pub use tap::SystemAudioCapture;

use std::path::PathBuf;

use crate::transcription::TranscriptionError;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};

pub struct ListenStarted {
    pub already_listening: bool,
    pub transcript: PathBuf,
}

pub struct ListenStopped {
    pub was_listening: bool,
    pub transcript: PathBuf,
}

pub struct ListenSnapshot {
    pub listening: bool,
    pub transcript: PathBuf,
    /// How the last session ended, when it failed after Start answered.
    pub error: Option<RpcFailure>,
}

#[derive(Debug, thiserror::Error)]
pub enum ListenError {
    #[error("failed to start the system-audio worker")]
    Spawn(#[source] std::io::Error),

    #[error("system-audio capture requires macOS 14.2 or newer")]
    UnsupportedOs,

    #[error(
        "system-audio recording is not permitted; allow it under \
         System Settings > Privacy & Security > Screen & System Audio Recording"
    )]
    PermissionDenied,

    #[error("system-audio capture failed: {detail}")]
    CaptureFailed { detail: String },

    #[error("listen transcription failed")]
    Transcription(#[source] TranscriptionError),

    #[error("failed to write the transcript file")]
    Transcript(#[source] std::io::Error),
}

/// The category a client sees, whether the failure ends a `stop` call or
/// reaches a follower as an event.
impl From<&ListenError> for RpcErrorCode {
    fn from(error: &ListenError) -> Self {
        match error {
            ListenError::UnsupportedOs => Self::ListenUnsupportedOs,
            ListenError::PermissionDenied => Self::ListenPermissionDenied,
            ListenError::Spawn(_) | ListenError::CaptureFailed { .. } => Self::ListenCaptureFailed,
            ListenError::Transcription(_) => Self::TranscriptionFailed,
            ListenError::Transcript(_) => Self::Internal,
        }
    }
}
