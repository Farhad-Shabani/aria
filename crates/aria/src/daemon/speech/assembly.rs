//! Composition of a speech capability: backend loading, the defaults a
//! request falls back to, and the coordinator the runtime executes.

use aria_kit::synthesis::{SpeakingRate, Voice};

use crate::config::{ConfigError, TtsConfig};
use crate::rpc::RpcErrorCode;
use crate::speech::{SpeechBackend, SpeechBuildError, SpeechCoordinator};

/// The coordinator plus the configured defaults a request falls back to.
pub(super) struct BuiltSpeech {
    pub(super) coordinator: SpeechCoordinator<SpeechBackend>,
    pub(super) default_voice: Option<Voice>,
    pub(super) default_rate: SpeakingRate,
}

pub(super) fn build_speech(config: TtsConfig) -> Result<BuiltSpeech, SpeechInitializationError> {
    let TtsConfig {
        backend,
        voice,
        rate,
        fallback,
    } = config;
    tracing::info!(backend = backend.name(), "building the speech engine");
    let backend =
        SpeechBackend::build(backend, fallback).map_err(SpeechInitializationError::Backend)?;
    Ok(BuiltSpeech {
        coordinator: SpeechCoordinator::new(backend),
        default_voice: voice,
        default_rate: rate,
    })
}

impl From<&SpeechInitializationError> for RpcErrorCode {
    fn from(error: &SpeechInitializationError) -> Self {
        match error {
            SpeechInitializationError::Config(_) => Self::Internal,
            SpeechInitializationError::Backend(error) => error.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpeechInitializationError {
    #[error("failed to load speech configuration")]
    Config(#[source] ConfigError),

    #[error("failed to initialize the speech engine")]
    Backend(#[source] SpeechBuildError),
}
