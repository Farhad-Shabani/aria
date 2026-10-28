//! In-process transcription through the Parakeet TDT ONNX bindings.
//!
//! ```no_run
//! use aria_stt::{parakeet::{Device, Parakeet}, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let transcription = Transcription::streaming(Parakeet::load("model", Device::Cpu)?)
//!     .with(RemoveHesitations)
//!     .with(Vocabulary::new(["aria-stt", "Cargo.toml"])?);
//! let mut session = Session::microphone(&transcription, &Language::Automatic)?;
//!
//! // Committed words arrive as you speak and are never revised.
//! while let Some(words) = session.next_finalized()? {
//!     print!("{}", words.as_str());
//! }
//!
//! let transcript = session.finish()?;
//! # Ok(())
//! # }
//! ```
//!
//! For audio you already hold, [`crate::Transcription::transcribe_file`] decodes a
//! WAV in one call. Streaming runs at [`CADENCE`].

use std::path::PathBuf;
use std::sync::Mutex;

#[cfg(all(target_os = "macos", feature = "parakeet-gpu"))]
use parakeet_rs::ExecutionProvider;
use parakeet_rs::{ExecutionConfig, ParakeetTDT, TimedToken, TimestampMode, Transcriber as _};

use aria_kit::audio::{CANONICAL_SAMPLE_RATE, CHANNELS};
use aria_kit::backend::{BackendError, ModelDownload, Unpack};
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{
    AudioInputKind, Cadence, Engine, Hypothesis, Language, Stream, StreamingEngine, TimedWord,
    Transcriber, TranscriptionAudio, TranscriptionError, check_cancelled, validate_model_directory,
    word_timings,
};

/// Names this engine under `transcription.backend` and in its own errors.
pub const NAME: &str = "parakeet";

/// Where inference runs. Gpu means WebGPU (Metal), which needs macOS and the
/// `parakeet-gpu` feature; without them a load refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    Gpu,
}

const BACKEND: &str = NAME;
const VOCABULARY_FILE: &str = "vocab.txt";

/// TDT 0.6B v3 at full precision. parakeet-rs finds the encoder and decoder
/// among several export names, int8 included, so a load requires only the
/// vocabulary by name.
pub const MODEL: &[ModelDownload] = &[
    ModelDownload {
        file_name: "encoder-model.onnx",
        url: "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/encoder-model.onnx",
        bytes: 41_770_866,
        sha256: "98a74b21b4cc0017c1e7030319a4a96f4a9506e50f0708f3a516d02a77c96bb1",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: "encoder-model.onnx.data",
        url: "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/encoder-model.onnx.data",
        bytes: 2_435_420_160,
        sha256: "9a22d372c51455c34f13405da2520baefb7125bd16981397561423ed32d24f36",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: "decoder_joint-model.onnx",
        url: "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/decoder_joint-model.onnx",
        bytes: 72_520_893,
        sha256: "e978ddf6688527182c10fde2eb4b83068421648985ef23f7a86be732be8706c1",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: VOCABULARY_FILE,
        url: "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/vocab.txt",
        bytes: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
        unpack: Unpack::Plain,
    },
];

/// A word commits when two decodes of the growing buffer agree on it.
/// The twenty-second cap bounds re-decode cost.
pub const CADENCE: Cadence = Cadence::from_millis(500, 20000);

pub struct Parakeet {
    model: Mutex<Box<ParakeetTDT>>,
}

#[derive(Clone)]
pub struct ParakeetConfig {
    pub model_directory: PathBuf,
    pub device: Device,
}

impl ParakeetConfig {
    pub fn new(model_directory: impl Into<PathBuf>, device: Device) -> Self {
        Self {
            model_directory: model_directory.into(),
            device,
        }
    }
}

impl Parakeet {
    pub fn load(directory: impl Into<PathBuf>, device: Device) -> Result<Self, TranscriptionError> {
        Self::load_from(
            ParakeetConfig::new(directory, device),
            &CancelToken::never(),
        )
    }
}

impl Engine for Parakeet {
    const NAME: &'static str = NAME;
    const MODEL: &'static [ModelDownload] = MODEL;
    const INPUT: AudioInputKind = AudioInputKind::Samples;

    type Config = ParakeetConfig;

    fn check_from(config: &ParakeetConfig) -> Result<PathBuf, TranscriptionError> {
        validate_model_directory(config.model_directory.clone(), BACKEND, &[VOCABULARY_FILE])
    }

    fn load_from(config: ParakeetConfig, cancel: &CancelToken) -> Result<Self, TranscriptionError> {
        check_cancelled(cancel)?;
        let directory = Self::check_from(&config)?;

        let execution = Some(execution(config.device)?);
        let model = ParakeetTDT::from_pretrained(&directory, execution).map_err(|source| {
            TranscriptionError::Load {
                backend: BACKEND,
                model: directory.display().to_string(),
                source: BackendError::engine(source),
            }
        })?;
        check_cancelled(cancel)?;
        Ok(Self {
            model: Mutex::new(Box::new(model)),
        })
    }
}

impl StreamingEngine for Parakeet {
    const STREAM: Stream = Stream::Agreement(CADENCE);
}

impl Transcriber for Parakeet {
    /// Accepts any language: the engine takes no language input.
    fn validate_language(
        &self,
        _language: &Language,
        _cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        Ok(())
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        _language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        check_cancelled(cancel)?;
        let samples = audio.require_samples(BACKEND)?;
        let mut model = self
            .model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (text, tokens) = decode_window(&mut model, samples)?;
        check_cancelled(cancel)?;
        let words = word_timings(tokens.iter().map(|token| {
            let timing = TimedWord {
                start: token.start,
                end: token.end,
            };
            (token.text.as_str(), timing)
        }));
        Ok(Hypothesis::with_words(text, words))
    }
}

// Use available CPU parallelism instead of the bindings' four-thread default.
fn execution(device: Device) -> Result<ExecutionConfig, TranscriptionError> {
    let threads = std::thread::available_parallelism().map_or(4, usize::from);
    let config = ExecutionConfig::new().with_intra_threads(threads);
    match device {
        Device::Cpu => Ok(config),
        #[cfg(all(target_os = "macos", feature = "parakeet-gpu"))]
        Device::Gpu => Ok(config.with_execution_provider(ExecutionProvider::WebGPU)),
        #[cfg(not(all(target_os = "macos", feature = "parakeet-gpu")))]
        Device::Gpu => Err(TranscriptionError::UnsupportedDevice { backend: BACKEND }),
    }
}

/// What one window said. The bindings decode owned samples, so the window is
/// copied out of the recording. Token mode keeps the text identical to an
/// untimed decode — word mode would rebuild it.
fn decode_window(
    model: &mut ParakeetTDT,
    window: &[f32],
) -> Result<(String, Vec<TimedToken>), TranscriptionError> {
    model
        .transcribe_samples(
            window.to_vec(),
            CANONICAL_SAMPLE_RATE,
            CHANNELS,
            Some(TimestampMode::Tokens),
        )
        .map(|decoded| (decoded.text, decoded.tokens))
        .map_err(|source| TranscriptionError::Inference {
            backend: BACKEND,
            source: BackendError::engine(source),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_load_stops_before_model_validation() {
        let cancel = CancelToken::new();
        cancel.cancel();

        let outcome = Parakeet::load_from(
            ParakeetConfig::new("/nonexistent/model", Device::Cpu),
            &cancel,
        );

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
    }
}
