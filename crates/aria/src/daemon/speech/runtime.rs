//! Speech initialization state and request execution owned by the daemon.

use std::sync::Arc;

use aria_kit::cancel::CancelToken;

use crate::config::{SpeechDefaults, TtsConfig, TtsEngine};
use crate::primitives::lazy::Lazy;
use crate::settings::SettingsCache;
use crate::speech::{
    SpeechError, SpeechEventEmitter, SpeechOutcome, SpeechRequest, SpeechSettings, SpeechStage,
};

use super::super::context::Context;
use super::super::subscriptions::Subscriptions;
use super::assembly::{BuiltSpeech, SpeechInitializationError, build_speech};

pub(in crate::daemon) struct SpeechRuntime {
    emitter: SpeechEventEmitter,
    cache: Arc<SettingsCache>,
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
    /// Settings and the engine are read when the first utterance asks
    /// for them, so a fault in either reaches the request that triggered it
    /// and is announced.
    pub(in crate::daemon) fn deferred(
        context: &Context<'_>,
        subscriptions: Arc<Subscriptions>,
        cache: Arc<SettingsCache>,
    ) -> Self {
        let home = context.home.clone();
        Self {
            emitter: event_emitter(subscriptions),
            cache: Arc::clone(&cache),
            speech: SpeechState::Deferred(Box::new(DeferredSpeech::new(move || {
                let settings = cache.effective();
                let config = TtsConfig::load(&settings.tts, &home)
                    .map_err(SpeechInitializationError::Config)?;
                build_speech(config)
            }))),
        }
    }

    /// Startup resolves the engine here, so a daemon that cannot speak says
    /// so before it accepts an utterance.
    pub(in crate::daemon) fn eager(
        subscriptions: Arc<Subscriptions>,
        cache: Arc<SettingsCache>,
        config: TtsConfig,
    ) -> Result<Self, SpeechInitializationError> {
        Ok(Self {
            emitter: event_emitter(subscriptions),
            cache,
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

        let settings = match next_utterance(&self.cache, request, speech.engine) {
            Ok(settings) => settings,
            Err(error) => {
                self.emitter.emit(SpeechStage::Failed((&error).into()));
                return Err(error.into());
            }
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

/// The request's voice and rate, else the file's as it stands now, else the
/// engine's own.
pub(in crate::daemon) fn next_utterance(
    cache: &SettingsCache,
    request: SpeechRequest,
    engine: TtsEngine,
) -> Result<SpeechSettings, SpeechInitializationError> {
    let effective = cache.effective();
    let saved =
        SpeechDefaults::load(&effective.tts, engine).map_err(SpeechInitializationError::Config)?;
    Ok(SpeechSettings {
        text: request.text,
        voice: request.voice.or(saved.voice),
        rate: request.rate.unwrap_or(saved.rate),
    })
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

#[cfg(test)]
mod tests {
    use aria_kit::synthesis::{SpeakingRate, SpeechText, Voice};

    use super::*;

    fn store(home: &std::path::Path, text: &str) -> SettingsCache {
        let path = home.join("settings.toml");
        std::fs::write(&path, text).unwrap();
        SettingsCache::open(path, home.to_path_buf()).unwrap()
    }

    fn request(voice: Option<&str>, rate: Option<f32>) -> SpeechRequest {
        SpeechRequest {
            text: SpeechText::new("hello").unwrap(),
            voice: voice.map(|voice| Voice::new(voice).unwrap()),
            rate: rate.map(|rate| SpeakingRate::new(rate).unwrap()),
        }
    }

    #[test]
    fn a_request_wins_then_the_saved_value_then_the_engine() {
        let home = tempfile::tempdir().unwrap();
        let store = store(
            home.path(),
            "[tts]\nrate = 1.5\n[tts.kokoro]\nvoice = \"af_heart\"\n",
        );

        let saved = next_utterance(&store, request(None, None), TtsEngine::Kokoro).unwrap();
        assert_eq!(saved.voice, Some(Voice::new("af_heart").unwrap()));
        assert_eq!(saved.rate, SpeakingRate::new(1.5).unwrap());

        let asked = next_utterance(
            &store,
            request(Some("af_sky"), Some(2.0)),
            TtsEngine::Kokoro,
        )
        .unwrap();
        assert_eq!(asked.voice, Some(Voice::new("af_sky").unwrap()));
        assert_eq!(asked.rate, SpeakingRate::new(2.0).unwrap());

        let engine = next_utterance(&store, request(None, None), TtsEngine::Qwen).unwrap();
        assert_eq!(engine.voice, None);
    }

    /// Kokoro failed to load and the system voice took over: it must not be
    /// handed a voice only Kokoro has.
    #[test]
    fn a_system_fallback_never_gets_a_kokoro_voice() {
        let home = tempfile::tempdir().unwrap();
        let store = store(
            home.path(),
            "[tts]\nbackend = \"kokoro\"\nfallback = \"system\"\n\
             [tts.kokoro]\nvoice = \"af_heart\"\n[tts.system]\nvoice = \"Samantha\"\n",
        );

        let spoken = next_utterance(&store, request(None, None), TtsEngine::System).unwrap();

        assert_eq!(spoken.voice, Some(Voice::new("Samantha").unwrap()));
    }
}
