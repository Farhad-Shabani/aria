//! The categories a daemon operation can fail in.
//!
//! One vocabulary, shared by the domain that raises a failure, the transport
//! that carries it, and the logs that record it. How a code is encoded on the
//! wire belongs to the IPC boundary; nothing here knows about it.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Declared in the wire enum's numeric order: generic, dictation, speech, chat,
// speaker, listen, history.
pub enum RpcErrorCode {
    InvalidRequest,
    Busy,
    Internal,
    Cancelled,
    Unauthenticated,
    Unsupported,
    ShuttingDown,
    PayloadTooLarge,
    PermissionDenied,
    DurabilityUnconfirmed,
    OverallTimeout,
    RecordingTimeout,
    DeliveryTimeout,
    CaptureFailed,
    NoSpeech,
    TranscriptionFailed,
    DeliveryFailed,
    DeliveryTargetChanged,
    DeliveryInterrupted,
    SynthesisFailed,
    PlaybackFailed,
    SpeechTimeout,
    VoiceUnavailable,
    AgentUnavailable,
    AgentFailed,
    AgentTimeout,
    SpeakerModelMissing,
    SpeakerAlreadyEnrolled,
    SpeakerNotEnrolled,
    SpeakerInconsistentSamples,
    SpeakerProfileWriteFailed,
    SpeakerNoSpeech,
    ListenUnsupportedOs,
    ListenPermissionDenied,
    ListenCaptureFailed,
    HistoryMissing,
    HistoryAudioUnavailable,
    HistoryUnavailable,
    HistoryProjectUnavailable,
    /// A code this build does not name, kept as it arrived so a newer daemon's
    /// failure is still reportable.
    Unknown(i32),
}

impl RpcErrorCode {
    pub fn label(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Busy => "busy",
            Self::Internal => "internal",
            Self::Cancelled => "cancelled",
            Self::Unauthenticated => "unauthenticated",
            Self::Unsupported => "unsupported",
            Self::ShuttingDown => "shutting_down",
            Self::PayloadTooLarge => "payload_too_large",
            Self::PermissionDenied => "permission_denied",
            Self::DurabilityUnconfirmed => "durability_unconfirmed",
            Self::OverallTimeout => "overall_timeout",
            Self::RecordingTimeout => "recording_timeout",
            Self::DeliveryTimeout => "delivery_timeout",
            Self::CaptureFailed => "capture_failed",
            Self::NoSpeech => "no_speech",
            Self::TranscriptionFailed => "transcription_failed",
            Self::DeliveryFailed => "delivery_failed",
            Self::DeliveryTargetChanged => "delivery_target_changed",
            Self::DeliveryInterrupted => "delivery_interrupted",
            Self::SynthesisFailed => "synthesis_failed",
            Self::PlaybackFailed => "playback_failed",
            Self::SpeechTimeout => "speech_timeout",
            Self::VoiceUnavailable => "voice_unavailable",
            Self::AgentUnavailable => "agent_unavailable",
            Self::AgentFailed => "agent_failed",
            Self::AgentTimeout => "agent_timeout",
            Self::SpeakerModelMissing => "speaker_model_missing",
            Self::SpeakerAlreadyEnrolled => "speaker_already_enrolled",
            Self::SpeakerNotEnrolled => "speaker_not_enrolled",
            Self::SpeakerInconsistentSamples => "speaker_inconsistent_samples",
            Self::SpeakerProfileWriteFailed => "speaker_profile_write_failed",
            Self::SpeakerNoSpeech => "speaker_no_speech",
            Self::ListenUnsupportedOs => "listen_unsupported_os",
            Self::ListenPermissionDenied => "listen_permission_denied",
            Self::ListenCaptureFailed => "listen_capture_failed",
            Self::HistoryMissing => "history_missing",
            Self::HistoryAudioUnavailable => "history_audio_unavailable",
            Self::HistoryUnavailable => "history_unavailable",
            Self::HistoryProjectUnavailable => "history_project_unavailable",
            Self::Unknown(_) => "unknown",
        }
    }
}

/// A failed request: a code and a message for the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcFailure {
    pub code: RpcErrorCode,
    pub message: String,
}

impl RpcFailure {
    pub fn new(code: RpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for RpcErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(value) => write!(formatter, "unknown ({value})"),
            code => formatter.write_str(code.label()),
        }
    }
}
