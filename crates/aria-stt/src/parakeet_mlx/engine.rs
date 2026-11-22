//! The engine the daemon holds: one resident sidecar, restarted when a
//! request leaves it unusable.

use std::path::PathBuf;
use std::sync::Mutex;

use aria_kit::backend::ModelDownload;
use aria_kit::cancel::CancelToken;
use aria_kit::primitives::process::{Sidecar, validate_interpreter};
use aria_kit::transcription::{
    AudioInputKind, Engine, Hypothesis, Language, RecognitionAudio, Recognizer, TranscriptionError,
    check_cancelled,
};

use super::protocol::{BACKEND, Phase, recognize, start, still_serving};

pub struct ParakeetMlx {
    interpreter: PathBuf,
    model: String,
    chunk_samples: Option<usize>,
    sidecar: Mutex<Option<Sidecar>>,
}

impl ParakeetMlx {
    /// Streams every recording through the sidecar's native streaming path,
    /// fed in chunks of this many samples.
    pub fn stream_chunks(mut self, chunk_samples: usize) -> Self {
        self.chunk_samples = Some(chunk_samples);
        self
    }

    pub fn load(
        interpreter: impl Into<PathBuf>,
        model: impl Into<String>,
    ) -> Result<Self, TranscriptionError> {
        Self::load_from(
            ParakeetMlxConfig {
                interpreter: interpreter.into(),
                model: model.into(),
            },
            &CancelToken::never(),
        )
    }

    /// Discards the sidecar after a poisoned lock: a panic may have left
    /// the request and reply stream out of sync.
    fn claim(&self) -> std::sync::MutexGuard<'_, Option<Sidecar>> {
        match self.sidecar.lock() {
            Ok(slot) => slot,
            Err(poisoned) => {
                let mut slot = poisoned.into_inner();
                slot.take();
                slot
            }
        }
    }
}

#[derive(Clone)]
pub struct ParakeetMlxConfig {
    /// The Python interpreter that runs the sidecar.
    pub interpreter: PathBuf,
    /// The model repository id the sidecar resolves from its own cache.
    pub model: String,
}

impl Engine for ParakeetMlx {
    const NAME: &'static str = super::NAME;
    const MODEL: &'static [ModelDownload] = &[];
    const INPUT: AudioInputKind = AudioInputKind::Samples;

    type Config = ParakeetMlxConfig;

    /// The model is a repository id the sidecar resolves against its own
    /// cache, so nothing here can confirm it without a download.
    fn check_from(config: &ParakeetMlxConfig) -> Result<PathBuf, TranscriptionError> {
        validate_interpreter(config.interpreter.clone()).map_err(|error| {
            Phase::Load {
                model: &config.model,
            }
            .transport(error)
        })
    }

    fn load_from(
        config: ParakeetMlxConfig,
        cancel: &CancelToken,
    ) -> Result<Self, TranscriptionError> {
        check_cancelled(cancel)?;
        let interpreter = Self::check_from(&config)?;
        let sidecar = start(&interpreter, &config.model, cancel)?;
        Ok(Self {
            interpreter,
            model: config.model,
            chunk_samples: None,
            sidecar: Mutex::new(Some(sidecar)),
        })
    }
}

impl Recognizer for ParakeetMlx {
    /// Accepts any language: the engine takes no language input.
    fn validate_language(
        &self,
        _language: &Language,
        _cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        Ok(())
    }

    fn recognize_with(
        &self,
        input: RecognitionAudio<'_>,
        _language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        let samples = input.require_samples(BACKEND)?;

        let mut slot = self.claim();
        let sidecar = match slot.as_mut() {
            Some(sidecar) => sidecar,
            None => slot.insert(start(&self.interpreter, &self.model, cancel)?),
        };

        let outcome = recognize(sidecar, samples, self.chunk_samples, cancel);
        if !still_serving(sidecar, &outcome) {
            slot.take();
        }
        outcome.map(Hypothesis::new)
    }
}

#[cfg(test)]
mod tests {
    use crate::parakeet_mlx::protocol::stubs::stub;

    use super::*;

    /// An engine whose slot starts empty and whose interpreter cannot exist,
    /// so any attempt to spawn is loud.
    fn unspawnable() -> ParakeetMlx {
        ParakeetMlx {
            interpreter: PathBuf::from("/nonexistent/python"),
            model: "unused".to_owned(),
            chunk_samples: None,
            sidecar: Mutex::new(None),
        }
    }

    #[test]
    fn cancelled_load_stops_before_interpreter_validation() {
        let cancel = CancelToken::new();
        cancel.cancel();

        let outcome = ParakeetMlx::load_from(
            ParakeetMlxConfig {
                interpreter: PathBuf::from("/nonexistent/python"),
                model: "unused".to_owned(),
            },
            &cancel,
        );

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
    }

    #[test]
    fn an_empty_slot_respawns_and_a_bad_interpreter_makes_that_a_load_error() {
        let outcome = unspawnable().recognize_with(
            RecognitionAudio::Samples(&[0.0]),
            &Language::Automatic,
            &CancelToken::never(),
        );

        assert!(matches!(outcome, Err(TranscriptionError::Load { .. })));
    }

    #[test]
    fn a_poisoned_lock_discards_the_suspect_sidecar() {
        let engine = ParakeetMlx {
            interpreter: PathBuf::from("/nonexistent/python"),
            model: "unused".to_owned(),
            chunk_samples: None,
            sidecar: Mutex::new(Some(stub("read line"))),
        };
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _guard = engine.sidecar.lock().expect("first claim of a clean lock");
                    panic!("poison while holding the request lock");
                })
                .join()
                .expect_err("the poisoning thread must panic");
        });

        assert!(
            engine.claim().is_none(),
            "a sidecar caught mid-request is not reused"
        );
    }
}
