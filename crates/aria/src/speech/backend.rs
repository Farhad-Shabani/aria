//! The synthesis engine an utterance runs on, chosen by the settings.

use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use aria_tts::kokoro::Kokoro;
use aria_tts::qwen::Qwen;
use aria_tts::system::SystemVoice;

use crate::config::{TtsBackend, TtsEngine, TtsFallback, TtsModel};
use crate::rpc::RpcErrorCode;
use crate::synthesis::{PlaybackError, Session, SpeakerOutput, Synthesis, SynthesisError};

use super::{
    ExecutionControl, SpeechError, SpeechEventEmitter, SpeechExecutor, SpeechOutcome,
    SpeechSettings, SpeechStage,
};

pub(crate) enum SpeechBackend {
    Model(SpeechModel),
    System(SystemVoice),
}

/// Synthesizes PCM without touching an audio device.
pub(crate) enum SpeechModel {
    Kokoro(Synthesis<Kokoro>),
    Qwen(Synthesis<Qwen>),
}

impl SpeechModel {
    fn load(config: TtsModel) -> Result<Self, SynthesisError> {
        match config {
            TtsModel::Kokoro { model, voices } => {
                Kokoro::load(model, voices).map(|engine| Self::Kokoro(Synthesis::new(engine)))
            }
            TtsModel::Qwen {
                model_dir,
                tokenizer_dir,
            } => Qwen::load(model_dir, tokenizer_dir)
                .map(|engine| Self::Qwen(Synthesis::new(engine))),
        }
    }

    fn start_with(
        &self,
        settings: &SpeechSettings,
        cancel: &CancelToken,
    ) -> Result<Session, SynthesisError> {
        let voice = settings.voice.as_ref();
        match self {
            Self::Kokoro(speech) => speech.start_with(&settings.text, voice, settings.rate, cancel),
            Self::Qwen(speech) => speech.start_with(&settings.text, voice, settings.rate, cancel),
        }
    }
}

impl SpeechBackend {
    /// The engine actually built: a fallback speaks with the system's.
    pub(crate) fn engine(&self) -> TtsEngine {
        match self {
            Self::Model(SpeechModel::Kokoro(_)) => TtsEngine::Kokoro,
            Self::Model(SpeechModel::Qwen(_)) => TtsEngine::Qwen,
            Self::System(_) => TtsEngine::System,
        }
    }

    /// Builds the configured engine. A model-backed engine that cannot be
    /// loaded — or cannot be heard, for want of a speaker — falls back to the
    /// platform voice only when the settings asked for that, loudly
    /// either way.
    pub(crate) fn build(
        backend: TtsBackend,
        fallback: TtsFallback,
    ) -> Result<Self, SpeechBuildError> {
        let model = match backend {
            TtsBackend::Model(model) => model,
            TtsBackend::System => return Self::build_system(),
        };
        let engine = model.name();
        match Self::build_model(model) {
            Ok(backend) => Ok(backend),
            Err(error) if fallback == TtsFallback::System => {
                tracing::warn!(
                    engine,
                    error = %crate::primitives::error_chain(&error),
                    "speech model is unavailable; speaking through the system voice"
                );
                Self::build_system()
            }
            Err(error) => Err(error),
        }
    }

    fn build_model(model: TtsModel) -> Result<Self, SpeechBuildError> {
        audible_output()?;
        SpeechModel::load(model)
            .map(Self::Model)
            .map_err(SpeechBuildError::Synthesis)
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
            Self::Model(model) => speak_utterance(model, &settings, events, &cancel),
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

fn speak_utterance(
    model: &SpeechModel,
    settings: &SpeechSettings,
    events: &SpeechEventEmitter,
    cancel: &CancelToken,
) -> Result<(), SynthesisError> {
    let output = SpeakerOutput::new().map_err(|source| SynthesisError::Playback { source })?;
    let session = model.start_with(settings, cancel)?;
    events.emit(SpeechStage::Synthesizing);
    session.play_on(&output, || events.emit(SpeechStage::Speaking))
}
