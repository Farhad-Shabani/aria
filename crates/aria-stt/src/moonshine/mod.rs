//! In-process transcription through the Moonshine ONNX graphs.
//!
//! Moonshine reads raw audio rather than a spectrogram, so a recording reaches
//! the model as the samples it was captured as. What it returns is token ids,
//! which the vocabulary reads back as text.
//!
//! ```no_run
//! use aria_stt::{moonshine::Moonshine, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let transcription = Transcription::streaming(Moonshine::load("model")?)
//!     .with(RemoveHesitations)
//!     .with(Vocabulary::new(["aria-stt", "Cargo.toml"])?);
//! let mut session = Session::microphone(&transcription, &Language::Automatic)?;
//!
//! // Committed words arrive as you speak and are never revised.
//! while let Some(words) = session.next_finalized()? {
//!     print!("{}", words.as_str());
//! }
//!
//! let transcript = session.stop()?;
//! # Ok(())
//! # }
//! ```
//!
//! For audio you already hold, [`crate::Transcription::transcribe_file`] decodes a
//! WAV in one call.

mod recognizer;
mod vocabulary;

use std::path::PathBuf;
use std::sync::Mutex;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{
    AudioInputKind, Cadence, Engine, Hypothesis, Language, Stream, StreamingEngine, Transcriber,
    TranscriptionAudio, TranscriptionError, check_cancelled, english_only,
    validate_model_directory,
};

use recognizer::{MIN_SAMPLES, Recognizer};

/// Names this engine under `transcription.backend` and in its own errors.
pub const NAME: &str = "moonshine";

const BACKEND: &str = NAME;

/// The 750 ms stride gives the decoder more context before words are committed.
/// The twenty-second window caps repeated decoding cost and token usage.
pub const CADENCE: Cadence = Cadence::from_millis(750, 20000);

pub struct Moonshine {
    recognizer: Mutex<Recognizer>,
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
    pub fn load(directory: impl Into<PathBuf>) -> Result<Self, TranscriptionError> {
        Self::load_from(MoonshineConfig::new(directory), &CancelToken::never())
    }
}

impl Engine for Moonshine {
    const NAME: &'static str = NAME;
    const INPUT: AudioInputKind = AudioInputKind::Samples;

    type Config = MoonshineConfig;

    fn check_from(config: &MoonshineConfig) -> Result<PathBuf, TranscriptionError> {
        validate_model_directory(
            config.model_directory.clone(),
            BACKEND,
            &[
                vocabulary::TOKENIZER_FILE,
                recognizer::ENCODER_FILE,
                recognizer::DECODER_FILE,
            ],
        )
    }

    fn load_from(
        config: MoonshineConfig,
        cancel: &CancelToken,
    ) -> Result<Self, TranscriptionError> {
        check_cancelled(cancel)?;
        let directory = Self::check_from(&config)?;
        let vocabulary = vocabulary::Vocabulary::load(&directory).map_err(|source| {
            TranscriptionError::Model {
                backend: BACKEND,
                source,
            }
        })?;
        let recognizer = Recognizer::load(&directory)?;
        check_cancelled(cancel)?;
        Ok(Self {
            recognizer: Mutex::new(recognizer),
            vocabulary,
        })
    }
}

impl StreamingEngine for Moonshine {
    const STREAM: Stream = Stream::Agreement(CADENCE);
}

impl Transcriber for Moonshine {
    fn validate_language(
        &self,
        language: &Language,
        _cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        english_only(BACKEND, language)
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        _language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        check_cancelled(cancel)?;
        let samples = audio.require_samples(BACKEND)?;
        let mut recognizer = self
            .recognizer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        decode_window(&mut recognizer, &self.vocabulary, samples, cancel).map(Hypothesis::new)
    }
}

/// What one window said.
fn decode_window(
    recognizer: &mut Recognizer,
    vocabulary: &vocabulary::Vocabulary,
    window: &[f32],
    cancel: &CancelToken,
) -> Result<String, TranscriptionError> {
    // Too brief to fill the encoder, so there is nothing to find: heard as
    // silence rather than run through the model.
    if window.len() < MIN_SAMPLES {
        return Ok(String::new());
    }
    let tokens = recognizer.transcribe(window, cancel)?;
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

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
    }
}
