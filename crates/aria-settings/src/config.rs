//! What every feature shares about resolving settings: engine names, the
//! `~/.aria` layout, and the errors a load can report.
//!
//! Engine crates declare the engine names. This crate builds without engines,
//! so it restates them, and each engine's owner holds the two equal with a
//! test.

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aria_kit::backend::ModelDownload;
use aria_primitives::user_home::{self, UserHomeError};

use crate::{InvalidValue, Key, Memory, SettingsError, Stt};

pub mod wake_backend {
    pub const OPEN_WAKE_WORD: &str = "openwakeword";
    pub const LIVEKIT: &str = "livekit";
    pub const DEFAULT: &str = OPEN_WAKE_WORD;
}

/// Names an engine under `tts.backend`, and labels that engine in its own
/// logs and errors.
pub mod tts_backend {
    pub const KOKORO: &str = "kokoro";
    pub const QWEN: &str = "qwen";
    pub const SYSTEM: &str = "system";
    pub const DEFAULT: &str = SYSTEM;
}

/// Names an engine under `stt.backend`, and labels that engine in
/// its own logs and errors. `SERVER` only labels a dictation the paired
/// server recognized.
pub mod backend {
    pub const SERVER: &str = "server";
    pub const WHISPER: &str = "whisper";
    pub const WHISPER_CANDLE: &str = "whisper-candle";
    pub const PARAKEET: &str = "parakeet";
    pub const MOONSHINE: &str = "moonshine";
    pub const NEMOTRON: &str = "nemotron";
    pub const PARAKEET_MLX: &str = "parakeet-mlx";
    pub const DEFAULT: &str = PARAKEET;

    pub const LOCAL: &[&str] = &[
        WHISPER,
        WHISPER_CANDLE,
        PARAKEET,
        MOONSHINE,
        NEMOTRON,
        PARAKEET_MLX,
    ];

    /// The local engines that decode while recording, so a dictation can type
    /// as you speak. A test in `aria-voice` pins this list to the engines.
    pub const STREAMING: &[&str] = &[WHISPER, PARAKEET, MOONSHINE, NEMOTRON];

    /// The canonical name of a local engine, or `None`.
    pub fn local(name: &str) -> Option<&'static str> {
        LOCAL.iter().copied().find(|engine| *engine == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AssistantId {
    ClaudeCode,
    Codex,
    /// A model on the local provider.
    LocalModel,
}

impl AssistantId {
    pub const ALL: &'static [Self] = &[Self::ClaudeCode, Self::Codex, Self::LocalModel];
    pub const DEFAULT: Self = Self::ClaudeCode;

    pub const fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::LocalModel => "local-model",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::LocalModel => "Local Provider",
        }
    }

    /// The local provider has no model of its own to fall back on.
    pub const fn needs_model(self) -> bool {
        matches!(self, Self::LocalModel)
    }
}

impl std::str::FromStr for AssistantId {
    type Err = ConfigError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|id| id.name() == name)
            .ok_or_else(|| ConfigError::UnknownAssistant(name.to_owned()))
    }
}

/// An HTTPS origin with no credentials, path, query, or fragment. Pairing
/// matches endpoints exactly, so spellings of one origin share one form.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServerEndpoint(String);

impl ServerEndpoint {
    pub fn parse(endpoint: &str) -> Option<Self> {
        let uri = endpoint.parse::<http::Uri>().ok()?;
        let authority = uri.authority()?.as_str();
        let valid = endpoint.len() <= 2048
            && uri.scheme_str() == Some("https")
            && uri.host().is_some()
            && !authority.contains('@')
            && uri.query().is_none()
            && !endpoint.contains('#')
            && matches!(uri.path(), "" | "/");
        valid.then(|| Self(format!("https://{}", authority.to_ascii_lowercase())))
    }
}

impl TryFrom<String> for ServerEndpoint {
    type Error = InvalidValue;

    fn try_from(endpoint: String) -> Result<Self, InvalidValue> {
        Self::parse(&endpoint).ok_or(InvalidValue::ServerEndpoint)
    }
}

impl From<ServerEndpoint> for String {
    fn from(endpoint: ServerEndpoint) -> Self {
        endpoint.0
    }
}

impl fmt::Display for ServerEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<crate::SettingsError> for ConfigError {
    fn from(error: crate::SettingsError) -> Self {
        Self::Settings(error)
    }
}

// A fault in the file is named by path. A fault in one setting is named by its
// field, so the message points at what to edit.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown assistant {0:?}; use aria ask --help to list choices")]
    UnknownAssistant(String),

    /// The engine that system transcription would run, its own or the one stt names, can't stream.
    #[error(
        "transcription cannot stream with the {0} engine; set transcription.backend to one of {engines}",
        engines = backend::STREAMING.join(", ")
    )]
    SystemTranscriptionUnusableEngine(String),
    #[error("cannot locate the Aria settings")]
    Home(#[source] UserHomeError),

    #[error("cannot read the settings")]
    Settings(#[source] SettingsError),

    #[error("environment variable {variable} is not valid UTF-8")]
    InvalidEnvironmentEncoding { variable: &'static str },

    /// The run turned wake-word monitoring on itself, so no key is at fault.
    #[error("wake-word monitoring requires capture.backend to be microphone")]
    WakeRequiresMicrophone,

    #[error("{key} {source}")]
    Invalid {
        key: Key,
        #[source]
        source: InvalidValue,
    },
}

/// Where setup downloads models, relative to home. The default model paths
/// below sit inside it.
const MODELS_DIR: &str = ".aria/models";

/// Where each model sits inside the models directory. Setup downloads into
/// the same folders.
pub mod model_folder {
    pub const WHISPER: &str = "whisper";
    pub const WHISPER_CANDLE: &str = "whisper-candle/tiny.en";
    pub const PARAKEET: &str = "parakeet/parakeet-tdt-0.6b-v3";
    pub const MOONSHINE: &str = "moonshine/base";
    pub const NEMOTRON: &str = "nemotron/nemotron-speech-streaming-en-0.6b";
    pub const QWEN: &str = "qwen/qwen3-tts-0.6b-customvoice";
    pub const QWEN_TOKENIZER: &str = "qwen/qwen3-tts-tokenizer";
    pub const KOKORO: &str = "kokoro";
    pub const SPEAKER: &str = "wespeaker/en";
    pub const SEGMENTATION: &str = "pyannote";
}

pub const DEFAULT_RECORDING_TIMEOUT: Duration = Duration::from_secs(600);
/// Long enough for an assistant to read a repo and think.
pub const DEFAULT_ASSISTANT_TIMEOUT: Duration = Duration::from_secs(300);
pub const DEFAULT_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_OVERALL_TIMEOUT: Duration = Duration::from_secs(660);

const DEFAULT_MEMORY_DIR: &str = ".aria/memory";

pub const UNATTENDED_ENV: &str = "ARIA_UNATTENDED";
/// Raw, because what counts as unattended is the daemon's call.
pub fn unattended() -> Result<Option<String>, ConfigError> {
    environment(UNATTENDED_ENV)
}

/// A downloaded file's path relative to the models directory.
pub fn model_file(folder: &str, file: &ModelDownload) -> PathBuf {
    Path::new(folder).join(file.file_name)
}

/// The configured path, else `default` inside the models directory.
pub fn model_location(value: Option<&Path>, default: impl AsRef<Path>, home: &Path) -> PathBuf {
    value.map_or_else(|| models_dir(home).join(default), Path::to_path_buf)
}

/// The recognition engine the section names, or the default.
pub fn stt_engine(file: &Stt) -> &str {
    file.backend.as_deref().unwrap_or(backend::DEFAULT)
}

pub fn models_dir(home: &Path) -> PathBuf {
    home.join(MODELS_DIR)
}

fn environment(variable: &'static str) -> Result<Option<String>, ConfigError> {
    let Some(value) = env::var_os(variable) else {
        return Ok(None);
    };
    value
        .into_string()
        .map(Some)
        .map_err(|_value| ConfigError::InvalidEnvironmentEncoding { variable })
}

pub fn home_directory() -> Result<PathBuf, ConfigError> {
    user_home::resolve().map_err(ConfigError::Home)
}

pub fn memory_directory(memory: &Memory, home: &Path) -> PathBuf {
    memory
        .dir
        .clone()
        .unwrap_or_else(|| home.join(DEFAULT_MEMORY_DIR))
}
