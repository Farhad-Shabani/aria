//! Immutable configuration for Aria capabilities.

mod file;
mod load;
mod show;

pub(crate) use file::LlmCleanupConfig;

use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use aria_kit::synthesis::{SpeakingRate, SpeakingRateError, Voice, VoiceError};

use crate::primitives::user_home::UserHomeError;

pub(crate) struct DaemonConfig {
    pub(crate) dictation: DictationConfig,
    pub(crate) wake: WakeConfig,
    pub(crate) hotkey: HotkeyConfig,
    pub(crate) relay: RelayConfig,
    pub(crate) tts: TtsConfig,
}

impl DaemonConfig {
    pub(crate) fn load(
        environment: &Environment,
        wake_word: WakeWordOverride,
    ) -> Result<Self, ConfigError> {
        load::load_daemon(environment, wake_word)
    }
}

/// The command's say over `wake.enabled`, outranking both the environment and
/// the file. `aria wake` exists to listen, so it enables monitoring whatever
/// the configuration says. Nothing here can turn monitoring off.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WakeWordOverride {
    Inherit,
    Enable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictationConfig {
    pub(crate) capture: CaptureConfig,
    pub(crate) transcription: TranscriptionBackend,
    pub(crate) dictation_timeouts: DictationTimeouts,
    /// Drops fillers and doubled words before delivery.
    pub(crate) cleanup: bool,
    pub(crate) backtrack: bool,
    pub(crate) cleanup_llm: Option<LlmCleanupConfig>,
}

/// The global dictation hotkey: one bare modifier key, tapped or held. Which
/// key is a user preference, not config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HotkeyConfig {
    Disabled,
    Enabled,
}

/// Whether this Mac takes recordings relayed off a phone or watch on the
/// LAN, and on which port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayConfig {
    Disabled,
    Enabled { port: u16 },
}

impl RelayConfig {
    pub(crate) fn port(self) -> Option<u16> {
        match self {
            Self::Disabled => None,
            Self::Enabled { port } => Some(port),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WakeConfig {
    Disabled,
    Enabled {
        backend: WakeBackend,
        classifier: PathBuf,
        threshold: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeBackend {
    OpenWakeWord,
    LiveKit,
}

impl WakeBackend {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::OpenWakeWord => wake_backend::OPEN_WAKE_WORD,
            Self::LiveKit => wake_backend::LIVEKIT,
        }
    }
}

pub mod wake_backend {
    pub const OPEN_WAKE_WORD: &str = aria_wake::openwakeword::NAME;
    pub const LIVEKIT: &str = aria_wake::livekit::NAME;
    pub const DEFAULT: &str = OPEN_WAKE_WORD;
}

/// The speech defaults every utterance falls back to when its request names
/// no voice or rate of its own.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TtsConfig {
    pub(crate) backend: TtsBackend,
    pub(crate) voice: Option<Voice>,
    pub(crate) rate: SpeakingRate,
    pub(crate) fallback: TtsFallback,
}

impl TtsConfig {
    pub(crate) fn load(environment: &Environment) -> Result<Self, ConfigError> {
        load::load_tts(environment)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TtsBackend {
    Model(TtsModel),
    System,
}

/// An engine that synthesizes PCM from model files on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TtsModel {
    Kokoro {
        model: PathBuf,
        voices: PathBuf,
    },
    Qwen {
        model_dir: PathBuf,
        tokenizer_dir: PathBuf,
    },
}

impl TtsBackend {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Model(model) => model.name(),
            Self::System => tts_backend::SYSTEM,
        }
    }
}

impl TtsModel {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Kokoro { .. } => tts_backend::KOKORO,
            Self::Qwen { .. } => tts_backend::QWEN,
        }
    }
}

/// What happens when the configured engine cannot be built: give up, or speak
/// through the platform voice instead. Decided once, when the engine is
/// built, never per utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TtsFallback {
    None,
    System,
}

/// Names an engine under `tts.backend`, and labels that engine in its own
/// logs and errors.
pub mod tts_backend {
    pub const KOKORO: &str = aria_tts::kokoro::NAME;
    pub const QWEN: &str = aria_tts::qwen::NAME;
    pub const SYSTEM: &str = aria_tts::system::NAME;
    pub const DEFAULT: &str = SYSTEM;
}

impl DictationConfig {
    pub(crate) fn load(environment: &Environment) -> Result<Self, ConfigError> {
        load::load(environment)
    }

    pub(crate) fn rewrites(
        &self,
        home: &std::path::Path,
    ) -> Result<
        crate::dictation::Rewrites<crate::dictation::ConfiguredLayers>,
        crate::dictation::CleanupError,
    > {
        let llm = self
            .cleanup_llm
            .as_ref()
            .map(|cleanup| {
                crate::dictation::LlmCleanup::new(&cleanup.endpoint, &cleanup.model, home)
            })
            .transpose()?;
        Ok(crate::dictation::Rewrites::new()
            .removing_disfluencies(self.cleanup)
            .backtracking(self.backtrack)
            .with(llm))
    }
}

/// An HTTPS origin with no credentials, path, query, or fragment. Pairing
/// matches endpoints exactly, so spellings of one origin share one form.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct ServerEndpoint(String);

impl ServerEndpoint {
    pub(crate) fn parse(endpoint: &str) -> Option<Self> {
        let uri = endpoint.parse::<tonic::codegen::http::Uri>().ok()?;
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

/// Where dictation and recorded transcription run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptionBackend {
    Local(TranscriptionConfig),
    Server { endpoint: ServerEndpoint },
}

impl TranscriptionBackend {
    pub(crate) fn load(environment: &Environment) -> Result<Self, ConfigError> {
        load::load_transcription(environment)
    }

    pub(crate) fn backend_name(&self) -> &'static str {
        match self {
            Self::Local(engine) => engine.backend_name(),
            Self::Server { .. } => backend::SERVER,
        }
    }
}

/// The assistant `aria ask` talks to. Loaded per ask, never at daemon startup,
/// so a broken chat section blocks nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatConfig {
    pub(crate) assistant_id: AssistantId,
    /// Pins the assistant's working directory; otherwise the caller's.
    pub(crate) workdir: Option<PathBuf>,
    /// Directories whose git repos an ask may name; empty turns routing off.
    pub(crate) project_roots: Vec<PathBuf>,
    pub(crate) timeout: Duration,
    /// A reply delivered like a transcript gets dictation's delivery timeout.
    pub(crate) delivery_timeout: Duration,
}

impl ChatConfig {
    pub(crate) fn load(
        environment: &Environment,
        assistant_id: AssistantId,
    ) -> Result<Self, ConfigError> {
        let mut environment = environment.clone();
        environment.assistant_id = Some(assistant_id.name().to_owned());
        load::load_chat(&environment)
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

    pub(crate) fn load(environment: &Environment) -> Result<Self, ConfigError> {
        load::load_assistant_id(environment)
    }

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

/// Listen mode: system-audio capture transcribed to a file. Loaded per Start,
/// never at daemon startup, so a broken listen section blocks nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListenConfig {
    pub(crate) transcription: ListenTranscription,
    pub(crate) transcripts_dir: PathBuf,
}

/// The engines that can transcribe a live stream — resolution refuses the
/// rest, so holding this type is proof the backend streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListenTranscription {
    Parakeet {
        model_directory: PathBuf,
        device: TranscriptionDevice,
    },
    Moonshine {
        model_directory: PathBuf,
    },
    Nemotron {
        model_directory: PathBuf,
    },
}

impl ListenTranscription {
    pub(crate) fn backend_name(&self) -> &'static str {
        match self {
            Self::Parakeet { .. } => backend::PARAKEET,
            Self::Moonshine { .. } => backend::MOONSHINE,
            Self::Nemotron { .. } => backend::NEMOTRON,
        }
    }

    pub(crate) fn model_directory(self) -> PathBuf {
        match self {
            Self::Parakeet {
                model_directory, ..
            }
            | Self::Moonshine { model_directory }
            | Self::Nemotron { model_directory } => model_directory,
        }
    }
}

/// Where the voice-embedding model and the enrolled profiles live. One
/// enrollment serves every feature that recognizes a speaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpeakerConfig {
    pub(crate) model: PathBuf,
    /// Segmentation model path. If unavailable, Listen labels per segment.
    pub(crate) segmentation_model: PathBuf,
    pub(crate) speakers_dir: PathBuf,
}

impl SpeakerConfig {
    pub(crate) fn load(environment: &Environment) -> Result<Self, ConfigError> {
        load::load_speaker(environment)
    }
}

/// What the assistant remembers across sessions and how it learns it.
/// `dir` also holds the lexicon, which works whether or not `enabled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryConfig {
    pub(crate) enabled: bool,
    /// A model name the `claude` CLI accepts for fact extraction.
    pub(crate) extract_model: String,
    pub(crate) max_facts_per_session: u32,
    pub(crate) dir: PathBuf,
}

impl MemoryConfig {
    pub(crate) fn load(environment: &Environment) -> Result<Self, ConfigError> {
        load::load_memory(environment)
    }
}

impl ListenConfig {
    pub(crate) fn load(environment: &Environment) -> Result<Self, ConfigError> {
        load::load_listen(environment)
    }
}

pub(crate) use file::{FileConfig, read as read_file};
pub(crate) use load::{
    Environment, LoadedModel, UNATTENDED_ENV, default_path, loaded_models, memory_directory,
    model_folder, models_dir, transcription_engine, tts_engine,
};
pub use show::show;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionConfig {
    Whisper {
        model: PathBuf,
    },
    WhisperCandle {
        model_directory: PathBuf,
    },
    Parakeet {
        model_directory: PathBuf,
        device: TranscriptionDevice,
    },
    Moonshine {
        model_directory: PathBuf,
    },
    Nemotron {
        model_directory: PathBuf,
    },
    ParakeetMlx {
        model: String,
        interpreter: PathBuf,
    },
}

impl TranscriptionConfig {
    /// `None` for parakeet-mlx, which fetches its model by name.
    pub(crate) fn model_path(self) -> Option<PathBuf> {
        match self {
            Self::Whisper { model, .. } => Some(model),
            Self::WhisperCandle {
                model_directory, ..
            }
            | Self::Parakeet {
                model_directory, ..
            }
            | Self::Moonshine { model_directory }
            | Self::Nemotron { model_directory } => Some(model_directory),
            Self::ParakeetMlx { .. } => None,
        }
    }

    pub(crate) fn for_backend(
        &self,
        backend: &str,
        home: &std::path::Path,
    ) -> Result<Self, ConfigError> {
        if self.backend_name() == backend {
            return Ok(self.clone());
        }
        load::transcription_defaults(backend, home)
    }

    pub(crate) fn backend_name(&self) -> &'static str {
        match self {
            Self::Whisper { .. } => backend::WHISPER,
            Self::WhisperCandle { .. } => backend::WHISPER_CANDLE,
            Self::Parakeet { .. } => backend::PARAKEET,
            Self::Moonshine { .. } => backend::MOONSHINE,
            Self::Nemotron { .. } => backend::NEMOTRON,
            Self::ParakeetMlx { .. } => backend::PARAKEET_MLX,
        }
    }
}

/// Names an engine under `transcription.backend`, and labels that engine in
/// its own logs and errors.
pub mod backend {
    pub const SERVER: &str = "server";
    pub const WHISPER: &str = aria_stt::whisper::NAME;
    pub const WHISPER_CANDLE: &str = aria_stt::whisper_candle::NAME;
    pub const PARAKEET: &str = aria_stt::parakeet::NAME;
    pub const MOONSHINE: &str = aria_stt::moonshine::NAME;
    pub const NEMOTRON: &str = aria_stt::nemotron::NAME;
    // iOS cannot spawn the sidecar, so the app leaves its feature disabled
    // and restates the name for config parsing.
    #[cfg(not(target_os = "ios"))]
    pub const PARAKEET_MLX: &str = aria_stt::parakeet_mlx::NAME;
    #[cfg(target_os = "ios")]
    pub const PARAKEET_MLX: &str = "parakeet-mlx";
    pub const DEFAULT: &str = PARAKEET;

    /// The canonical name of a local engine, or `None`.
    pub(crate) fn local(name: &str) -> Option<&'static str> {
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

pub use aria_stt::parakeet::Device as TranscriptionDevice;

use crate::audio::ReplayPace;

/// Audio source for dictation recording. `WavFile` replays a fixed recording and
/// exists for unattended benchmarking and testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureConfig {
    Microphone,
    WavFile { path: PathBuf, pace: ReplayPace },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictationTimeouts {
    pub(crate) recording: Duration,
    pub(crate) delivery: Duration,
    pub(crate) overall: Duration,
}

// A fault in the file is named by path. A fault in one setting is named by its
// field, so the message points at what to edit.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown assistant {0:?}; use aria assistant --help to list choices")]
    UnknownAssistant(String),

    #[error("transcription.server must be an HTTPS origin without credentials, query or fragment")]
    InvalidServerEndpoint,

    #[error("transcription.server requires transcription.backend: server")]
    ServerRequiresServerBackend,

    #[error(
        "listen requires a local engine; set listen.backend to parakeet, moonshine, or nemotron"
    )]
    ListenRequiresLocal,
    #[error("cannot locate the Aria configuration")]
    Home(#[source] UserHomeError),

    #[error("failed to open Aria configuration at {}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to inspect Aria configuration at {}", path.display())]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Aria configuration path is not a regular file: {}", path.display())]
    NotRegularFile { path: PathBuf },

    #[error("Aria configuration exceeds the maximum size of {maximum_bytes} bytes")]
    FileTooLarge { maximum_bytes: u64 },

    #[error("failed to read Aria configuration at {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Aria configuration is not valid UTF-8")]
    InvalidUtf8(#[source] std::string::FromUtf8Error),

    #[error("failed to parse Aria configuration at {}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: ConfigParseError,
    },

    #[error("environment variable {variable} is not valid UTF-8")]
    InvalidEnvironmentEncoding { variable: &'static str },

    #[error("environment variable {variable} must be a number")]
    InvalidEnvironmentNumber { variable: &'static str },

    #[error("{field} must not be empty")]
    EmptyValue { field: &'static str },

    #[error("{field} must be a finite number of seconds greater than zero; got {seconds}")]
    InvalidDuration { field: &'static str, seconds: f64 },

    #[error("{field} does not accept {value:?}; supported values are {supported}")]
    UnsupportedValue {
        field: &'static str,
        value: String,
        supported: &'static str,
    },

    #[error("chat.timeout must be at most {maximum:?}; got {actual:?}")]
    ChatTimeoutTooLong { actual: Duration, maximum: Duration },

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

    #[error("dictation_cleanup_llm requires dictation_cleanup to be enabled")]
    CleanupLlmRequiresCleanup,

    #[error("invalid dictation_cleanup_llm configuration")]
    CleanupLlm(#[source] crate::dictation::CleanupError),

    #[error("{field} must be finite, greater than zero, and at most one; got {value}")]
    InvalidThreshold { field: &'static str, value: f32 },

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

    #[error("path uses unsupported home syntax: {0}")]
    UnsupportedHomeSyntax(String),

    #[error("dictation_timeouts.overall must be at most {maximum:?}; got {actual:?}")]
    OverallTimeoutTooLong { actual: Duration, maximum: Duration },

    #[error(
        "dictation_timeouts.recording ({recording:?}) must be shorter than dictation_timeouts.overall ({overall:?})"
    )]
    RecordingTimeoutTooLong {
        recording: Duration,
        overall: Duration,
    },
}

/// A redacted YAML error that remains safe to include in daemon diagnostics.
pub struct ConfigParseError {
    location: Option<(u64, u64)>,
    unknown_field: Option<String>,
}

impl ConfigParseError {
    pub(super) fn from_yaml(error: &serde_saphyr::Error) -> Self {
        let location = error
            .location()
            .map(|location| (location.line(), location.column()));
        let unknown_field = unknown_field(error).map(str::to_owned);
        Self {
            location,
            unknown_field,
        }
    }
}

/// Fields the configuration no longer accepts, each with the field that
/// replaced it, so the error names the fix instead of only the rejection.
const RENAMED_FIELDS: [(&str, &str); 1] = [("agent", "chat.assistant")];

fn replacement(field: &str) -> Option<&'static str> {
    RENAMED_FIELDS
        .iter()
        .find(|(renamed, _)| *renamed == field)
        .map(|(_, replacement)| *replacement)
}

fn unknown_field(error: &serde_saphyr::Error) -> Option<&str> {
    match error {
        serde_saphyr::Error::SerdeUnknownField { field, .. } => Some(field),
        serde_saphyr::Error::WithSnippet { error, .. } => unknown_field(error),
        _ => None,
    }
}

impl fmt::Debug for ConfigParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigParseError")
            .field("location", &self.location)
            .field("unknown_field", &self.unknown_field)
            .finish()
    }
}

impl fmt::Display for ConfigParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.unknown_field, self.location) {
            (Some(field), Some((line, column))) => {
                write!(
                    formatter,
                    "unknown configuration field {field:?} at line {line}, column {column}"
                )?;
            }
            (Some(field), None) => write!(formatter, "unknown configuration field {field:?}")?,
            (None, Some((line, column))) => {
                write!(formatter, "invalid YAML at line {line}, column {column}")?;
            }
            (None, None) => formatter.write_str("invalid YAML")?,
        }
        if let Some(replacement) = self.unknown_field.as_deref().and_then(replacement) {
            write!(formatter, "; it was renamed to {replacement:?}")?;
        }
        Ok(())
    }
}

impl Error for ConfigParseError {}
