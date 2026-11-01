//! Immutable configuration for Aria capabilities.

mod load;

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use aria_kit::synthesis::{SpeakingRate, SpeakingRateError, Voice, VoiceError};

use crate::primitives::user_home::UserHomeError;
use crate::settings::SettingsError;

pub(crate) struct DaemonConfig {
    pub(crate) dictation: DictationConfig,
    pub(crate) wake: WakeConfig,
    pub(crate) hotkey: HotkeyConfig,
    pub(crate) relay: RelayConfig,
    pub(crate) tts: TtsConfig,
}

impl From<crate::settings::SettingsError> for ConfigError {
    fn from(error: crate::settings::SettingsError) -> Self {
        Self::Settings(error)
    }
}

/// A loopback LLM pass over the transcript, after the rule-based cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LlmCleanupConfig {
    pub(crate) endpoint: String,
    pub(crate) model: String,
}

/// The command's say over `wake.enabled`, outranking the file. `aria wake`
/// exists to listen, so it enables monitoring whatever the settings say.
/// Nothing here can turn monitoring off.
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
    pub(crate) cleanup_llm: Option<LlmCleanupConfig>,
}

/// The global dictation hotkey: one bare modifier key, tapped or held. Which
/// key lives in the settings.
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

/// The engine a daemon builds, and what it falls back to if that fails.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TtsConfig {
    pub(crate) backend: TtsBackend,
    pub(crate) fallback: TtsFallback,
}

/// An engine that speaks, which owns its saved voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TtsEngine {
    Kokoro,
    Qwen,
    System,
}

/// What an utterance falls back to when its request names no voice or rate.
/// Live keys, so each utterance reads them anew.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpeechDefaults {
    /// `None` leaves the choice to the engine.
    pub(crate) voice: Option<Voice>,
    pub(crate) rate: SpeakingRate,
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
    pub(crate) fn backend_name(&self) -> &'static str {
        match self {
            Self::Local(engine) => engine.backend_name(),
            Self::Server { .. } => backend::SERVER,
        }
    }
}

/// Where and how long `aria ask` runs the assistant. Loaded per ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatConfig {
    /// Pins the assistant's working directory; otherwise the caller's.
    pub(crate) workdir: Option<PathBuf>,
    /// Directories whose git repos an ask may name; empty turns routing off.
    pub(crate) project_roots: Vec<PathBuf>,
    pub(crate) timeout: Duration,
    /// A reply delivered like a transcript gets dictation's delivery timeout.
    pub(crate) delivery_timeout: Duration,
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
    pub(crate) profiles_dir: PathBuf,
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

pub(crate) use load::{
    DEFAULT_OVERALL_TIMEOUT, DEFAULT_RECORDING_TIMEOUT, LoadedModel, UNATTENDED_ENV, codex_home,
    home_directory, loaded_models, memory_directory, model_folder, models_dir,
    transcription_engine, unattended,
};

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

/// Names an engine under `stt.backend`, and labels that engine in
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
