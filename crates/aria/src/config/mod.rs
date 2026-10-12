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
    pub(crate) transcription: TranscriptionConfig,
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
    Kokoro {
        model: PathBuf,
        voices: PathBuf,
    },
    Qwen {
        model_dir: PathBuf,
        tokenizer_dir: PathBuf,
    },
    System,
}

impl TtsBackend {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Kokoro { .. } => tts_backend::KOKORO,
            Self::Qwen { .. } => tts_backend::QWEN,
            Self::System => tts_backend::SYSTEM,
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
}

impl DictationConfig {
    pub(crate) fn load(environment: &Environment) -> Result<Self, ConfigError> {
        load::load(environment)
    }

    /// The LLM cleanup layer this configuration asks for, if any.
    pub(crate) fn cleanup_layer(
        &self,
        home: &std::path::Path,
    ) -> Result<Option<crate::dictation::LlmCleanup>, crate::dictation::CleanupError> {
        self.cleanup_llm
            .as_ref()
            .map(|cleanup| {
                crate::dictation::LlmCleanup::new(&cleanup.endpoint, &cleanup.model, home)
            })
            .transpose()
    }
}

/// The agent `aria ask` talks to. Loaded per ask, never at daemon startup,
/// so a broken chat section blocks nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatConfig {
    pub(crate) agent: ChatAgent,
    /// Pins the agent's working directory; otherwise the caller's.
    pub(crate) workdir: Option<PathBuf>,
    /// Directories whose git repos an ask may name; empty turns routing off.
    pub(crate) project_roots: Vec<PathBuf>,
    pub(crate) timeout: Duration,
    /// A reply delivered like a transcript gets dictation's delivery timeout.
    pub(crate) delivery_timeout: Duration,
}

impl ChatConfig {
    pub(crate) fn load(environment: &Environment) -> Result<Self, ConfigError> {
        load::load_chat(environment)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatAgent {
    ClaudeCode,
    Codex,
}

impl ChatAgent {
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => chat_agent::CLAUDE_CODE,
            Self::Codex => chat_agent::CODEX,
        }
    }
}

/// Names an agent under `chat.agent`, and labels it in the conversation
/// record so a switch starts a fresh one.
pub mod chat_agent {
    pub const CLAUDE_CODE: &str = "claude-code";
    pub const CODEX: &str = "codex";
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
    Environment, UNATTENDED_ENV, default_model_path, default_path, memory_directory,
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
                )
            }
            (Some(field), None) => write!(formatter, "unknown configuration field {field:?}"),
            (None, Some((line, column))) => {
                write!(formatter, "invalid YAML at line {line}, column {column}")
            }
            (None, None) => formatter.write_str("invalid YAML"),
        }
    }
}

impl Error for ConfigParseError {}
