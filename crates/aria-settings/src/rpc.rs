//! The categories a daemon operation can fail in.
//!
//! One vocabulary, shared by the domain that raises a failure, the transport
//! that carries it, and the logs that record it. How a code is encoded on the
//! wire belongs to the IPC boundary; nothing here knows about it.
//!
//! A domain error declares its category once, beside itself, as
//! `impl From<&E> for RpcErrorCode`. That alone makes `RpcFailure::from(&error)`
//! available: the category, the whole cause chain as the message, and for a
//! settings fault its place in the file.

use std::fmt;

use aria_primitives::error_chain;

use crate::SettingsError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Declared in the wire enum's numeric order: generic, dictation, speech, chat,
// speaker, system transcription, history.
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
    /// settings.toml is invalid, so nothing was written over it.
    SettingsInvalid,
    OverallTimeout,
    RecordingTimeout,
    DeliveryTimeout,
    CaptureFailed,
    NoSpeech,
    TranscriptionFailed,
    DeliveryFailed,
    DeliveryTargetChanged,
    DeliveryInterrupted,
    ApplicationExcluded,
    SynthesisFailed,
    PlaybackFailed,
    SpeechTimeout,
    VoiceUnavailable,
    AssistantUnavailable,
    AssistantFailed,
    AssistantTimeout,
    AssistantSignedOut,
    PromptUnreadable,
    PromptMissing,
    PromptsFull,
    PromptWriteFailed,
    ChatNameTaken,
    SpeakerModelMissing,
    SpeakerAlreadyEnrolled,
    SpeakerNotEnrolled,
    SpeakerInconsistentSamples,
    SpeakerProfileWriteFailed,
    SpeakerNoSpeech,
    SystemTranscriptionUnsupportedOs,
    SystemTranscriptionPermissionDenied,
    SystemTranscriptionCaptureFailed,
    UnsavedTranscriptMissing,
    HistoryMissing,
    HistoryAudioUnavailable,
    HistoryUnavailable,
    HistoryProjectUnavailable,
    HistoryEntryEmpty,
    HistoryTranscriptUnreadable,
    HistoryFull,
    /// A code this build does not name, kept as it arrived so a newer daemon's
    /// failure is still reportable.
    Unknown(i32),
}

impl RpcErrorCode {
    /// The label as words, for a person to read.
    pub fn words(self) -> String {
        self.to_string().replace('_', " ")
    }

    /// What a failure says to a person: its own text, or else its code.
    pub fn reason(self, message: &str) -> String {
        if message.is_empty() {
            self.words()
        } else {
            message.to_owned()
        }
    }

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
            Self::SettingsInvalid => "settings_invalid",
            Self::OverallTimeout => "overall_timeout",
            Self::RecordingTimeout => "recording_timeout",
            Self::DeliveryTimeout => "delivery_timeout",
            Self::CaptureFailed => "capture_failed",
            Self::NoSpeech => "no_speech",
            Self::TranscriptionFailed => "transcription_failed",
            Self::DeliveryFailed => "delivery_failed",
            Self::DeliveryTargetChanged => "delivery_target_changed",
            Self::DeliveryInterrupted => "delivery_interrupted",
            Self::ApplicationExcluded => "application_excluded",
            Self::SynthesisFailed => "synthesis_failed",
            Self::PlaybackFailed => "playback_failed",
            Self::SpeechTimeout => "speech_timeout",
            Self::VoiceUnavailable => "voice_unavailable",
            Self::AssistantUnavailable => "assistant_unavailable",
            Self::AssistantFailed => "assistant_failed",
            Self::AssistantTimeout => "assistant_timeout",
            Self::AssistantSignedOut => "assistant_signed_out",
            Self::PromptUnreadable => "prompt_unreadable",
            Self::PromptMissing => "prompt_missing",
            Self::PromptsFull => "prompts_full",
            Self::PromptWriteFailed => "prompt_write_failed",
            Self::ChatNameTaken => "chat_name_taken",
            Self::SpeakerModelMissing => "speaker_model_missing",
            Self::SpeakerAlreadyEnrolled => "speaker_already_enrolled",
            Self::SpeakerNotEnrolled => "speaker_not_enrolled",
            Self::SpeakerInconsistentSamples => "speaker_inconsistent_samples",
            Self::SpeakerProfileWriteFailed => "speaker_profile_write_failed",
            Self::SpeakerNoSpeech => "speaker_no_speech",
            Self::SystemTranscriptionUnsupportedOs => "system_transcription_unsupported_os",
            Self::SystemTranscriptionPermissionDenied => "system_transcription_permission_denied",
            Self::SystemTranscriptionCaptureFailed => "system_transcription_capture_failed",
            Self::UnsavedTranscriptMissing => "unsaved_transcript_missing",
            Self::HistoryMissing => "history_missing",
            Self::HistoryAudioUnavailable => "history_audio_unavailable",
            Self::HistoryUnavailable => "history_unavailable",
            Self::HistoryProjectUnavailable => "history_project_unavailable",
            Self::HistoryEntryEmpty => "history_entry_empty",
            Self::HistoryTranscriptUnreadable => "history_transcript_unreadable",
            Self::HistoryFull => "history_full",
            Self::Unknown(_) => "unknown",
        }
    }
}

/// A failed request: a code and a message for the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcFailure {
    pub code: RpcErrorCode,
    pub message: String,
    /// With [`RpcErrorCode::SettingsInvalid`]: where in settings.toml.
    pub settings_position: Option<crate::Position>,
}

impl RpcFailure {
    pub fn new(code: RpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            settings_position: None,
        }
    }

    pub fn with_settings_position(mut self, position: Option<crate::Position>) -> Self {
        self.settings_position = position;
        self
    }
}

impl fmt::Display for RpcFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.code.reason(&self.message))
    }
}

impl std::error::Error for RpcFailure {}

impl<'a, E> From<&'a E> for RpcFailure
where
    E: std::error::Error + 'static,
    RpcErrorCode: From<&'a E>,
{
    fn from(error: &'a E) -> Self {
        let code = RpcErrorCode::from(error);
        let fault = settings_fault(error);
        if code == RpcErrorCode::SettingsInvalid {
            let position = fault.and_then(SettingsError::position);
            return Self::new(code, error_chain(error)).with_settings_position(position);
        }
        // A change the file cannot take names the key, never the value: the
        // message may cross to a phone, and the value may be a path.
        let message = match fault {
            Some(SettingsError::Invalid { key, .. }) => format!("{key} does not accept that value"),
            Some(SettingsError::Parse(_)) => "the change does not fit settings.toml".to_owned(),
            Some(
                SettingsError::Io { .. }
                | SettingsError::TooLarge { .. }
                | SettingsError::NotAFile { .. }
                | SettingsError::TornRead { .. }
                | SettingsError::InvalidUtf8 { .. }
                | SettingsError::UnknownKey(_)
                | SettingsError::Locked { .. }
                | SettingsError::ReadOnly { .. }
                | SettingsError::SerializedTooLarge
                | SettingsError::Encode { .. }
                | SettingsError::Freeze(_)
                | SettingsError::Thread(_)
                | SettingsError::KeyAndAlias { .. },
            )
            | None => error_chain(error),
        };
        Self::new(code, message)
    }
}

/// The settings fault somewhere in the cause chain.
fn settings_fault<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a SettingsError> {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(fault) = error.downcast_ref::<SettingsError>() {
            return Some(fault);
        }
        cause = error.source();
    }
    None
}

/// Every error that declares its category, as one bound. A generic wrapper
/// such as an actor's call error bounds its payload on this rather than on
/// `From<&E>`: the solver would otherwise chase `From<&Wrapper<Wrapper<_>>>`
/// forever while the payload type is still being inferred.
pub trait Categorized: std::error::Error {
    fn category(&self) -> RpcErrorCode;
}

impl<E> Categorized for E
where
    E: std::error::Error,
    for<'a> RpcErrorCode: From<&'a E>,
{
    fn category(&self) -> RpcErrorCode {
        self.into()
    }
}

/// An actor whose work cannot fail still reports the actor's own failures.
impl From<&std::convert::Infallible> for RpcErrorCode {
    fn from(never: &std::convert::Infallible) -> Self {
        match *never {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreError;

    #[test]
    fn a_change_the_file_cannot_parse_quotes_nothing_from_it() {
        let fault =
            crate::Loaded::parse("[memory]\nenabled = maybe\n", &crate::testing::test_home())
                .expect_err("invalid");

        let failure = RpcFailure::from(&StoreError::Settings(fault));

        assert_eq!(failure.code, RpcErrorCode::InvalidRequest);
        assert_eq!(failure.message, "the change does not fit settings.toml");
        assert!(!failure.message.contains("maybe"));
        assert_eq!(failure.settings_position, None);
    }

    #[test]
    fn a_rejected_value_names_its_key_and_never_the_value() {
        let key = crate::Key::ALL[0];
        let fault = crate::invalid(
            key,
            crate::InvalidValue::Unsupported {
                value: "/Users/someone/private".to_owned(),
                supported: "a, b".to_owned(),
            },
        );

        let failure = RpcFailure::from(&StoreError::Settings(fault));

        assert_eq!(failure.code, RpcErrorCode::InvalidRequest);
        assert_eq!(failure.message, format!("{key} does not accept that value"));
        assert!(!failure.message.contains("private"));
    }

    #[test]
    fn a_failure_reads_as_its_message_or_else_its_code() {
        let named = RpcFailure::new(
            RpcErrorCode::DeliveryTargetChanged,
            "the window changed".to_owned(),
        );
        let bare = RpcFailure::new(RpcErrorCode::HistoryUnavailable, String::new());
        let newer = RpcFailure::new(RpcErrorCode::Unknown(42), String::new());

        assert_eq!(named.to_string(), "the window changed");
        assert_eq!(bare.to_string(), "history unavailable");
        assert_eq!(newer.to_string(), "unknown (42)");
    }
}
