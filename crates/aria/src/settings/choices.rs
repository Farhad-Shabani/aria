//! The checked settings the app, the phone and operations read.

use std::collections::BTreeMap;

use aria_kit::synthesis::SpeakingRate;

use super::SettingsError;
use super::{ApplicationOverride, ApplicationOverrides, Chord, DictationOverrides, Prompts};
use crate::audio::{AutoStop, SilenceDuration};
use crate::config::AssistantId;
use crate::delivery::{OutputMode, PlatformTextDelivery, TextDelivery};
use crate::dictation::{DeliveryMode, DictationRequest, DictationSettings};
use crate::history::Application;
use crate::hotkey::{AriaKey, Combo};
use crate::transcription::{Language, LanguageCode};

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub output_mode: OutputMode,
    pub delivery_mode: DeliveryMode,
    pub silence_duration: SilenceDuration,
    /// `None` leaves a manual recording running until it is stopped.
    pub auto_stop: Option<AutoStop>,
    pub mute_sound: bool,
    /// Drops fillers and doubled words.
    pub cleanup: bool,
    /// Applies a spoken self-correction.
    pub backtrack: bool,
    /// Dictation types only what an enrolled voice said.
    pub own_voice_only: bool,
    pub vocabulary_enabled: bool,
    pub vocabulary_dir: Option<crate::vocabulary::Directory>,
    pub history_enabled: bool,
    pub history_keep_audio: bool,
    pub assistant_id: Option<AssistantId>,
    pub assistant_models: BTreeMap<AssistantId, String>,
    /// Asks are sent to the assistant to remember facts.
    pub memory_enabled: bool,
    pub prompts: Prompts,
    pub application_overrides: ApplicationOverrides,
    /// `None` is off: no listener runs.
    pub aria_key: Option<AriaKey>,
    /// Only the Mac app reads these. `None` is off.
    pub ask_shortcut: Option<Shortcut>,
    pub prompt_list_shortcut: Option<Shortcut>,
    pub stt_language: Language,
    pub tts_rate: SpeakingRate,
}

impl Settings {
    pub fn model_id(&self, assistant_id: AssistantId) -> Option<&str> {
        self.assistant_models
            .get(&assistant_id)
            .map(String::as_str)
            .filter(|id| !id.is_empty())
    }

    pub(crate) fn validate_assistant_models(&self) -> Result<(), StoreError> {
        for model_id in self.assistant_models.values() {
            if !crate::chat::is_valid_model_id(model_id) {
                return Err(StoreError::InvalidModelId);
            }
        }
        Ok(())
    }

    pub fn resolve_assistant_id(&self) -> AssistantId {
        self.assistant_id.unwrap_or(AssistantId::DEFAULT)
    }

    /// One shortcut can run only one thing: the ask, the prompt list, or a
    /// prompt. Prompts hold chords only, so a combo can clash only between
    /// the first two.
    pub(crate) fn shares_a_binding(&self) -> bool {
        if self.ask_shortcut.is_some() && self.ask_shortcut == self.prompt_list_shortcut {
            return true;
        }
        let chords: Vec<Chord> = [self.ask_shortcut, self.prompt_list_shortcut]
            .into_iter()
            .flatten()
            .filter_map(Shortcut::chord)
            .chain(
                self.prompts
                    .entries()
                    .iter()
                    .filter_map(|prompt| prompt.chord()),
            )
            .collect();
        chords
            .iter()
            .enumerate()
            .any(|(index, chord)| chords[..index].contains(chord))
    }
}

/// What starts the ask or the prompt list: an Aria key combo, or a chord of
/// the user's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    Combo(Combo),
    Chord(Chord),
}

impl Shortcut {
    pub fn chord(self) -> Option<Chord> {
        match self {
            Self::Chord(chord) => Some(chord),
            Self::Combo(_) => None,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_mode: PlatformTextDelivery::DEFAULT_MODE,
            delivery_mode: DeliveryMode::Streaming,
            silence_duration: SilenceDuration::default(),
            auto_stop: Some(AutoStop::default()),
            mute_sound: false,
            cleanup: false,
            backtrack: false,
            own_voice_only: false,
            vocabulary_enabled: false,
            vocabulary_dir: None,
            history_enabled: true,
            history_keep_audio: false,
            assistant_id: None,
            assistant_models: BTreeMap::new(),
            memory_enabled: false,
            prompts: Prompts::default(),
            application_overrides: ApplicationOverrides::default(),
            aria_key: Some(AriaKey::RightOption),
            ask_shortcut: Some(Shortcut::Combo(Combo::Shift)),
            prompt_list_shortcut: Some(Shortcut::Combo(Combo::Command)),
            stt_language: Language::Specific(LanguageCode::english()),
            tts_rate: SpeakingRate::standard(),
        }
    }
}

impl Settings {
    /// `application` is the app in front as dictation is admitted. Each
    /// setting comes from the request, else that app's overrides, else these
    /// settings.
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
            history_enabled: self.history_enabled,
            history_keep_audio: self.history_keep_audio,
            mute_sound: self.mute_sound,
            stt_language: self.stt_language,
            output_mode,
            delivery_mode,
            stop_mode: request.stop_mode,
            silence_duration: self.silence_duration,
            auto_stop: self.auto_stop,
            own_voice_only: self.own_voice_only,
            rewriting: crate::dictation::Rewriting {
                cleanup: self.cleanup,
                backtrack: self.backtrack,
            },
            transcript_only: request.transcript_only,
            project: if self.vocabulary_enabled {
                match request.project {
                    crate::vocabulary::Context::Default => self.vocabulary_dir,
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
pub enum StoreError {
    #[error(
        "model IDs must be at most 200 bytes, contain no whitespace or control characters, and not start with a dash"
    )]
    InvalidModelId,

    #[error("one shortcut is set for two things")]
    BindingInUse,

    #[error("settings could not be read or written")]
    Settings(#[from] SettingsError),

    /// Nothing is written over an edit in progress.
    #[error("fix settings.toml first")]
    FileInvalid(#[source] SettingsError),

    #[error("settings.toml kept changing; try again")]
    ChangedRecently,
}

#[cfg(test)]
mod tests {
    use super::super::ApplicationId;
    use super::*;
    use crate::dictation::StopMode;

    const SLACK: &str = "com.tinyspeck.slackmacgap";

    fn front(bundle_id: &str) -> Option<Application> {
        Some(Application {
            name: "App".into(),
            bundle_id: Some(bundle_id.into()),
        })
    }

    fn with_slack(overrides: DictationOverrides) -> Settings {
        Settings {
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
            ..Settings::default()
        }
    }

    fn modes(settings: DictationSettings) -> (OutputMode, DeliveryMode) {
        (settings.output_mode, settings.delivery_mode)
    }

    #[test]
    fn the_front_apps_overrides_replace_settings() {
        let settings = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::InsertAndSubmit),
            delivery_mode: Some(DeliveryMode::Complete),
        });
        let request = DictationRequest::new(StopMode::OnSilence);

        assert_eq!(
            modes(
                settings
                    .clone()
                    .resolve_dictation(request.clone(), front(SLACK))
            ),
            (OutputMode::InsertAndSubmit, DeliveryMode::Complete)
        );
        assert_eq!(
            modes(
                settings
                    .clone()
                    .resolve_dictation(request.clone(), front("com.apple.Notes"))
            ),
            (OutputMode::Insert, DeliveryMode::Streaming)
        );
        assert_eq!(
            modes(settings.resolve_dictation(request, None)),
            (OutputMode::Insert, DeliveryMode::Streaming)
        );
    }

    #[test]
    fn unset_overrides_inherit_and_requests_win() {
        let settings = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::InsertAndSubmit),
            delivery_mode: None,
        });
        let request = DictationRequest {
            output_mode: Some(OutputMode::InsertOrCopy),
            ..DictationRequest::new(StopMode::OnSilence)
        };

        assert_eq!(
            modes(settings.resolve_dictation(request, front(SLACK))),
            (OutputMode::InsertOrCopy, DeliveryMode::Streaming)
        );
    }

    #[test]
    fn copy_delivers_after_recording_unless_the_request_asks_to_stream() {
        let settings = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::Copy),
            delivery_mode: Some(DeliveryMode::Streaming),
        });

        assert_eq!(
            modes(
                settings
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
            modes(settings.resolve_dictation(streaming, front(SLACK))),
            (OutputMode::Copy, DeliveryMode::Streaming)
        );
    }

    #[test]
    fn the_front_app_is_kept_for_history() {
        let settings = Settings::default()
            .resolve_dictation(DictationRequest::new(StopMode::OnSilence), front(SLACK));

        assert_eq!(settings.application, front(SLACK));
    }

    #[test]
    fn only_the_front_app_with_aria_off_is_excluded() {
        let mut settings = with_slack(DictationOverrides::default());
        settings.application_overrides = ApplicationOverrides::new([(
            ApplicationId::new(SLACK).expect("valid"),
            ApplicationOverride {
                excluded: true,
                overrides: DictationOverrides::default(),
            },
        )])
        .expect("valid");
        let request = DictationRequest::new(StopMode::OnSilence);

        assert!(
            settings
                .clone()
                .resolve_dictation(request.clone(), front(SLACK))
                .application_excluded()
        );
        assert!(
            !settings
                .resolve_dictation(request, front("com.apple.Notes"))
                .application_excluded()
        );
    }

    #[test]
    fn delivery_is_pinned_only_to_an_app_with_overrides() {
        let settings = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::Copy),
            delivery_mode: None,
        });
        let request = DictationRequest::new(StopMode::OnSilence);

        assert_eq!(
            settings
                .clone()
                .resolve_dictation(request.clone(), front(SLACK))
                .target
                .overridden_for
                .as_deref(),
            Some(SLACK)
        );
        assert_eq!(
            settings
                .resolve_dictation(request, front("com.apple.Notes"))
                .target
                .overridden_for,
            None
        );
    }
}
