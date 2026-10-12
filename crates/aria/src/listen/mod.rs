//! Listen mode: the machine's own audio output, captured through a Core
//! Audio process tap and transcribed continuously into a transcript file.

mod event;
mod session;
mod sink;
mod tap;
mod track;

pub use event::ListenStage;
pub(crate) use event::{EVENT_THREAD, ListenEventEmitter};
pub(crate) use session::spawn_session;
pub(crate) use sink::TranscriptFile;
pub use sink::{GAP_BREAK, stamp};
pub(crate) use tap::SystemAudioCapture;

use crate::rpc::RpcErrorCode;
use crate::transcription::TranscriptionError;

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

impl ListenError {
    /// The category a client sees, whether the failure ends a `stop` call or
    /// reaches a follower as an event.
    pub(crate) fn code(&self) -> RpcErrorCode {
        match self {
            Self::UnsupportedOs => RpcErrorCode::ListenUnsupportedOs,
            Self::PermissionDenied => RpcErrorCode::ListenPermissionDenied,
            Self::Spawn(_) | Self::CaptureFailed { .. } => RpcErrorCode::ListenCaptureFailed,
            Self::Transcription(_) => RpcErrorCode::TranscriptionFailed,
            Self::Transcript(_) => RpcErrorCode::Internal,
        }
    }
}
