//! In-process recognition through the Moonshine ONNX graphs.
//!
//! Moonshine reads raw audio rather than a spectrogram, so a recording reaches
//! the model as the samples it was captured as. What it returns is token ids,
//! which the vocabulary reads back as text.
//!
//! ```no_run
//! use aria_stt::{moonshine::Moonshine, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let recognition = Recognition::streaming(Moonshine::load("model")?)
//!     .with(RemoveHesitations)
//!     .with(Vocabulary::new(["aria-stt", "Cargo.toml"])?);
//! let mut session = Session::microphone(&recognition, &Language::Automatic)?;
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
//! For audio you already hold, [`crate::Recognition::recognize_file`] decodes a
//! WAV in one call.

mod graphs;
mod vocabulary;

use std::path::PathBuf;
use std::sync::Mutex;

use aria_kit::backend::{ModelDownload, Unpack};
use aria_kit::cancel::CancelToken;
use aria_kit::recognition::{
    AudioInputKind, Cadence, Engine, Hypothesis, Language, RecognitionAudio, RecognitionError,
    Recognizer, Stream, StreamingEngine, check_cancelled, english_only, validate_model_directory,
};

use graphs::{Graphs, MIN_SAMPLES};

/// Names this engine under `stt.backend` and in its own errors.
pub const NAME: &str = "moonshine";

const BACKEND: &str = NAME;

/// The base model, its encoder and decoder quantized to 8 bits.
pub const MODEL: &[ModelDownload] = &[
    ModelDownload {
        file_name: graphs::ENCODER_FILE,
        url: "https://huggingface.co/onnx-community/moonshine-base-ONNX/resolve/b1e9b6aae3c3c7298f10c3798393fdf38e8fbbad/onnx/encoder_model_quantized.onnx",
        bytes: 20_513_063,
        sha256: "1dd9ab0a7f987113d30affcba5a068d11c8f90fa0223caa3e491ade431ad9751",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: graphs::DECODER_FILE,
        url: "https://huggingface.co/onnx-community/moonshine-base-ONNX/resolve/b1e9b6aae3c3c7298f10c3798393fdf38e8fbbad/onnx/decoder_model_merged_quantized.onnx",
        bytes: 42_498_870,
        sha256: "cc9f3cd6698a369c6008b41aa60aa3fb3322e7f03c9bdf19d8e6b7200afca4f3",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: vocabulary::TOKENIZER_FILE,
        url: "https://huggingface.co/onnx-community/moonshine-base-ONNX/resolve/b1e9b6aae3c3c7298f10c3798393fdf38e8fbbad/tokenizer.json",
        bytes: 3_761_754,
        sha256: "7b913404bdd039af4756783218af4440bc07fb7d6d8258d677e34f95b3ec416f",
        unpack: Unpack::Plain,
    },
];

/// The 750 ms stride gives the decoder more context before words are committed.
/// The twenty-second window caps repeated decoding cost and token usage.
pub const CADENCE: Cadence = Cadence::from_millis(750, 20000);

pub struct Moonshine {
    graphs: Mutex<Graphs>,
    vocabulary: vocabulary::Vocabulary,
}

#[derive(Clone)]
pub struct MoonshineConfig {
    pub model_directory: PathBuf,
}

impl MoonshineConfig {
    pub fn new(model_directory: impl Into<PathBuf>) -> Self {
        Self {
            model_directory: model_directory.into(),
        }
    }
}

impl Moonshine {
    pub fn load(directory: impl Into<PathBuf>) -> Result<Self, RecognitionError> {
        Self::load_from(MoonshineConfig::new(directory), &CancelToken::never())
    }
}

impl Engine for Moonshine {
    const NAME: &'static str = NAME;
    const MODEL: &'static [ModelDownload] = MODEL;
    const INPUT: AudioInputKind = AudioInputKind::Samples;

    type Config = MoonshineConfig;

    fn check_from(config: &MoonshineConfig) -> Result<PathBuf, RecognitionError> {
        validate_model_directory(
            config.model_directory.clone(),
            BACKEND,
            &[
                vocabulary::TOKENIZER_FILE,
                graphs::ENCODER_FILE,
                graphs::DECODER_FILE,
            ],
        )
    }

    fn load_from(config: MoonshineConfig, cancel: &CancelToken) -> Result<Self, RecognitionError> {
        check_cancelled(cancel)?;
        let directory = Self::check_from(&config)?;
        let vocabulary =
            vocabulary::Vocabulary::load(&directory).map_err(|source| RecognitionError::Model {
                backend: BACKEND,
                source,
            })?;
        let graphs = Graphs::load(&directory)?;
        check_cancelled(cancel)?;
        Ok(Self {
            graphs: Mutex::new(graphs),
            vocabulary,
        })
    }
}

impl StreamingEngine for Moonshine {
    const STREAM: Stream = Stream::Agreement(CADENCE);
}

impl Recognizer for Moonshine {
    fn validate_language(
        &self,
        language: &Language,
        _cancel: &CancelToken,
    ) -> Result<(), RecognitionError> {
        english_only(BACKEND, language)
    }

    fn recognize_with(
        &self,
        audio: RecognitionAudio<'_>,
        _language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, RecognitionError> {
        check_cancelled(cancel)?;
        let samples = audio.require_samples(BACKEND)?;
        let mut graphs = self
            .graphs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        decode_window(&mut graphs, &self.vocabulary, samples, cancel).map(Hypothesis::new)
    }
}

/// What one window said.
fn decode_window(
    graphs: &mut Graphs,
    vocabulary: &vocabulary::Vocabulary,
    window: &[f32],
    cancel: &CancelToken,
) -> Result<String, RecognitionError> {
    // Too brief to fill the encoder, so there is nothing to find: heard as
    // silence rather than run through the model.
    if window.len() < MIN_SAMPLES {
        return Ok(String::new());
    }
    let tokens = graphs.recognize(window, cancel)?;
    Ok(vocabulary.decode(&tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_load_stops_before_model_validation() {
        let cancel = CancelToken::new();
        cancel.cancel();

        let outcome = Moonshine::load_from(MoonshineConfig::new("/nonexistent/model"), &cancel);

        assert!(matches!(outcome, Err(RecognitionError::Cancelled)));
    }
}
