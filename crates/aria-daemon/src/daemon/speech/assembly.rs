//! Composition of a speech capability: backend loading and the coordinator
//! the runtime executes.

use aria_settings::TtsEngine;
use aria_settings::rpc::RpcErrorCode;
use aria_voice::config::TtsConfig;
use aria_voice::speech::{SpeechBackend, SpeechBuildError, SpeechCoordinator};

pub(super) struct BuiltSpeech {
    pub(super) coordinator: SpeechCoordinator<SpeechBackend>,
    /// Whose saved voice applies, which a fallback may have changed.
    pub(super) engine: TtsEngine,
}

pub(super) fn build_speech(config: TtsConfig) -> Result<BuiltSpeech, SpeechInitializationError> {
    let TtsConfig { backend, fallback } = config;
    tracing::info!(
        backend = backend.name(),
        "building the text-to-speech engine"
    );
    let backend =
        SpeechBackend::build(backend, fallback).map_err(SpeechInitializationError::Backend)?;
    Ok(BuiltSpeech {
        engine: backend.engine(),
        coordinator: SpeechCoordinator::new(backend),
    })
}

impl From<&SpeechInitializationError> for RpcErrorCode {
    fn from(error: &SpeechInitializationError) -> Self {
        match error {
            SpeechInitializationError::LoadPanicked => Self::Internal,
            SpeechInitializationError::Backend(error) => error.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(in crate::daemon) enum SpeechInitializationError {
    #[error("failed to initialize the text-to-speech engine")]
    Backend(#[source] SpeechBuildError),

    #[error("the text-to-speech engine panicked while loading")]
    LoadPanicked,
}
