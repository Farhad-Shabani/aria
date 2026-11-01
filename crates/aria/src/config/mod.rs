//! Immutable configuration for Aria capabilities.

mod load;

use std::path::PathBuf;
use std::time::Duration;

use aria_kit::synthesis::{SpeakingRate, Voice};
use aria_settings::config::{ConfigError, ServerEndpoint, backend, tts_backend, wake_backend};

pub(crate) struct DaemonConfig {
    pub(crate) dictation: DictationConfig,
    pub(crate) wake: WakeConfig,
    pub(crate) hotkey: HotkeyConfig,
    pub(crate) relay: RelayConfig,
    pub(crate) tts: TtsConfig,
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

pub(crate) use load::{LoadedModel, loaded_models};

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
