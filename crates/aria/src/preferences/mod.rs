//! Shared user choices, independent of daemon setup and resource limits.

mod file;
mod store;

use std::path::PathBuf;

use crate::audio::{SilenceDuration, SilenceDurationError};
use crate::delivery::{OutputMode, PlatformTextDelivery, TextDelivery};
use crate::dictation::{DeliveryMode, DictationRequest, DictationSettings};
use crate::hotkey::HotkeyKey;
use crate::primitives::private_document::DocumentError;
use crate::transcription::{Language, LanguageCode, LanguageCodeError};

pub(crate) use store::PreferenceStore;
pub use store::{PreferencePatch, PreferenceSnapshot, UpdateResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPreferences {
    pub dictation_history: bool,
    pub history_audio_retention: bool,
    pub mute_output: bool,
    pub recognition_language: Language,
    pub output_mode: OutputMode,
    pub delivery_mode: DeliveryMode,
    pub silence_duration: SilenceDuration,
    /// Dictation types only what an enrolled voice said.
    pub own_voice_only: bool,
    pub dictation_hotkey: HotkeyKey,
    pub ask_hotkey: AskHotkey,
    pub project_vocabulary: bool,
    pub vocabulary_directory: Option<crate::vocabulary::Directory>,
}

/// The chord that runs an ask: press to record, again to send, again to
/// cancel. Nothing in the daemon reads it. It lives here because this is
/// where a choice shared between the app and the settings window belongs,
/// and because a chord is registered by whoever owns a run loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskHotkey {
    enabled: bool,
    key_code: u16,
    modifiers: Modifiers,
}

/// The modifiers held with the chord's key, named rather than masked: the
/// mask constants belong to one platform and this is stored on all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub control: bool,
    pub option: bool,
    pub shift: bool,
    pub command: bool,
}

impl Modifiers {
    pub fn is_empty(self) -> bool {
        !(self.control || self.option || self.shift || self.command)
    }
}

impl AskHotkey {
    /// A chord with no modifier takes a bare key away from every
    /// application, so an enabled one must carry a modifier.
    pub fn new(key_code: u16, modifiers: Modifiers, enabled: bool) -> Result<Self, AskHotkeyError> {
        if enabled && modifiers.is_empty() {
            return Err(AskHotkeyError::NoModifier);
        }
        Ok(Self {
            enabled,
            key_code,
            modifiers,
        })
    }

    pub fn enabled(self) -> bool {
        self.enabled
    }

    pub fn key_code(self) -> u16 {
        self.key_code
    }

    pub fn modifiers(self) -> Modifiers {
        self.modifiers
    }
}

impl Default for AskHotkey {
    /// Control-option-A. `0` is `kVK_ANSI_A`, the letter, not an absence.
    fn default() -> Self {
        Self {
            enabled: true,
            key_code: 0,
            modifiers: Modifiers {
                control: true,
                option: true,
                shift: false,
                command: false,
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AskHotkeyError {
    #[error("an enabled ask hotkey needs at least one modifier")]
    NoModifier,
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            dictation_history: true,
            history_audio_retention: false,
            mute_output: false,
            recognition_language: Language::Specific(LanguageCode::english()),
            output_mode: PlatformTextDelivery::DEFAULT_MODE,
            delivery_mode: DeliveryMode::Streaming,
            silence_duration: SilenceDuration::default(),
            own_voice_only: false,
            dictation_hotkey: HotkeyKey::RightOption,
            ask_hotkey: AskHotkey::default(),
            project_vocabulary: false,
            vocabulary_directory: None,
        }
    }
}

impl UserPreferences {
    pub(crate) fn resolve_dictation(self, request: DictationRequest) -> DictationSettings {
        DictationSettings {
            dictation_history: self.dictation_history,
            history_audio_retention: self.history_audio_retention,
            mute_output: self.mute_output,
            recognition_language: self.recognition_language,
            output_mode: request.output_mode.unwrap_or(self.output_mode),
            delivery_mode: request.delivery_mode.unwrap_or(self.delivery_mode),
            stop_mode: request.stop_mode,
            silence_duration: self.silence_duration,
            own_voice_only: self.own_voice_only,
            transcript_only: request.transcript_only,
            project: if self.project_vocabulary {
                match request.project {
                    crate::vocabulary::Context::Default => self.vocabulary_directory,
                    crate::vocabulary::Context::Directory(directory) => Some(directory),
                    crate::vocabulary::Context::Disabled => None,
                }
            } else {
                None
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PreferenceStoreError {
    #[error("preference file {} is not usable", path.display())]
    Document {
        path: PathBuf,
        #[source]
        source: DocumentError,
    },

    #[error("failed to parse preference file {}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("preference file {} contains an invalid recognition language", path.display())]
    InvalidLanguage {
        path: PathBuf,
        #[source]
        source: LanguageCodeError,
    },

    #[error("preference file {} contains an invalid silence duration", path.display())]
    InvalidSilenceDuration {
        path: PathBuf,
        #[source]
        source: SilenceDurationError,
    },

    #[error("preference file {} contains an invalid ask hotkey", path.display())]
    InvalidAskHotkey {
        path: PathBuf,
        #[source]
        source: AskHotkeyError,
    },

    #[error("failed to serialize preferences")]
    Serialize(#[source] serde_json::Error),

    #[error(
        "preference update was committed at {}, but its durability could not be confirmed",
        path.display()
    )]
    DurabilityUnconfirmed {
        path: PathBuf,
        #[source]
        source: DocumentError,
    },

    #[error(
        "serialized preferences contain {actual_bytes} bytes; maximum is {maximum_bytes} bytes"
    )]
    SerializedTooLarge {
        actual_bytes: usize,
        maximum_bytes: usize,
    },

    #[error("invalid vocabulary directory")]
    InvalidVocabularyDirectory(#[from] crate::vocabulary::DirectoryError),

    #[error("preference revision is exhausted")]
    RevisionExhausted,

    #[error("preference state lock is poisoned")]
    LockPoisoned,
}
