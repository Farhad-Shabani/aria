//! Which engines a run measures, and how each is named in the results.
//!
//! A loaded engine keeps no record of where it came from — the model paths
//! are consumed at load. Identity therefore comes from what an engine was
//! asked to be, named before the model is ever loaded.

use std::path::PathBuf;

// The contracts are called through, never named by this module's own types.
use aria_kit::cancel::CancelToken;
use aria_kit::synthesis::{
    Engine as _, SpeakingRate, SpeechText, SpeechWriter, SynthesisError, Synthesizer as _, Voice,
};
use aria_tts::kokoro::{Config as KokoroConfig, Kokoro, NAME as KOKORO_NAME};
use aria_tts::qwen::engine::Config as QwenConfig;
use aria_tts::qwen::{NAME as QWEN_NAME, Qwen};

/// One engine to measure, named with everything that engine needs.
#[derive(Clone)]
pub enum EngineRequest {
    Qwen {
        model_dir: PathBuf,
        tokenizer_dir: PathBuf,
    },
    Kokoro {
        model: PathBuf,
        voices: PathBuf,
    },
}

impl EngineRequest {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Qwen { .. } => QWEN_NAME,
            Self::Kokoro { .. } => KOKORO_NAME,
        }
    }
}

/// A backend the machine cannot run, and why.
pub struct Skipped {
    pub backend: &'static str,
    /// The top-level reason only; chains can name paths, so they go to the
    /// log instead.
    pub reason: String,
}

pub(super) struct Selection {
    pub name: &'static str,
    config: EngineConfig,
}

enum EngineConfig {
    Qwen(QwenConfig),
    Kokoro(KokoroConfig),
}

impl Selection {
    /// Builds a selection for every request, setting aside those whose
    /// model is missing — a benchmark of what this machine has. The caller
    /// reports what was set aside, so a short run cannot look like a full
    /// one.
    pub(super) fn selections(requested: &[EngineRequest]) -> (Vec<Self>, Vec<Skipped>) {
        let mut selected = Vec::new();
        let mut skipped = Vec::new();
        for request in requested {
            match Self::from_request(request) {
                Ok(selection) => selected.push(selection),
                Err(failure) => {
                    let detail = aria_kit::primitives::error_chain(&failure);
                    tracing::warn!(%detail, "a tts backend cannot be used");
                    skipped.push(Skipped {
                        backend: request.name(),
                        reason: failure.to_string(),
                    });
                }
            }
        }
        (selected, skipped)
    }

    fn from_request(requested: &EngineRequest) -> Result<Self, SynthesisError> {
        let config = match requested {
            EngineRequest::Qwen {
                model_dir,
                tokenizer_dir,
            } => {
                let config = QwenConfig {
                    model_dir: model_dir.clone(),
                    speech_tokenizer_dir: tokenizer_dir.clone(),
                };
                Qwen::check_from(&config)?;
                EngineConfig::Qwen(config)
            }
            EngineRequest::Kokoro { model, voices } => {
                let config = KokoroConfig {
                    model: model.clone(),
                    voices: voices.clone(),
                };
                Kokoro::check_from(&config)?;
                EngineConfig::Kokoro(config)
            }
        };
        Ok(Self {
            name: requested.name(),
            config,
        })
    }

    pub(super) fn load(&self) -> Result<Loaded, SynthesisError> {
        match &self.config {
            EngineConfig::Qwen(config) => {
                Qwen::load_from(config.clone()).map(|qwen| Loaded::Qwen(Box::new(qwen)))
            }
            EngineConfig::Kokoro(config) => {
                Kokoro::load_from(config.clone()).map(|kokoro| Loaded::Kokoro(Box::new(kokoro)))
            }
        }
    }
}

/// The engines a run measures, dispatched by enum rather than a trait.
pub(crate) enum Loaded {
    Qwen(Box<Qwen>),
    Kokoro(Box<Kokoro>),
}

impl Loaded {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Qwen(_) => QWEN_NAME,
            Self::Kokoro(_) => KOKORO_NAME,
        }
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        match self {
            Self::Qwen(engine) => engine.sample_rate(),
            Self::Kokoro(engine) => engine.sample_rate(),
        }
    }

    pub(crate) fn synthesize_with(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        out: SpeechWriter,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError> {
        match self {
            Self::Qwen(engine) => engine.synthesize_with(text, voice, rate, out, cancel),
            Self::Kokoro(engine) => engine.synthesize_with(text, voice, rate, out, cancel),
        }
    }
}
