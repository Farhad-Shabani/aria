//! Shared user choices, independent of daemon setup and resource limits.

mod file;
mod overrides;
mod store;

use std::path::PathBuf;

use crate::audio::{SilenceDuration, SilenceDurationError};
use crate::delivery::{OutputMode, PlatformTextDelivery, TextDelivery};
use crate::dictation::{DeliveryMode, DictationRequest, DictationSettings};
use crate::history::Application;
use crate::hotkey::HotkeyKey;
use crate::primitives::private_document::DocumentError;
use crate::transcription::{Language, LanguageCode, LanguageCodeError};

pub use overrides::{
    ApplicationId, ApplicationOverride, ApplicationOverrides, DictationOverrides, OverrideError,
};
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
    pub application_overrides: ApplicationOverrides,
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
            application_overrides: ApplicationOverrides::default(),
        }
    }
}

impl UserPreferences {
    /// `application` is the app in front as dictation is admitted. Each
    /// setting comes from the request, else that app's overrides, else these
    /// preferences.
    pub(crate) fn resolve_dictation(
        self,
        request: DictationRequest,
        application: Option<Application>,
    ) -> DictationSettings {
        let application_id = application
            .as_ref()
            .and_then(|application| application.bundle_id.clone());
        let application_overrides = application_id
            .as_deref()
            .and_then(|id| self.application_overrides.get(id));
        let target = crate::delivery::TargetPolicy {
            overridden_for: application_overrides.and(application_id),
            excluded: self.application_overrides.excluded_ids(),
        };
        // Exclusion of every app, not just this one, travels in `target`.
        let ApplicationOverride {
            excluded: _,
            overrides,
        } = application_overrides.unwrap_or_default();
        // Destructured so a new override field left unused here fails lint.
        let DictationOverrides {
            output_mode: application_output,
            delivery_mode: application_delivery,
        } = overrides;
        let output_mode = request
            .output_mode
            .or(application_output)
            .unwrap_or(self.output_mode);
        // Copy cannot stream, so it delivers after recording stops. A request
        // that asks to stream keeps its choice, and delivery refuses it.
        let delivery_mode = match (output_mode, request.delivery_mode) {
            (OutputMode::Copy, None) => DeliveryMode::Complete,
            (_, requested) => requested
                .or(application_delivery)
                .unwrap_or(self.delivery_mode),
        };
        DictationSettings {
            dictation_history: self.dictation_history,
            history_audio_retention: self.history_audio_retention,
            mute_output: self.mute_output,
            recognition_language: self.recognition_language,
            output_mode,
            delivery_mode,
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
            application,
            target,
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

    #[error("preference file {} contains invalid application overrides", path.display())]
    InvalidApplicationOverrides {
        path: PathBuf,
        #[source]
        source: OverrideError,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation::StopMode;

    const SLACK: &str = "com.tinyspeck.slackmacgap";

    fn front(bundle_id: &str) -> Option<Application> {
        Some(Application {
            name: "App".into(),
            bundle_id: Some(bundle_id.into()),
        })
    }

    fn with_slack(overrides: DictationOverrides) -> UserPreferences {
        UserPreferences {
            output_mode: OutputMode::Insert,
            delivery_mode: DeliveryMode::Streaming,
            application_overrides: ApplicationOverrides::new([(
                ApplicationId::new(SLACK).expect("valid"),
                ApplicationOverride {
                    excluded: false,
                    overrides,
                },
            )])
            .expect("valid"),
            ..UserPreferences::default()
        }
    }

    fn modes(settings: DictationSettings) -> (OutputMode, DeliveryMode) {
        (settings.output_mode, settings.delivery_mode)
    }

    #[test]
    fn the_front_apps_overrides_replace_preferences() {
        let preferences = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::InsertAndSubmit),
            delivery_mode: Some(DeliveryMode::Complete),
        });
        let request = DictationRequest::new(StopMode::OnSilence);

        assert_eq!(
            modes(
                preferences
                    .clone()
                    .resolve_dictation(request.clone(), front(SLACK))
            ),
            (OutputMode::InsertAndSubmit, DeliveryMode::Complete)
        );
        assert_eq!(
            modes(
                preferences
                    .clone()
                    .resolve_dictation(request.clone(), front("com.apple.Notes"))
            ),
            (OutputMode::Insert, DeliveryMode::Streaming)
        );
        assert_eq!(
            modes(preferences.resolve_dictation(request, None)),
            (OutputMode::Insert, DeliveryMode::Streaming)
        );
    }

    #[test]
    fn unset_overrides_inherit_and_requests_win() {
        let preferences = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::InsertAndSubmit),
            delivery_mode: None,
        });
        let request = DictationRequest {
            output_mode: Some(OutputMode::InsertOrCopy),
            ..DictationRequest::new(StopMode::OnSilence)
        };

        assert_eq!(
            modes(preferences.resolve_dictation(request, front(SLACK))),
            (OutputMode::InsertOrCopy, DeliveryMode::Streaming)
        );
    }

    #[test]
    fn copy_delivers_after_recording_unless_the_request_asks_to_stream() {
        let preferences = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::Copy),
            delivery_mode: Some(DeliveryMode::Streaming),
        });

        assert_eq!(
            modes(
                preferences
                    .clone()
                    .resolve_dictation(DictationRequest::new(StopMode::OnSilence), front(SLACK))
            ),
            (OutputMode::Copy, DeliveryMode::Complete)
        );
        let streaming = DictationRequest {
            delivery_mode: Some(DeliveryMode::Streaming),
            ..DictationRequest::new(StopMode::OnSilence)
        };
        assert_eq!(
            modes(preferences.resolve_dictation(streaming, front(SLACK))),
            (OutputMode::Copy, DeliveryMode::Streaming)
        );
    }

    #[test]
    fn the_front_app_is_kept_for_history() {
        let settings = UserPreferences::default()
            .resolve_dictation(DictationRequest::new(StopMode::OnSilence), front(SLACK));

        assert_eq!(settings.application, front(SLACK));
    }

    #[test]
    fn only_the_front_app_with_aria_off_is_excluded() {
        let mut preferences = with_slack(DictationOverrides::default());
        preferences.application_overrides = ApplicationOverrides::new([(
            ApplicationId::new(SLACK).expect("valid"),
            ApplicationOverride {
                excluded: true,
                overrides: DictationOverrides::default(),
            },
        )])
        .expect("valid");
        let request = DictationRequest::new(StopMode::OnSilence);

        assert!(
            preferences
                .clone()
                .resolve_dictation(request.clone(), front(SLACK))
                .application_excluded()
        );
        assert!(
            !preferences
                .resolve_dictation(request, front("com.apple.Notes"))
                .application_excluded()
        );
    }

    #[test]
    fn delivery_is_pinned_only_to_an_app_with_overrides() {
        let preferences = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::Copy),
            delivery_mode: None,
        });
        let request = DictationRequest::new(StopMode::OnSilence);

        assert_eq!(
            preferences
                .clone()
                .resolve_dictation(request.clone(), front(SLACK))
                .target
                .overridden_for
                .as_deref(),
            Some(SLACK)
        );
        assert_eq!(
            preferences
                .resolve_dictation(request, front("com.apple.Notes"))
                .target
                .overridden_for,
            None
        );
    }
}
