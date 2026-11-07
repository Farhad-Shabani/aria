//! The checked settings the app, the phone and operations read.

use std::collections::BTreeMap;

use aria_kit::synthesis::SpeakingRate;
use aria_kit::transcription::{Language, LanguageCode};

use super::{ApplicationOverrides, Choices, Chord, Prompts};
use super::{SettingsError, is_valid_model_id};
use crate::config::AssistantId;
use crate::rpc::RpcErrorCode;
use aria_kit::audio::{AutoStop, SilenceDuration};
use aria_platform::delivery::{OutputMode, PlatformTextDelivery, TextDelivery};
use aria_platform::hotkey::{AriaKey, Combo};

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
    pub vocabulary_dir: Option<super::Directory>,
    pub history_enabled: bool,
    pub history_keep_audio: bool,
    pub assistant_id: Option<AssistantId>,
    pub assistant_models: BTreeMap<AssistantId, String>,
    /// Asks are sent to the assistant to remember facts.
    pub memory_enabled: bool,
    pub prompts: Prompts,
    pub application_overrides: ApplicationOverrides,
    pub aria_key: AriaKey,
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
            if !is_valid_model_id(model_id) {
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
            mute_sound: true,
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
            aria_key: AriaKey::RightOption,
            ask_shortcut: Some(Shortcut::Combo(Combo::Shift)),
            prompt_list_shortcut: Some(Shortcut::Combo(Combo::Command)),
            stt_language: Language::Specific(LanguageCode::english()),
            tts_rate: SpeakingRate::standard(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeliveryMode {
    #[default]
    Complete,
    Streaming,
}

/// Why dictation with some output can't stream here, so it delivers after
/// recording stops. The stored delivery choice stays as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingUnavailable {
    /// The clipboard takes the whole text at once.
    Copy,
    /// This platform pastes, so it delivers once.
    Platform,
    /// The speech model decodes only the finished recording.
    Model,
}

/// Each output mode that can't stream under one speech engine, with why. A
/// client looks up the output it shows, so a change to it shows at once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamingSupport(Vec<StreamingUnavailableOutput>);

/// An output mode that can't stream, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamingUnavailableOutput {
    pub output_mode: OutputMode,
    pub reason: StreamingUnavailable,
}

impl StreamingSupport {
    /// Under the engine the settings name and this platform's delivery.
    pub fn of(choices: &Choices) -> Self {
        Self::new(choices.stt_streams(), PlatformTextDelivery::STREAMS)
    }

    /// Under an engine and a delivery that each can or can't stream.
    pub fn new(engine_streams: bool, delivery_streams: bool) -> Self {
        OutputMode::ALL
            .iter()
            .filter_map(|&output| {
                let reason = if output == OutputMode::Copy {
                    StreamingUnavailable::Copy
                } else if !delivery_streams {
                    StreamingUnavailable::Platform
                } else if !engine_streams {
                    StreamingUnavailable::Model
                } else {
                    return None;
                };
                Some(StreamingUnavailableOutput {
                    output_mode: output,
                    reason,
                })
            })
            .collect()
    }

    pub fn unavailable(&self, output: OutputMode) -> Option<StreamingUnavailable> {
        self.0
            .iter()
            .find(|entry| entry.output_mode == output)
            .map(|entry| entry.reason)
    }

    pub fn iter(&self) -> impl Iterator<Item = StreamingUnavailableOutput> + '_ {
        self.0.iter().copied()
    }
}

impl FromIterator<StreamingUnavailableOutput> for StreamingSupport {
    fn from_iter<I: IntoIterator<Item = StreamingUnavailableOutput>>(entries: I) -> Self {
        Self(entries.into_iter().collect())
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

impl From<&StoreError> for RpcErrorCode {
    fn from(error: &StoreError) -> Self {
        match error {
            StoreError::FileInvalid(_) => Self::SettingsInvalid,
            StoreError::BindingInUse
            | StoreError::InvalidModelId
            | StoreError::Settings(SettingsError::Invalid { .. } | SettingsError::Parse(_)) => {
                Self::InvalidRequest
            }
            StoreError::ChangedRecently | StoreError::Settings(SettingsError::Locked { .. }) => {
                Self::Busy
            }
            StoreError::Settings(SettingsError::ReadOnly { .. }) => Self::PermissionDenied,
            StoreError::Settings(
                SettingsError::Io { .. }
                | SettingsError::TooLarge { .. }
                | SettingsError::InvalidUtf8 { .. }
                | SettingsError::NotAFile { .. }
                | SettingsError::TornRead { .. }
                | SettingsError::UnknownKey(_)
                | SettingsError::SerializedTooLarge
                | SettingsError::Encode { .. }
                | SettingsError::Freeze(_)
                | SettingsError::Thread(_)
                | SettingsError::KeyAndAlias { .. },
            ) => Self::Internal,
        }
    }
}

#[cfg(test)]
mod rpc_tests {
    use super::*;
    use crate::rpc::RpcFailure;

    #[test]
    fn a_change_the_file_cannot_parse_quotes_nothing_from_it() {
        let fault =
            crate::Loaded::parse("[memory]\nenabled = maybe\n", std::path::Path::new("/home"))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn under(settings: &str) -> StreamingSupport {
        StreamingSupport::of(crate::testing::file(settings).choices())
    }

    fn whole() -> StreamingSupport {
        under("[stt]\nbackend = \"whisper-candle\"\n")
    }

    #[test]
    fn copy_never_streams_whatever_the_engine() {
        assert_eq!(
            whole().unavailable(OutputMode::Copy),
            Some(StreamingUnavailable::Copy)
        );
    }

    #[test]
    fn only_an_engine_that_decodes_the_finished_recording_stops_streaming() {
        let insert = OutputMode::Insert;
        let platform = (!PlatformTextDelivery::STREAMS).then_some(StreamingUnavailable::Platform);
        let reason = |settings| under(settings).unavailable(insert);

        assert_eq!(
            whole().unavailable(insert),
            platform.or(Some(StreamingUnavailable::Model))
        );
        assert_eq!(reason("[stt]\nbackend = \"parakeet\"\n"), platform);
        assert_eq!(
            reason("[stt]\nbackend = \"parakeet-mlx\"\ninterpreter = \"/usr/bin/python3\"\n"),
            platform.or(Some(StreamingUnavailable::Model))
        );
        assert_eq!(
            reason("[stt]\nbackend = \"whisper-candle\"\nruns_on = \"server\"\n"),
            platform,
            "the server's engine decides, not the local one kept for later"
        );
    }
}
