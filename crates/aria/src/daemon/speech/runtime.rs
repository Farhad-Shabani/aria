//! Speech initialization state and request execution owned by the daemon.

use std::sync::Arc;

use aria_kit::cancel::CancelToken;

use crate::config::TtsConfig;
use crate::primitives::lazy::Lazy;
use crate::speech::{
    SpeechError, SpeechEventEmitter, SpeechOutcome, SpeechRequest, SpeechSettings, SpeechStage,
};

use super::super::context::Context;
use super::super::subscriptions::Subscriptions;
use super::assembly::{BuiltSpeech, SpeechInitializationError, build_speech};

pub(in crate::daemon) struct SpeechRuntime {
    emitter: SpeechEventEmitter,
    speech: SpeechState,
}

/// An engine the daemon already holds, or how to build one when the first
/// utterance asks for it.
enum SpeechState {
    Ready(BuiltSpeech),
    /// Boxed: the empty slot inside is as wide as the engine it will hold,
    /// and a ready runtime should not pay for that.
    Deferred(Box<DeferredSpeech>),
}

type DeferredSpeech = Lazy<BuiltSpeech, SpeechInitializationError>;

impl SpeechRuntime {
    /// Configuration and the engine are read when the first utterance asks
    /// for them, so a fault in either reaches the request that triggered it
    /// and is announced.
    pub(in crate::daemon) fn deferred(
        context: &Context<'_>,
        subscriptions: Arc<Subscriptions>,
    ) -> Self {
        let config_environment = context.config_environment.clone();
        Self {
            emitter: event_emitter(subscriptions),
            speech: SpeechState::Deferred(Box::new(DeferredSpeech::new(move || {
                let config = TtsConfig::load(&config_environment)
                    .map_err(SpeechInitializationError::Config)?;
                build_speech(config)
            }))),
        }
    }

    /// Startup resolves the engine here, so a daemon that cannot speak says
    /// so before it accepts an utterance.
    pub(in crate::daemon) fn eager(
        subscriptions: Arc<Subscriptions>,
        config: TtsConfig,
    ) -> Result<Self, SpeechInitializationError> {
        Ok(Self {
            emitter: event_emitter(subscriptions),
            speech: SpeechState::Ready(build_speech(config)?),
        })
    }

    pub(in crate::daemon) fn execute(
        &self,
        request: SpeechRequest,
        cancellation: CancelToken,
    ) -> Result<SpeechOutcome, SpeechRuntimeError> {
        if cancellation.is_cancelled() {
            return Err(SpeechRuntimeError::Execution(SpeechError::Cancelled));
        }
        let speech = match self.speech() {
            Ok(speech) => speech,
            Err(_) if cancellation.is_cancelled() => {
                return Err(SpeechRuntimeError::Execution(SpeechError::Cancelled));
            }
            Err(error) => {
                self.emitter.emit(SpeechStage::Failed((&error).into()));
                return Err(error.into());
            }
        };

        let settings = SpeechSettings {
            text: request.text,
            voice: request.voice.or_else(|| speech.default_voice.clone()),
            rate: request.rate.unwrap_or(speech.default_rate),
        };
        speech
            .coordinator
            .execute(settings, cancellation, &self.emitter)
            .map_err(SpeechRuntimeError::Execution)
    }

    /// Stops the utterance playing, if any. A runtime that never spoke has
    /// nothing to stop and nothing to build.
    pub(in crate::daemon) fn stop(&self) -> bool {
        match &self.speech {
            SpeechState::Ready(speech) => speech.coordinator.stop(),
            SpeechState::Deferred(deferred) => deferred
                .built()
                .is_some_and(|speech| speech.coordinator.stop()),
        }
    }

    fn speech(&self) -> Result<&BuiltSpeech, SpeechInitializationError> {
        match &self.speech {
            SpeechState::Ready(speech) => Ok(speech),
            SpeechState::Deferred(deferred) => deferred.get(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(in crate::daemon) enum SpeechRuntimeError {
    #[error(transparent)]
    Initialization(#[from] SpeechInitializationError),

    #[error(transparent)]
    Execution(#[from] SpeechError),
}

fn event_emitter(subscriptions: Arc<Subscriptions>) -> SpeechEventEmitter {
    SpeechEventEmitter::subscribed_by(crate::speech::EVENT_THREAD, move |stage| {
        subscriptions.broadcast(stage);
    })
}
