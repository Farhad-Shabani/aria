//! In-process transcription through candle's Whisper implementation.
//!
//! Whisper in pure Rust, which is what makes this the one engine that reaches
//! a browser: the others bind a native runtime with no wasm build. It loads
//! safetensors from memory, so a caller without a filesystem hands over bytes.
//!
//! ```no_run
//! use aria_stt::{whisper_candle::WhisperCandle, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let transcription = Transcription::complete_samples(WhisperCandle::load("model")?)
//!     .with(RemoveHesitations)
//!     .with(Vocabulary::new(["aria-stt", "Cargo.toml"])?);
//! let session = Session::microphone(&transcription, &Language::Automatic)?;
//!
//! // ... the speaker talks ...
//!
//! let transcript = session.finish()?;
//! # Ok(())
//! # }
//! ```

mod decode;
// A model directory needs a filesystem, which a browser does not have.
#[cfg(not(target_family = "wasm"))]
mod directory;
mod mel;

use std::sync::Mutex;

use candle_core::{DType, Device};
use candle_nn::VarBuilder;
use candle_transformers::models::whisper::{Config, model::Whisper};
use tokenizers::Tokenizer;

use aria_kit::backend::{BackendError, ModelDownload, Unpack};
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{
    Hypothesis, Language, Transcriber, TranscriptionAudio, TranscriptionError, check_cancelled,
};

#[cfg(not(target_family = "wasm"))]
pub use directory::WhisperCandleConfig;

/// Names this engine under `stt.backend` and in its own errors.
pub const NAME: &str = "whisper-candle";

const BACKEND: &str = NAME;

const WEIGHTS_FILE: &str = "model.safetensors";
const TOKENIZER_FILE: &str = "tokenizer.json";
const CONFIG_FILE: &str = "config.json";

/// tiny.en, as a Hugging Face Whisper export.
pub const MODEL: &[ModelDownload] = &[
    ModelDownload {
        file_name: WEIGHTS_FILE,
        url: "https://huggingface.co/openai/whisper-tiny.en/resolve/87c7102498dcde7456f24cfd30239ca606ed9063/model.safetensors",
        bytes: 151_060_136,
        sha256: "db59695928ded6043adaef491a53ef4e12da9611184d77c53baa691a60b958ad",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: TOKENIZER_FILE,
        url: "https://huggingface.co/openai/whisper-tiny.en/resolve/87c7102498dcde7456f24cfd30239ca606ed9063/tokenizer.json",
        bytes: 2_405_679,
        sha256: "5eb60cec1e77aeeb6869a2bb5a8e01a84c3fe5d072d75369343021fe6f5310d0",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: CONFIG_FILE,
        url: "https://huggingface.co/openai/whisper-tiny.en/resolve/87c7102498dcde7456f24cfd30239ca606ed9063/config.json",
        bytes: 1_937,
        sha256: "b5366317df280f79e6cc366f259f379674f81a8721d458acf2e00680ccb10b1d",
        unpack: Unpack::Plain,
    },
];

/// The mel filter bank Whisper was trained against, as raw little-endian
/// `f32`.
const MEL_FILTERS: &[u8] = include_bytes!("melfilters.bytes");

// 80 bands covers every model up to large-v2. large-v3 wants 128, which is a
// second filter file.
const MEL_BANDS: usize = 80;

pub struct WhisperCandle {
    /// One resident model. Decoding mutates its key-value cache, so a
    /// transcription holds it for the whole utterance.
    model: Mutex<Whisper>,
    tokenizer: Tokenizer,
    config: Config,
    mel_filters: Vec<f32>,
}

impl WhisperCandle {
    /// Loads a model from bytes the caller already holds — the only load path
    /// a browser has, where the weights arrive over the network.
    ///
    /// `weights` is safetensors; `tokenizer` and `config` are the JSON files
    /// that ship beside them.
    pub fn from_bytes(
        weights: &[u8],
        tokenizer: &[u8],
        config: &[u8],
    ) -> Result<Self, TranscriptionError> {
        let config: Config = serde_json::from_slice(config).map_err(load_failed)?;
        if config.num_mel_bins != MEL_BANDS {
            return Err(load_failed(EngineError(format!(
                "the model wants {} mel bands; only {MEL_BANDS} ship with this backend",
                config.num_mel_bins
            ))));
        }
        let tokenizer =
            Tokenizer::from_bytes(tokenizer).map_err(|error| load_failed(engine(&*error)))?;

        // Borrows the caller's bytes rather than copying them: a model is
        // tens of megabytes and wasm has one address space to spend.
        let variables = VarBuilder::from_slice_safetensors(weights, DType::F32, &Device::Cpu)
            .map_err(load_failed)?;
        let model = Whisper::load(&variables, config.clone()).map_err(load_failed)?;

        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            config,
            mel_filters: decode_mel_filters(),
        })
    }
}

impl Transcriber for WhisperCandle {
    fn validate_language(
        &self,
        language: &Language,
        _cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        decode::Prompt::new(&self.tokenizer, &self.config, language).map(|_prompt| ())
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        check_cancelled(cancel)?;
        let samples = audio.require_samples(BACKEND)?;
        let prompt = decode::Prompt::new(&self.tokenizer, &self.config, language)?;
        let mel = mel::spectrogram(samples, &self.mel_filters, MEL_BANDS);
        let spoken = mel::spoken_frames(samples.len());

        let mut model = self
            .model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = decode::all_windows(&mut model, &self.tokenizer, &prompt, &mel, spoken, cancel)?;
        Ok(Hypothesis::new(text))
    }
}

fn decode_mel_filters() -> Vec<f32> {
    MEL_FILTERS
        .as_chunks()
        .0
        .iter()
        .copied()
        .map(f32::from_le_bytes)
        .collect()
}

fn load_failed(source: impl std::error::Error + Send + Sync + 'static) -> TranscriptionError {
    TranscriptionError::Load {
        backend: BACKEND,
        model: "safetensors".to_owned(),
        source: BackendError::engine(source),
    }
}

fn inference_failed(source: impl std::error::Error + Send + Sync + 'static) -> TranscriptionError {
    TranscriptionError::Inference {
        backend: BACKEND,
        source: BackendError::engine(source),
    }
}

/// Carries a message from a dependency whose error type is a boxed trait
/// object, which [`BackendError::engine`] cannot take.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct EngineError(String);

fn engine(error: &dyn std::error::Error) -> EngineError {
    EngineError(aria_kit::primitives::error_chain(error))
}
