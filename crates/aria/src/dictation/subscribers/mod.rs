//! Everything that reacts to a dictation event.
//!
//! None of them can fail a press: a subscriber that cannot do its job logs
//! and returns.

use std::path::PathBuf;

use crate::dictation::DictationStage;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod status;

#[cfg(target_os = "linux")]
use linux::LinuxFeedback as PlatformFeedback;
#[cfg(target_os = "macos")]
use macos::MacosFeedback as PlatformFeedback;
use status::StatusFile;

#[cfg(target_os = "linux")]
use crate::rpc::RpcErrorCode;

pub(crate) struct DictationSubscribers {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    feedback: PlatformFeedback,
    status: StatusFile,
}

impl DictationSubscribers {
    pub(crate) fn new(status_file: impl Into<PathBuf>) -> Self {
        Self {
            #[cfg(target_os = "linux")]
            feedback: PlatformFeedback::new(),
            #[cfg(target_os = "macos")]
            feedback: PlatformFeedback::load(),
            status: StatusFile::new(status_file),
        }
    }

    pub(crate) fn observe(&self, stage: &DictationStage, _suppress_cues: bool) {
        #[cfg(target_os = "macos")]
        self.feedback.observe(stage, _suppress_cues);
        #[cfg(target_os = "linux")]
        self.feedback.observe(stage);
        self.status.observe(stage);
    }
}

/// What a failure says to someone reading a banner. `RpcErrorCode::label` is
/// the log and wire vocabulary and stays there.
#[cfg(target_os = "linux")]
fn message(code: RpcErrorCode) -> &'static str {
    match code {
        RpcErrorCode::NoSpeech => "No speech detected. Check your mic.",
        RpcErrorCode::CaptureFailed => "Recording failed. Check your mic.",
        RpcErrorCode::TranscriptionFailed => "Transcription failed.",
        RpcErrorCode::DeliveryTargetChanged => "The window changed, so nothing was inserted.",
        RpcErrorCode::DeliveryInterrupted => "Focus moved, so typing stopped.",
        RpcErrorCode::ApplicationExcluded => "Aria is off in this app.",
        RpcErrorCode::DeliveryFailed | RpcErrorCode::DeliveryTimeout => {
            "The transcript could not be delivered."
        }
        RpcErrorCode::RecordingTimeout | RpcErrorCode::OverallTimeout => {
            "The press ran past its time limit."
        }
        RpcErrorCode::InvalidRequest
        | RpcErrorCode::Busy
        | RpcErrorCode::PayloadTooLarge
        | RpcErrorCode::Internal
        | RpcErrorCode::Cancelled
        | RpcErrorCode::DurabilityUnconfirmed
        | RpcErrorCode::SynthesisFailed
        | RpcErrorCode::PlaybackFailed
        | RpcErrorCode::SpeechTimeout
        | RpcErrorCode::VoiceUnavailable
        | RpcErrorCode::SpeakerModelMissing
        | RpcErrorCode::SpeakerAlreadyEnrolled
        | RpcErrorCode::SpeakerNotEnrolled
        | RpcErrorCode::SpeakerInconsistentSamples
        | RpcErrorCode::SpeakerProfileWriteFailed
        | RpcErrorCode::SpeakerNoSpeech
        | RpcErrorCode::AssistantUnavailable
        | RpcErrorCode::AssistantFailed
        | RpcErrorCode::AssistantTimeout
        | RpcErrorCode::Unauthenticated
        | RpcErrorCode::PermissionDenied
        | RpcErrorCode::Unsupported
        | RpcErrorCode::ShuttingDown
        | RpcErrorCode::ListenUnsupportedOs
        | RpcErrorCode::ListenPermissionDenied
        | RpcErrorCode::ListenCaptureFailed
        | RpcErrorCode::HistoryMissing
        | RpcErrorCode::HistoryAudioUnavailable
        | RpcErrorCode::HistoryUnavailable
        | RpcErrorCode::HistoryProjectUnavailable
        | RpcErrorCode::Unknown(_) => "Something went wrong. See the daemon log.",
    }
}
