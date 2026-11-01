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
use aria_kit::synthesis::{SpeakingRateError, VoiceError};
use aria_primitives::user_home::{self, UserHomeError};

use crate::settings::{Delivery, Memory, SettingsError, Stt};

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
/// its own logs and errors.
pub mod backend {
    pub const SERVER: &str = "server";
    pub const WHISPER: &str = "whisper";
    pub const WHISPER_CANDLE: &str = "whisper-candle";
    pub const PARAKEET: &str = "parakeet";
    pub const MOONSHINE: &str = "moonshine";
    pub const NEMOTRON: &str = "nemotron";
    pub const PARAKEET_MLX: &str = "parakeet-mlx";
    pub const DEFAULT: &str = PARAKEET;

    /// The canonical name of a local engine, or `None`.
    pub fn local(name: &str) -> Option<&'static str> {
        [
            WHISPER,
            WHISPER_CANDLE,
            PARAKEET,
            MOONSHINE,
            NEMOTRON,
            PARAKEET_MLX,
        ]
        .into_iter()
        .find(|engine| *engine == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AssistantId {
    ClaudeCode,
    Codex,
}

impl AssistantId {
    pub const ALL: &'static [Self] = &[Self::ClaudeCode, Self::Codex];
    pub const DEFAULT: Self = Self::ClaudeCode;

    pub const fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
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
    type Error = ConfigError;

    fn try_from(endpoint: String) -> Result<Self, ConfigError> {
        Self::parse(&endpoint).ok_or(ConfigError::InvalidServerEndpoint)
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

impl From<crate::settings::SettingsError> for ConfigError {
    fn from(error: crate::settings::SettingsError) -> Self {
        Self::Settings(error)
    }
}

// A fault in the file is named by path. A fault in one setting is named by its
// field, so the message points at what to edit.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown assistant {0:?}; use aria ask --help to list choices")]
    UnknownAssistant(String),

    #[error("stt.server must be an HTTPS origin without credentials, query or fragment")]
    InvalidServerEndpoint,

    #[error(
        "listen requires a local engine; set listen.backend to parakeet, moonshine, or nemotron"
    )]
    ListenRequiresLocal,
    #[error("cannot locate the Aria settings")]
    Home(#[source] UserHomeError),

    #[error("cannot read the settings")]
    Settings(#[source] SettingsError),

    #[error("environment variable {variable} is not valid UTF-8")]
    InvalidEnvironmentEncoding { variable: &'static str },

    #[error("{field} must be a finite number of seconds greater than zero; got {seconds}")]
    InvalidDuration { field: &'static str, seconds: f64 },

    #[error("{field} does not accept {value:?}; supported values are {supported}")]
    UnsupportedValue {
        field: &'static str,
        value: String,
        supported: &'static str,
    },

    #[error("{field} does not apply to the {backend} backend")]
    NotApplicableToBackend {
        field: &'static str,
        backend: &'static str,
    },

    #[error("the {backend} backend requires {field}")]
    MissingValue {
        field: &'static str,
        backend: &'static str,
    },

    #[error("wake-word monitoring requires capture.backend to be microphone")]
    WakeRequiresMicrophone,

    #[error("invalid dictation.cleanup_llm settings")]
    CleanupLlm(#[source] crate::settings::CleanupConfigError),

    #[error("{field} is not a usable speaking rate")]
    InvalidSpeakingRate {
        field: &'static str,
        #[source]
        source: SpeakingRateError,
    },

    #[error("{field} is not a usable voice name")]
    InvalidVoice {
        field: &'static str,
        #[source]
        source: VoiceError,
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

/// The transcription engine the section names, or the default.
pub fn transcription_engine(file: &Stt) -> &str {
    file.backend.as_deref().unwrap_or(backend::DEFAULT)
}

pub fn models_dir(home: &Path) -> PathBuf {
    home.join(MODELS_DIR)
}

pub fn delivery_timeout(delivery: &Delivery) -> Result<Duration, ConfigError> {
    timeout(
        delivery.timeout_secs,
        "delivery.timeout_secs",
        DEFAULT_DELIVERY_TIMEOUT,
    )
}

pub fn string_value(file_value: Option<String>, default: &'static str) -> String {
    file_value.unwrap_or_else(|| default.to_owned())
}

pub fn timeout(
    file_value: Option<f64>,
    field: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    match file_value {
        Some(seconds) => duration(seconds, field),
        None => Ok(default),
    }
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

fn duration(seconds: f64, field: &'static str) -> Result<Duration, ConfigError> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(ConfigError::InvalidDuration { field, seconds });
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_error| ConfigError::InvalidDuration { field, seconds })
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
