//! Domain types for spoken responses.

mod backend;
mod execution;

use aria_kit::synthesis::{SpeakingRate, SpeechText, SynthesisError, Voice};

use crate::event::EventEmitter;
use crate::rpc::RpcErrorCode;

pub(crate) use backend::SpeechBackend;
pub use backend::SpeechBuildError;
pub(crate) use execution::{ExecutionControl, SpeechCoordinator, SpeechExecutor};

pub(crate) const EVENT_THREAD: &str = "aria-speech-events";

/// What one utterance should say; `None` falls back to the configured
/// default.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechRequest {
    pub text: SpeechText,
    pub voice: Option<Voice>,
    pub rate: Option<SpeakingRate>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpeechSettings {
    pub(crate) text: SpeechText,
    pub(crate) voice: Option<Voice>,
    pub(crate) rate: SpeakingRate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechOutcome {
    Spoken,
    Stopped,
}

/// Where an utterance has got to. A failure names its category from the same
/// vocabulary the transport and the logs use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechStage {
    /// The engine is producing audio that has not reached the speaker yet.
    Synthesizing,
    /// Audio is actually flowing.
    Speaking,
    Spoken,
    Stopped,
    Cancelled,
    Failed(RpcErrorCode),
}

pub(crate) type SpeechEventEmitter = EventEmitter<SpeechStage>;

// A fault a client must distinguish needs its own variant: `status` and the
// RPC error code are derived from this enum.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SpeechError {
    #[error("another utterance is already being spoken")]
    Busy,

    #[error("speech was cancelled")]
    Cancelled,

    #[error("speech exceeded its deadline of {0:?}")]
    Timeout(std::time::Duration),

    #[error("synthesis failed: {0}")]
    Synthesis(#[source] SynthesisError),
}

impl From<&SpeechError> for RpcErrorCode {
    fn from(error: &SpeechError) -> Self {
        match error {
            SpeechError::Busy => Self::Busy,
            SpeechError::Cancelled => Self::Cancelled,
            SpeechError::Timeout(_) => Self::SpeechTimeout,
            SpeechError::Synthesis(SynthesisError::UnsupportedVoice { .. }) => {
                Self::VoiceUnavailable
            }
            // Synthesis reports a failed playback device as part of the utterance.
            #[cfg(not(target_family = "wasm"))]
            SpeechError::Synthesis(SynthesisError::Playback { .. }) => Self::PlaybackFailed,
            SpeechError::Synthesis(_) => Self::SynthesisFailed,
        }
    }
}

impl SpeechError {
    pub(crate) fn status(&self) -> &'static str {
        RpcErrorCode::from(self).label()
    }
}
