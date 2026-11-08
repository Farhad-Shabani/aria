//! Immutable configuration for Aria capabilities.

mod load;

use std::path::PathBuf;

use aria_kit::synthesis::{SpeakingRate, Voice};
use aria_settings::config::{ConfigError, backend, tts_backend};
use aria_settings::{DictationTimeouts, LlmCleanupTarget};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictationConfig {
    pub capture: CaptureConfig,
    pub transcription: TranscriptionBackend,
    pub dictation_timeouts: DictationTimeouts,
    /// A loopback LLM pass over the transcript, after the rule-based cleanup.
    pub cleanup_llm: Option<LlmCleanupTarget>,
}

/// The engine a daemon builds, and what it falls back to if that fails.
#[derive(Debug, Clone, PartialEq)]
pub struct TtsConfig {
    pub backend: TtsBackend,
    pub fallback: TtsFallback,
}

/// What an utterance falls back to when its request names no voice or rate.
/// Live keys, so each utterance reads them anew.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechDefaults {
    /// `None` leaves the choice to the engine.
    pub voice: Option<Voice>,
    pub rate: SpeakingRate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsBackend {
    Model(TtsModel),
    System,
}

/// An engine that synthesizes PCM from model files on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsModel {
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
    pub fn name(&self) -> &'static str {
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
pub enum TtsFallback {
    None,
    System,
}

/// Where dictation and recorded transcription run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionBackend {
    Local(TranscriptionConfig),
    /// The paired server, or `None` before any pairing.
    Server(Option<PairingId>),
}

/// Which pairing a server engine is built for, opaque to everything but the
/// pairing that makes it. Re-pairing changes it even at the same address, so
/// an engine built on an older pairing is replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingId([u8; 32]);

impl PairingId {
    pub fn new(id: [u8; 32]) -> Self {
        Self(id)
    }
}

impl TranscriptionBackend {
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Local(engine) => engine.backend_name(),
            Self::Server(_) => backend::SERVER,
        }
    }
}

/// System transcription: system-audio capture transcribed to a file. Loaded per Start,
/// never at daemon startup, so a broken transcription section blocks nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTranscriptionConfig {
    pub transcription: SystemTranscriptionEngine,
    pub transcripts_dir: PathBuf,
}

/// An engine that transcribes a live stream. Only a config whose engine
/// streams becomes one, so holding it proves the engine streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTranscriptionEngine(TranscriptionConfig);

impl SystemTranscriptionEngine {
    pub(crate) fn new(config: TranscriptionConfig) -> Result<Self, ConfigError> {
        if crate::transcription::streams(&config) {
            Ok(Self(config))
        } else {
            Err(ConfigError::SystemTranscriptionUnusableEngine(
                config.backend_name().to_owned(),
            ))
        }
    }

    pub fn backend_name(&self) -> &'static str {
        self.0.backend_name()
    }

    /// Resolves the model a session would load, without reading it.
    pub fn check_model(&self) -> Result<String, crate::transcription::TranscriptionError> {
        crate::transcription::check_model(&self.0)
    }

    pub(crate) fn config(&self) -> &TranscriptionConfig {
        &self.0
    }
}

/// Where the voice-embedding model and the enrolled profiles live. One
/// enrollment serves every feature that recognizes a speaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerConfig {
    pub model: PathBuf,
    /// Segmentation model path. If unavailable, system transcription labels per segment.
    pub segmentation_model: PathBuf,
    pub profiles_dir: PathBuf,
}

pub use load::{LoadedModel, Section, loaded_models};

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

    pub fn for_backend(&self, backend: &str, home: &std::path::Path) -> Result<Self, ConfigError> {
        if self.backend_name() == backend {
            return Ok(self.clone());
        }
        load::transcription_defaults(backend, home)
    }

    pub fn backend_name(&self) -> &'static str {
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

use aria_kit::audio::ReplayPace;

/// Audio source for dictation recording. `WavFile` replays a fixed recording and
/// exists for unattended benchmarking and testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureConfig {
    Microphone,
    WavFile { path: PathBuf, pace: ReplayPace },
}
