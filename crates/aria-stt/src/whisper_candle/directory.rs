//! Loading a model from a directory of files, which only a native caller has.

use std::path::{Path, PathBuf};

use aria_kit::backend::{ModelDownload, ModelError};
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{
    AudioInputKind, Engine, TranscriptionError, check_cancelled, validate_model_directory,
};

use super::{BACKEND, CONFIG_FILE, MODEL, NAME, TOKENIZER_FILE, WEIGHTS_FILE, WhisperCandle};

/// Everything a whisper-candle load needs: a directory holding the three
/// files a Hugging Face Whisper export ships.
#[derive(Clone)]
pub struct WhisperCandleConfig {
    pub model_directory: PathBuf,
}

impl WhisperCandleConfig {
    pub fn new(model_directory: impl Into<PathBuf>) -> Self {
        Self {
            model_directory: model_directory.into(),
        }
    }
}

impl WhisperCandle {
    pub fn load(directory: impl Into<PathBuf>) -> Result<Self, TranscriptionError> {
        Self::load_from(WhisperCandleConfig::new(directory), &CancelToken::never())
    }
}

impl Engine for WhisperCandle {
    const NAME: &'static str = NAME;
    const MODEL: &'static [ModelDownload] = MODEL;
    const INPUT: AudioInputKind = AudioInputKind::Samples;

    type Config = WhisperCandleConfig;

    fn check_from(config: &WhisperCandleConfig) -> Result<PathBuf, TranscriptionError> {
        validate_model_directory(
            config.model_directory.clone(),
            BACKEND,
            &[WEIGHTS_FILE, TOKENIZER_FILE, CONFIG_FILE],
        )
    }

    fn load_from(
        config: WhisperCandleConfig,
        cancel: &CancelToken,
    ) -> Result<Self, TranscriptionError> {
        check_cancelled(cancel)?;
        let directory = Self::check_from(&config)?;
        let weights = read(&directory, WEIGHTS_FILE)?;
        let tokenizer = read(&directory, TOKENIZER_FILE)?;
        let settings = read(&directory, CONFIG_FILE)?;
        let engine = Self::from_bytes(&weights, &tokenizer, &settings)?;
        check_cancelled(cancel)?;
        Ok(engine)
    }
}

fn read(directory: &Path, file: &str) -> Result<Vec<u8>, TranscriptionError> {
    let path = directory.join(file);
    std::fs::read(&path).map_err(|source| TranscriptionError::Model {
        backend: BACKEND,
        source: ModelError::Unreadable { path, source },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_error(directory: &Path) -> TranscriptionError {
        match WhisperCandle::load(directory) {
            Err(error) => error,
            Ok(_) => panic!("a model must not load from this directory"),
        }
    }

    #[test]
    fn a_directory_without_weights_names_what_is_missing() {
        let directory = tempfile::TempDir::new().expect("test directory should be created");

        let message = aria_kit::primitives::error_chain(&load_error(directory.path()));

        assert!(
            message.contains(WEIGHTS_FILE),
            "{message} does not name the missing file"
        );
    }

    #[test]
    fn cancelled_load_stops_before_model_validation() {
        let cancel = CancelToken::new();
        cancel.cancel();

        let outcome =
            WhisperCandle::load_from(WhisperCandleConfig::new("/nonexistent/model"), &cancel);

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
    }
}
