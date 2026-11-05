//! Speech initialization state and request execution owned by the daemon.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use aria_kit::cancel::CancelToken;

use aria_primitives::lazy::Lazy;
use aria_settings::SettingsCache;
use aria_settings::TtsEngine;
use aria_settings::rpc::RpcErrorCode;
use aria_voice::config::{SpeechDefaults, TtsConfig};
use aria_voice::speech::{
    SpeechError, SpeechEventEmitter, SpeechOutcome, SpeechRequest, SpeechSettings, SpeechStage,
};

use super::super::context::Context;
use super::assembly::{BuiltSpeech, SpeechInitializationError, build_speech};

pub(in crate::daemon) struct SpeechRuntime {
    emitter: SpeechEventEmitter,
    cache: Arc<SettingsCache>,
    speech: Lazy<BuiltSpeech, SpeechInitializationError>,
}

impl SpeechRuntime {
    /// Reads the settings and loads the engine on first use. A failed load
    /// keeps nothing, so the next request to speak tries again.
    pub(in crate::daemon) fn new(
        context: &Context<'_>,
        emitter: SpeechEventEmitter,
        cache: Arc<SettingsCache>,
    ) -> Self {
        let home = context.home.clone();
        Self {
            emitter,
            cache: Arc::clone(&cache),
            speech: speech_load(move || {
                let settings = cache.effective();
                build_speech(TtsConfig::load(&settings, &home))
            }),
        }
    }

    /// Loads the engine now instead of at the first request to speak.
    pub(in crate::daemon) fn load(&self) -> Result<(), SpeechInitializationError> {
        self.speech.get().map(|_speech| ())
    }

    pub(in crate::daemon) fn execute(
        &self,
        request: SpeechRequest,
        cancellation: CancelToken,
    ) -> Result<SpeechOutcome, SpeechRuntimeError> {
        if cancellation.is_cancelled() {
            return Err(SpeechRuntimeError::Execution(SpeechError::Cancelled));
        }
        let speech = match self.speech.get() {
            Ok(speech) => speech,
            Err(_) if cancellation.is_cancelled() => {
                return Err(SpeechRuntimeError::Execution(SpeechError::Cancelled));
            }
            Err(error) => {
                self.emitter.emit(SpeechStage::Failed((&error).into()));
                return Err(error.into());
            }
        };

        let settings = next_utterance(&self.cache, request, speech.engine);
        speech
            .coordinator
            .execute(settings, cancellation, &self.emitter)
            .map_err(SpeechRuntimeError::Execution)
    }

    /// Stops the utterance playing, if any. A runtime that never spoke has
    /// nothing to stop and nothing to build.
    pub(in crate::daemon) fn stop(&self) -> bool {
        self.speech
            .built()
            .is_some_and(|speech| speech.coordinator.stop())
    }
}

/// A panicking load counts as a failed one, so it can't end the daemon.
fn speech_load(
    build: impl Fn() -> Result<BuiltSpeech, SpeechInitializationError> + Send + Sync + 'static,
) -> Lazy<BuiltSpeech, SpeechInitializationError> {
    Lazy::new(move || {
        catch_unwind(AssertUnwindSafe(&build))
            .unwrap_or(Err(SpeechInitializationError::LoadPanicked))
    })
}

/// The request's voice and rate, else the file's as it stands now, else the
/// engine's own.
pub(in crate::daemon) fn next_utterance(
    cache: &SettingsCache,
    request: SpeechRequest,
    engine: TtsEngine,
) -> SpeechSettings {
    let effective = cache.effective();
    let saved = SpeechDefaults::load(&effective, engine);
    SpeechSettings {
        text: request.text,
        voice: request.voice.or(saved.voice),
        rate: request.rate.unwrap_or(saved.rate),
    }
}

#[derive(Debug, thiserror::Error)]
pub(in crate::daemon) enum SpeechRuntimeError {
    #[error(transparent)]
    Initialization(#[from] SpeechInitializationError),

    #[error(transparent)]
    Execution(#[from] SpeechError),
}

impl From<&SpeechRuntimeError> for RpcErrorCode {
    fn from(error: &SpeechRuntimeError) -> Self {
        match error {
            SpeechRuntimeError::Initialization(error) => error.into(),
            SpeechRuntimeError::Execution(error) => error.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aria_kit::synthesis::SynthesisError;
    use aria_kit::synthesis::{SpeakingRate, SpeechText, Voice};
    use aria_voice::speech::SpeechBuildError;

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

        let saved = next_utterance(&store, request(None, None), TtsEngine::Kokoro);
        assert_eq!(saved.voice, Some(Voice::new("af_heart").unwrap()));
        assert_eq!(saved.rate, SpeakingRate::new(1.5).unwrap());

        let asked = next_utterance(
            &store,
            request(Some("af_sky"), Some(2.0)),
            TtsEngine::Kokoro,
        );
        assert_eq!(asked.voice, Some(Voice::new("af_sky").unwrap()));
        assert_eq!(asked.rate, SpeakingRate::new(2.0).unwrap());

        let engine = next_utterance(&store, request(None, None), TtsEngine::Qwen);
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

        let spoken = next_utterance(&store, request(None, None), TtsEngine::System);

        assert_eq!(spoken.voice, Some(Voice::new("Samantha").unwrap()));
    }

    fn unusable_model() -> SpeechInitializationError {
        SpeechInitializationError::Backend(SpeechBuildError::Synthesis(
            SynthesisError::UnsupportedBackend { backend: "kokoro" },
        ))
    }

    /// A runtime that loads with `build`, and a count of how often it ran.
    fn runtime(
        home: &std::path::Path,
        build: impl Fn() -> Result<BuiltSpeech, SpeechInitializationError> + Send + Sync + 'static,
    ) -> (SpeechRuntime, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let runtime = SpeechRuntime {
            emitter: SpeechEventEmitter::subscribed_by(
                aria_voice::speech::EVENT_THREAD,
                |_stage| {},
            ),
            cache: Arc::new(store(home, "")),
            speech: speech_load(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                build()
            }),
        };
        (runtime, attempts)
    }

    #[test]
    fn a_failed_load_leaves_each_utterance_its_own_attempt() {
        let home = tempfile::tempdir().unwrap();
        let (runtime, attempts) = runtime(home.path(), || Err(unusable_model()));

        assert!(runtime.load().is_err());
        for _ in 0..2 {
            let failure = runtime.execute(request(None, None), CancelToken::new());
            assert!(matches!(
                failure,
                Err(SpeechRuntimeError::Initialization(
                    SpeechInitializationError::Backend(_)
                ))
            ));
        }

        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_panicking_load_fails_and_stays_retryable() {
        let home = tempfile::tempdir().unwrap();
        let (runtime, attempts) = runtime(home.path(), || panic!("a broken model file"));

        for _ in 0..2 {
            assert!(matches!(
                runtime.load(),
                Err(SpeechInitializationError::LoadPanicked)
            ));
        }

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stopping_or_cancelling_before_the_load_builds_nothing() {
        let home = tempfile::tempdir().unwrap();
        let (runtime, attempts) = runtime(home.path(), || Err(unusable_model()));
        let cancelled = CancelToken::new();
        cancelled.cancel();

        assert!(!runtime.stop());
        let outcome = runtime.execute(request(None, None), cancelled);

        assert!(matches!(
            outcome,
            Err(SpeechRuntimeError::Execution(SpeechError::Cancelled))
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
    }

    /// The system voice stands in for a repaired model. Only macOS always has it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_repaired_engine_loads_once_and_stays() {
        let home = tempfile::tempdir().unwrap();
        let broken = std::sync::atomic::AtomicBool::new(true);
        let (runtime, attempts) = runtime(home.path(), move || {
            if broken.swap(false, Ordering::SeqCst) {
                return Err(unusable_model());
            }
            build_speech(TtsConfig {
                backend: aria_voice::config::TtsBackend::System,
                fallback: aria_voice::config::TtsFallback::None,
            })
        });

        assert!(runtime.load().is_err());
        assert!(runtime.load().is_ok());
        assert!(runtime.load().is_ok());

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
