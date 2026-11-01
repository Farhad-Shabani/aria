//! The checked settings the app, the phone and operations read.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use aria_kit::synthesis::SpeakingRate;
use aria_kit::transcription::{Language, LanguageCode};

use super::SettingsError;
use super::{ApplicationOverrides, Chord, Prompts};
use crate::audio::{AutoStop, SilenceDuration};
use crate::config::AssistantId;
use crate::delivery::{OutputMode, PlatformTextDelivery, TextDelivery};
use crate::hotkey::{AriaKey, Combo};

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeliveryMode {
    #[default]
    Complete,
    Streaming,
}

/// What a prompt makes of what was said, which sets how the assistant is told to
/// treat the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// The same words and meaning, cleaned up or restyled.
    Rewrite,
    /// New text made from what was said, such as a reply or a summary.
    Compose,
}

pub(crate) const MAX_DICTATION_TIMEOUT: Duration = Duration::from_secs(660);

/// The most `chat.timeout` may ask for.
pub(super) const MAX_CHAT_TIMEOUT: Duration = Duration::from_secs(570);

pub(crate) fn is_valid_model_id(model_id: &str) -> bool {
    const MAX_MODEL_ID_BYTES: usize = 200;
    model_id.len() <= MAX_MODEL_ID_BYTES
        && !model_id.starts_with('-')
        && !model_id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// Why a `dictation.cleanup_llm` table cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CleanupConfigError {
    #[error(
        "cleanup endpoint must be HTTP with a literal loopback address, explicit nonzero port, and /v1/chat/completions path"
    )]
    Endpoint,
    #[error("cleanup model must contain 1–256 bytes without whitespace or control characters")]
    Model,
}

pub(crate) fn validate_cleanup_llm(endpoint: &str, model: &str) -> Result<(), CleanupConfigError> {
    let address = endpoint
        .strip_prefix("http://")
        .and_then(|rest| rest.strip_suffix("/v1/chat/completions"))
        .and_then(|authority| authority.parse::<SocketAddr>().ok())
        .filter(|address| address.ip().is_loopback() && address.port() != 0);
    if address.is_none() {
        return Err(CleanupConfigError::Endpoint);
    }
    if model.is_empty()
        || model.len() > 256
        || model.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(CleanupConfigError::Model);
    }
    Ok(())
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
    use super::*;

    #[test]
    fn configuration_accepts_only_explicit_loopback_chat_endpoints() {
        for endpoint in [
            "http://127.0.0.1:8080/v1/chat/completions",
            "http://[::1]:8080/v1/chat/completions",
        ] {
            validate_cleanup_llm(endpoint, "local/model:1").unwrap();
        }
        for endpoint in [
            "http://localhost:8080/v1/chat/completions",
            "https://127.0.0.1:8080/v1/chat/completions",
            "http://192.168.0.1:8080/v1/chat/completions",
            "http://127.0.0.1/v1/chat/completions",
            "http://127.0.0.1:0/v1/chat/completions",
            "http://user@127.0.0.1:80/v1/chat/completions",
            "http://127.0.0.1:80/v1/chat/completions?key=secret",
            "http://127.0.0.1:80/v1/chat/completions#x",
            "http://127.0.0.1:80/other",
            "http://[::ffff:127.0.0.1]:80/v1/chat/completions",
        ] {
            assert!(
                matches!(
                    validate_cleanup_llm(endpoint, "local"),
                    Err(CleanupConfigError::Endpoint)
                ),
                "{endpoint}"
            );
        }
        for model in ["", " ", "model\nsecret", &"a".repeat(257)] {
            assert!(matches!(
                validate_cleanup_llm("http://127.0.0.1:80/v1/chat/completions", model),
                Err(CleanupConfigError::Model)
            ));
        }
    }
}
