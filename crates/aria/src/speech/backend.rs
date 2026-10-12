//! The synthesis engine an utterance runs on, chosen by configuration.

use std::path::PathBuf;
use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use aria_tts::kokoro::Kokoro;
use aria_tts::qwen::Qwen;
use aria_tts::system::SystemVoice;

use crate::config::{TtsBackend, TtsFallback};
use crate::rpc::RpcErrorCode;
use crate::synthesis::{PlaybackError, SpeakerOutput, Synthesis, SynthesisError};

use super::{
    ExecutionControl, SpeechError, SpeechEventEmitter, SpeechExecutor, SpeechOutcome,
    SpeechSettings, SpeechStage,
};

pub(crate) enum SpeechBackend {
    Kokoro(Synthesis<Kokoro>),
    Qwen(Synthesis<Qwen>),
    System(SystemVoice),
}

impl SpeechBackend {
    /// Builds the configured engine. A model-backed engine that cannot be
    /// loaded — or cannot be heard, for want of a speaker — falls back to the
    /// platform voice only when the configuration asked for that, loudly
    /// either way.
    pub(crate) fn build(
        backend: TtsBackend,
        fallback: TtsFallback,
    ) -> Result<Self, SpeechBuildError> {
        match backend {
            TtsBackend::Kokoro { model, voices } => match Self::build_kokoro(model, voices) {
                Ok(backend) => Ok(backend),
                Err(error) if fallback == TtsFallback::System => {
                    tracing::warn!(
                        error = %crate::primitives::error_chain(&error),
                        "kokoro is unavailable; speaking through the system voice"
                    );
                    Self::build_system()
                }
                Err(error) => Err(error),
            },
            TtsBackend::Qwen {
                model_dir,
                tokenizer_dir,
            } => match Self::build_qwen(model_dir, tokenizer_dir) {
                Ok(backend) => Ok(backend),
                Err(error) if fallback == TtsFallback::System => {
                    tracing::warn!(
                        error = %crate::primitives::error_chain(&error),
                        "qwen is unavailable; speaking through the system voice"
                    );
                    Self::build_system()
                }
                Err(error) => Err(error),
            },
            TtsBackend::System => Self::build_system(),
        }
    }

    fn build_kokoro(model: PathBuf, voices: PathBuf) -> Result<Self, SpeechBuildError> {
        audible_output()?;
        let engine = Kokoro::load(&model, &voices).map_err(SpeechBuildError::Synthesis)?;
        Ok(Self::Kokoro(Synthesis::new(engine)))
    }

    fn build_qwen(model_dir: PathBuf, tokenizer_dir: PathBuf) -> Result<Self, SpeechBuildError> {
        audible_output()?;
        let engine = Qwen::load(&model_dir, &tokenizer_dir).map_err(SpeechBuildError::Synthesis)?;
        Ok(Self::Qwen(Synthesis::new(engine)))
    }

    fn build_system() -> Result<Self, SpeechBuildError> {
        let voice = SystemVoice::new().map_err(SpeechBuildError::Synthesis)?;
        Ok(Self::System(voice))
    }

    fn speak(
        &self,
        settings: SpeechSettings,
        control: &Arc<ExecutionControl>,
        events: &SpeechEventEmitter,
    ) -> Result<SpeechOutcome, SpeechError> {
        let stop = Arc::clone(control);
        let cancel = CancelToken::from_fn(move || stop.should_stop());
        let spoken = match self {
            Self::Kokoro(speech) => speak_utterance(speech, &settings, events, &cancel),
            Self::Qwen(speech) => speak_utterance(speech, &settings, events, &cancel),
            Self::System(voice) => voice.speak_with_started(
                &settings.text,
                settings.voice.as_ref(),
                settings.rate,
                &cancel,
                || events.emit(SpeechStage::Speaking),
            ),
        };
        match spoken {
            Ok(()) => Ok(SpeechOutcome::Spoken),
            // An engine killed by its own ceiling is the utterance overrunning
            // its allowance, not a synthesis fault.
            Err(SynthesisError::TimedOut { .. }) => Err(SpeechError::Timeout(control.timeout())),
            Err(error) => Err(SpeechError::Synthesis(error)),
        }
    }
}

/// Why the speech engine could not be built. The speaker and the model fail
/// for different reasons, and an operator needs to know which.
#[derive(Debug, thiserror::Error)]
pub enum SpeechBuildError {
    #[error("the speech engine could not be loaded")]
    Synthesis(#[source] SynthesisError),

    #[error("audio output is unavailable")]
    Playback(#[source] PlaybackError),
}

impl From<&SpeechBuildError> for RpcErrorCode {
    fn from(error: &SpeechBuildError) -> Self {
        match error {
            SpeechBuildError::Synthesis(_) => Self::SynthesisFailed,
            SpeechBuildError::Playback(_) => Self::PlaybackFailed,
        }
    }
}

impl SpeechExecutor for SpeechBackend {
    fn execute(
        &self,
        settings: SpeechSettings,
        control: &Arc<ExecutionControl>,
        events: &SpeechEventEmitter,
    ) -> Result<SpeechOutcome, SpeechError> {
        self.speak(settings, control, events)
    }
}

/// A machine that cannot play audio has no use for a loaded model, so the
/// speaker is resolved at build time even though playback itself opens a
/// device per utterance.
fn audible_output() -> Result<SpeakerOutput, SpeechBuildError> {
    SpeakerOutput::new().map_err(SpeechBuildError::Playback)
}

fn speak_utterance<S: aria_kit::synthesis::Engine>(
    speech: &Synthesis<S>,
    settings: &SpeechSettings,
    events: &SpeechEventEmitter,
    cancel: &CancelToken,
) -> Result<(), SynthesisError> {
    let output = SpeakerOutput::new().map_err(|source| SynthesisError::Playback { source })?;
    let session = speech.start_with(
        &settings.text,
        settings.voice.as_ref(),
        settings.rate,
        cancel,
    )?;
    events.emit(SpeechStage::Synthesizing);
    session.play_on(&output, || events.emit(SpeechStage::Speaking))
}
