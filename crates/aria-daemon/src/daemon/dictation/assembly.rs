//! Composition of a dictation capability: engine selection, model-loading
//! policy, and the coordinator the runtime executes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::slot::Slot;
use aria_kit::voice_activity::SileroVad;

use super::super::capture::{CaptureBackend, PreparedCapture};
use super::super::inference::client::RemoteTranscriber;
use super::super::vocabulary::Runtime as VocabularyRuntime;
use super::DictationSetup;
use aria_kit::audio::{CaptureError, RecordingReader, WakeWordControl, WakeWordMonitor};
use aria_platform::delivery::{DeliveryError, DeliveryFactory, PlatformTextDelivery};
use aria_settings::Directory;
use aria_settings::config::ConfigError;
use aria_settings::rpc::RpcErrorCode;
use aria_voice::config::{
    DictationConfig, SpeakerConfig, TranscriptionBackend, TranscriptionConfig,
};
use aria_voice::dictation::{
    CleanupError, ConfiguredLayers, Cursor, DictationCoordinator, DictationError,
    DictationEventEmitter, DictationExecutor, DictationPipeline, DictationPolicy,
    DictationPolicyError, DictationResult, DictationSettings, ExecutionControl, Rewrites,
    Rewriting, SoundMuting, Transcribed,
};
use aria_voice::transcription::{
    Language, Mode, Streaming, Transcriber, Transcription, TranscriptionError, Whole,
};
use aria_voice::vocabulary::Vocabulary;

use super::super::transcripts::{History, TranscriptStore};
use super::model_load::{ModelLoad, Preload};

use aria_segmentation::pyannote::Pyannote;
use aria_speaker::wespeaker::WeSpeaker;
use aria_stt::EngineConfig;

use aria_voice::microphone::{Claim, Gate};
use aria_voice::speaker;

pub(super) type RuntimeExecutor = Box<dyn DictationExecutor>;

/// The sinks a dictation publishes to, in order: history, then the cursor,
/// so a transcript the app refuses is still on record.
type Sinks = (History, Cursor<PlatformTextDelivery>);

struct EngineExecution<T: Transcriber, M: Mode> {
    engine: &'static str,
    pipeline: DictationPipeline<
        Arc<CaptureBackend>,
        SileroVad,
        WeSpeaker,
        Pyannote,
        T,
        M,
        ConfiguredLayers,
        Sinks,
    >,
    wake: Option<WakeWordControl>,
    /// Read before every press: a voice enrolled since the daemon started
    /// must be recognized without restarting it.
    speaker: SpeakerConfig,
    vocabulary: Arc<VocabularyRuntime>,
}

impl<T, M> EngineExecution<T, M>
where
    T: Transcriber,
    M: Mode,
{
    /// Runs `work` against the current vocabulary and enrolled voices, with
    /// the wake word suspended until it returns.
    fn prepared<R>(
        &self,
        project: Option<&Directory>,
        work: impl FnOnce(&Vocabulary) -> Result<R, DictationError>,
    ) -> Result<R, DictationError> {
        let snapshot = self.vocabulary.snapshot(project);
        let _wake_suspension = self
            .wake
            .as_ref()
            .map(WakeWordControl::suspend)
            .transpose()
            .map_err(DictationError::Capture)?;
        self.pipeline
            .refresh_enrolled(speaker::enrolled(&self.speaker));
        work(&snapshot.matcher)
    }
}

impl<T, M> DictationExecutor for EngineExecution<T, M>
where
    T: Transcriber,
    M: Mode,
{
    fn execute(
        &self,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Result<DictationResult, DictationError> {
        let project = settings.project.clone();
        self.prepared(project.as_ref(), |vocabulary| {
            self.pipeline
                .execute(settings, control, events, Some(vocabulary))
        })
    }

    fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        own_voice_only: bool,
        project: Option<&Directory>,
        rewriting: Rewriting,
        cancel: &CancelToken,
    ) -> Result<Transcribed, DictationError> {
        self.prepared(project, |vocabulary| {
            self.pipeline.transcribe(
                recording,
                language,
                own_voice_only,
                Some(vocabulary),
                rewriting,
                self.engine,
                cancel,
            )
        })
    }

    fn deliver_recording(
        &self,
        recording: &mut RecordingReader,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Result<String, DictationError> {
        let project = settings.project.clone();
        self.prepared(project.as_ref(), |vocabulary| {
            self.pipeline
                .deliver_recording(recording, settings, control, events, Some(vocabulary))
        })
    }
}

impl From<&DictationInitializationError> for RpcErrorCode {
    fn from(error: &DictationInitializationError) -> Self {
        match error {
            DictationInitializationError::Config(_)
            | DictationInitializationError::Cleanup(_)
            | DictationInitializationError::Policy(_)
            | DictationInitializationError::WakeConfiguration(_)
            | DictationInitializationError::WakeModel(_) => Self::Internal,
            DictationInitializationError::Capture(_) => Self::CaptureFailed,
            DictationInitializationError::Transcription(_) => Self::TranscriptionFailed,
            DictationInitializationError::Delivery(_) => Self::DeliveryFailed,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DictationInitializationError {
    #[error("failed to resolve the dictation settings")]
    Config(#[source] ConfigError),

    #[error("failed to prepare dictation cleanup")]
    Cleanup(#[source] CleanupError),

    #[error("failed to initialize audio capture")]
    Capture(#[source] CaptureError),

    #[error("failed to configure wake-word detection")]
    WakeConfiguration(#[source] aria_wake::ConfigError),

    #[error("failed to load the wake-word model")]
    WakeModel(#[source] aria_wake::Error),

    #[error("failed to initialize transcription")]
    Transcription(#[source] TranscriptionError),

    #[error("failed to initialize text delivery")]
    Delivery(#[source] DeliveryError),

    #[error("failed to initialize the dictation policy")]
    Policy(#[source] DictationPolicyError),
}

/// Whether startup blocks on the model, so a daemon that cannot transcribe
/// says so before it accepts a press instead of failing the press itself.
#[derive(Clone, Copy)]
pub(super) enum ModelLoading {
    Eager,
    OnFirstUse,
}

impl ModelLoading {
    fn load(
        self,
        engine: EngineConfig,
    ) -> Result<ModelLoad<Box<dyn Transcriber>>, TranscriptionError> {
        match self {
            Self::Eager => Ok(ModelLoad::loaded(load_engine(
                engine,
                &CancelToken::never(),
            )?)),
            Self::OnFirstUse => Ok(ModelLoad::deferred(move |cancel| {
                load_engine(engine.clone(), cancel)
            })),
        }
    }
}

fn load_engine(
    engine: EngineConfig,
    cancel: &CancelToken,
) -> Result<Box<dyn Transcriber>, TranscriptionError> {
    let name = engine.name();
    let started = Instant::now();
    let transcriber = engine.load(cancel)?;
    tracing::info!(
        engine = name,
        load_ms = started.elapsed().as_millis(),
        "transcription model loaded"
    );
    Ok(transcriber)
}

/// Type-erased so capture can open between selecting the engine and building
/// its pipeline.
struct SelectedEngine {
    name: &'static str,
    preload: Option<Preload>,
    build: Box<dyn FnOnce(ExecutionParts) -> RuntimeExecutor>,
}

impl SelectedEngine {
    fn new<T: Transcriber, M: Mode>(
        name: &'static str,
        transcription: Transcription<T, M>,
        preload: Option<Preload>,
    ) -> Self {
        Self {
            name,
            preload,
            build: Box::new(move |parts: ExecutionParts| parts.finish(name, transcription)),
        }
    }
}

fn select_engine(
    backend: TranscriptionBackend,
    loading: ModelLoading,
    home: &Path,
) -> Result<SelectedEngine, TranscriptionError> {
    match backend {
        TranscriptionBackend::Local(config) => select_local(config, loading),
        TranscriptionBackend::Server => select_remote(home),
    }
}

/// Each operation names the engine the server ran, so the executor's own
/// name only labels a dictation whose engine was never reported.
fn select_remote(home: &Path) -> Result<SelectedEngine, TranscriptionError> {
    let remote = RemoteTranscriber::open(home)?;
    Ok(SelectedEngine::new(
        aria_settings::config::backend::SERVER,
        Transcription::<_, Streaming>::delegated(remote),
        None,
    ))
}

fn select_local(
    config: TranscriptionConfig,
    loading: ModelLoading,
) -> Result<SelectedEngine, TranscriptionError> {
    let engine = EngineConfig::try_from(config)?;
    let name = engine.name();
    let input = engine.input();
    let stream = engine.stream();
    let model = loading.load(engine)?;
    let preload = model.preload();
    Ok(match stream {
        Some(stream) => SelectedEngine::new(name, Transcription::stream(stream, model), preload),
        // A whole-mode engine reads the finished recording, so its input
        // kind decides whether the pipeline hands it samples or a WAV file.
        None => SelectedEngine::new(name, Transcription::complete(input, model), preload),
    })
}

/// Everything a dictation pipeline needs except the engine.
struct ExecutionParts {
    capture: Arc<CaptureBackend>,
    delivery: PlatformTextDelivery,
    transcripts: Arc<TranscriptStore>,
    wake: Option<WakeWordControl>,
    speaker: SpeakerConfig,
    vocabulary: Arc<VocabularyRuntime>,
    rewrites: Rewrites<ConfiguredLayers>,
}

impl ExecutionParts {
    /// The one place the concrete pipeline becomes a boxed executor, so the
    /// engine, its mode, and its cadence stay static until here.
    fn finish<T, M>(
        self,
        engine: &'static str,
        transcription: Transcription<T, M>,
    ) -> RuntimeExecutor
    where
        T: Transcriber,
        M: Mode,
    {
        let (diarizer, segmenter) = speaker::tracking_models(&self.speaker);
        Box::new(EngineExecution::<T, M> {
            engine,
            pipeline: DictationPipeline::new(
                self.capture,
                transcription,
                (
                    History::new(self.transcripts, engine),
                    Cursor::new(self.delivery),
                ),
            )
            .hearing_speech(speaker::speech_gate())
            .tracking_voices(diarizer, segmenter)
            .rewriting(self.rewrites),
            wake: self.wake,
            speaker: self.speaker,
            vocabulary: self.vocabulary,
        })
    }
}

pub(super) struct DictationEngine {
    pub(super) name: &'static str,
    pub(super) backend: TranscriptionBackend,
    executor: RuntimeExecutor,
    preload: Option<Preload>,
}

/// Each call reads the current engine once and keeps it for its whole run.
#[derive(Clone)]
pub(super) struct EngineSlot {
    current: Arc<RwLock<Arc<DictationEngine>>>,
}

impl EngineSlot {
    pub(super) fn new(engine: DictationEngine) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(engine))),
        }
    }

    pub(super) fn current(&self) -> Arc<DictationEngine> {
        Arc::clone(&self.current.read().unwrap_or_else(|poisoned| {
            tracing::error!("the engine lock was poisoned; recovering");
            poisoned.into_inner()
        }))
    }

    /// The claim proves the microphone is held, so no call still runs on the
    /// old engine. The old engine drops here.
    pub(super) fn replace(&self, engine: DictationEngine, _held: &Claim) {
        let mut current = self.current.write().unwrap_or_else(|poisoned| {
            tracing::error!("the engine lock was poisoned; recovering");
            poisoned.into_inner()
        });
        let previous = std::mem::replace(&mut *current, Arc::new(engine));
        drop(current);
        drop(previous);
    }

    #[cfg(test)]
    pub(super) fn of(name: &'static str, executor: RuntimeExecutor) -> Self {
        let backend = TranscriptionBackend::Local(TranscriptionConfig::Moonshine {
            model_directory: PathBuf::from("/models"),
        });
        Self::new(DictationEngine::built(name, backend, executor))
    }
}

#[cfg(test)]
impl DictationEngine {
    pub(super) fn built(
        name: &'static str,
        backend: TranscriptionBackend,
        executor: RuntimeExecutor,
    ) -> Self {
        Self {
            name,
            backend,
            executor,
            preload: None,
        }
    }

    pub(super) fn with_preload(self, preload: Preload) -> Self {
        Self {
            preload: Some(preload),
            ..self
        }
    }
}

impl DictationExecutor for EngineSlot {
    fn execute(
        &self,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Result<DictationResult, DictationError> {
        self.current().executor.execute(settings, control, events)
    }

    fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        own_voice_only: bool,
        project: Option<&Directory>,
        rewriting: Rewriting,
        cancel: &CancelToken,
    ) -> Result<Transcribed, DictationError> {
        self.current().executor.transcribe(
            recording,
            language,
            own_voice_only,
            project,
            rewriting,
            cancel,
        )
    }

    fn deliver_recording(
        &self,
        recording: &mut RecordingReader,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Result<String, DictationError> {
        self.current()
            .executor
            .deliver_recording(recording, settings, control, events)
    }
}

/// Shared, so an engine that can't be built tries again on each press.
pub(super) type BuildEngine =
    Arc<dyn Fn(&TranscriptionBackend) -> Result<DictationEngine, DictationError> + Send + Sync>;

impl DictationEngine {
    /// Stands in for an engine that can't be built, so the slot never keeps
    /// an engine the settings no longer name, such as a server turned off.
    pub(super) fn unusable(backend: TranscriptionBackend, build: BuildEngine) -> Self {
        Self {
            name: backend.backend_name(),
            backend: backend.clone(),
            executor: Box::new(Unusable {
                backend,
                build,
                built: Slot::empty(),
            }),
            preload: None,
        }
    }

    /// Loads the model ahead of use. A stop leaves nothing half loaded.
    pub(super) fn preload(&self, stop: &CancelToken) -> Result<(), TranscriptionError> {
        self.preload
            .as_ref()
            .map_or(Ok(()), |preload| preload(stop))
    }
}

/// Builds on each press until a build succeeds, then keeps that engine, so a
/// transient failure costs one failed press, and a working engine isn't
/// rebuilt each press.
struct Unusable {
    backend: TranscriptionBackend,
    build: BuildEngine,
    built: Slot<DictationEngine>,
}

impl Unusable {
    fn engine(&self) -> Result<&DictationEngine, DictationError> {
        self.built.get_or_try_init(|| (self.build)(&self.backend))
    }
}

impl DictationExecutor for Unusable {
    fn execute(
        &self,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Result<DictationResult, DictationError> {
        self.engine()?.executor.execute(settings, control, events)
    }

    fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        own_voice_only: bool,
        project: Option<&Directory>,
        rewriting: Rewriting,
        cancel: &CancelToken,
    ) -> Result<Transcribed, DictationError> {
        self.engine()?.executor.transcribe(
            recording,
            language,
            own_voice_only,
            project,
            rewriting,
            cancel,
        )
    }

    fn deliver_recording(
        &self,
        recording: &mut RecordingReader,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Result<String, DictationError> {
        self.engine()?
            .executor
            .deliver_recording(recording, settings, control, events)
    }
}

/// Each new engine reuses these, including the open capture.
pub(super) struct EngineParts {
    capture: Arc<CaptureBackend>,
    delivery: DeliveryFactory,
    delivery_timeout: Duration,
    transcripts: Arc<TranscriptStore>,
    wake: Option<WakeWordControl>,
    speaker: SpeakerConfig,
    vocabulary: Arc<VocabularyRuntime>,
    /// For cleanup, whose keys wait for a restart.
    dictation: DictationConfig,
    home: PathBuf,
}

impl EngineParts {
    pub(super) fn engine(
        &self,
        backend: &TranscriptionBackend,
    ) -> Result<DictationEngine, DictationError> {
        let selected = select_engine(backend.clone(), ModelLoading::OnFirstUse, &self.home)
            .map_err(DictationError::Transcription)?;
        let parts = ExecutionParts {
            capture: Arc::clone(&self.capture),
            delivery: self.delivery.create(self.delivery_timeout)?,
            transcripts: Arc::clone(&self.transcripts),
            wake: self.wake.clone(),
            speaker: self.speaker.clone(),
            vocabulary: Arc::clone(&self.vocabulary),
            rewrites: Rewrites::load(&self.dictation, &self.home)
                .map_err(|error| DictationError::Transcription(error.into()))?,
        };
        Ok(DictationEngine {
            name: selected.name,
            backend: backend.clone(),
            executor: (selected.build)(parts),
            preload: selected.preload,
        })
    }
}

pub(super) struct BuiltDictation {
    pub(super) slot: EngineSlot,
    pub(super) parts: EngineParts,
    pub(super) coordinator: DictationCoordinator<RuntimeExecutor>,
    pub(super) wake: Option<WakeWordMonitor>,
    /// The one capture backend, for the other operations that record.
    pub(super) capture: Arc<CaptureBackend>,
    pub(super) wake_control: Option<WakeWordControl>,
}

/// Every other fallible step runs before the engine loads. Capture opens
/// last, so a failed eager load never opens the input device.
pub(super) fn build_dictation(
    setup: DictationSetup,
    spool_dir: &Path,
    home: &Path,
    transcripts: Arc<TranscriptStore>,
    loading: ModelLoading,
    microphone: Arc<Gate>,
    sound_muting: SoundMuting,
) -> Result<BuiltDictation, DictationInitializationError> {
    let rewrites =
        Rewrites::load(&setup.dictation, home).map_err(DictationInitializationError::Cleanup)?;
    let dictation = setup.dictation.clone();
    let DictationSetup {
        dictation:
            DictationConfig {
                capture,
                transcription,
                dictation_timeouts: timeouts,
                cleanup_llm: _,
            },
        wake,
        speaker,
        vocabulary,
        delivery: factory,
    } = setup;
    let delivery = factory
        .create(timeouts.delivery())
        .map_err(DictationInitializationError::Delivery)?;
    let policy =
        DictationPolicy::new(timeouts.overall()).map_err(DictationInitializationError::Policy)?;
    let capture = PreparedCapture::load(capture, wake, spool_dir, timeouts.recording())?;
    let backend = transcription.clone();
    let engine = select_engine(transcription, loading, home)
        .map_err(DictationInitializationError::Transcription)?;
    let (capture, wake) = capture
        .open()
        .map_err(DictationInitializationError::Capture)?;
    let capture = Arc::new(capture);
    let wake_control = wake.as_ref().map(WakeWordMonitor::control);
    let parts = EngineParts {
        capture: Arc::clone(&capture),
        delivery: factory,
        delivery_timeout: timeouts.delivery(),
        transcripts: Arc::clone(&transcripts),
        wake: wake_control.clone(),
        speaker: speaker.clone(),
        vocabulary: Arc::clone(&vocabulary),
        dictation,
        home: home.to_path_buf(),
    };
    let slot = EngineSlot::new(DictationEngine {
        name: engine.name,
        backend,
        executor: (engine.build)(ExecutionParts {
            capture: Arc::clone(&capture),
            delivery,
            transcripts,
            wake: wake_control.clone(),
            speaker,
            vocabulary,
            rewrites,
        }),
        preload: engine.preload,
    });
    let execution: RuntimeExecutor = Box::new(slot.clone());
    Ok(BuiltDictation {
        coordinator: DictationCoordinator::new(policy, execution, microphone)
            .with_sound_muting(sound_muting),
        slot,
        parts,
        wake,
        capture,
        wake_control,
    })
}

/// What a history retry runs on. A named server `engine` must match what the
/// server runs.
pub(super) enum RetryTarget<'a> {
    Local(TranscriptionConfig),
    Server { engine: Option<&'a str> },
}

pub(super) fn transcribe_retry(
    setup: &super::runtime::RetrySetup,
    target: RetryTarget<'_>,
    recording: &mut RecordingReader,
    settings: &DictationSettings,
    cancel: &CancelToken,
) -> Result<Transcribed, DictationError> {
    let config = &setup.dictation;
    let rewrites = Rewrites::load(config, &setup.home)
        .map_err(|error| DictationError::Transcription(error.into()))?;
    let (transcription, engine) = retry_transcription(&setup.home, target, cancel)?;
    let snapshot = setup.vocabulary.snapshot(settings.project.as_ref());
    let (diarizer, segmenter) = if settings.own_voice_only {
        speaker::tracking_models(&setup.speaker)
    } else {
        (None, None)
    };
    let pipeline = DictationPipeline::new((), transcription, ())
        .hearing_speech(speaker::speech_gate())
        .tracking_voices(diarizer, segmenter)
        .rewriting(rewrites);
    pipeline.transcribe(
        recording,
        &settings.stt_language,
        settings.own_voice_only,
        Some(snapshot.matcher.as_ref()),
        settings.rewriting,
        engine,
        cancel,
    )
}

/// A retry decodes the stored recording once, so every engine runs whole.
type RetryTranscription = Transcription<Box<dyn Transcriber>, Whole>;

fn retry_transcription(
    home: &Path,
    target: RetryTarget<'_>,
    cancel: &CancelToken,
) -> Result<(RetryTranscription, &'static str), TranscriptionError> {
    match target {
        RetryTarget::Server { engine } => {
            let mut remote = RemoteTranscriber::open(home)?;
            if let Some(engine) = engine {
                remote = remote.require_engine(engine)?;
            }
            let remote: Box<dyn Transcriber> = Box::new(remote);
            Ok((
                Transcription::delegated(remote),
                aria_settings::config::backend::SERVER,
            ))
        }
        RetryTarget::Local(config) => {
            let name = config.backend_name();
            let engine = EngineConfig::try_from(config)?;
            Ok((
                Transcription::complete(engine.input(), engine.load(cancel)?),
                name,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the server pinned at pairing receives audio.
    #[test]
    fn the_server_engine_needs_a_pairing() {
        let home = tempfile::tempdir().unwrap();
        let select = || {
            select_engine(
                TranscriptionBackend::Server,
                ModelLoading::OnFirstUse,
                home.path(),
            )
        };

        assert!(select().is_err());
        crate::daemon::inference::pin_server_for_tests(home.path(), "https://paired.test:9851");
        assert!(select().is_ok());
    }

    /// A settings store over `home`, with no file yet.
    fn settings_store(home: &Path) -> Arc<aria_settings::SettingsStore> {
        let cache =
            aria_settings::SettingsCache::open(home.join("settings.toml"), home.to_path_buf())
                .unwrap();
        Arc::new(aria_settings::SettingsStore::new(Arc::new(cache)))
    }

    use crate::daemon::config::WakeConfig;
    use aria_kit::audio::ReplayPace;
    use aria_voice::config::CaptureConfig;

    #[tokio::test]
    async fn one_server_build_dictates_and_fails_only_the_press_the_server_misses() {
        let server = crate::daemon::inference::host::tests::Fixture::start().await;
        let home = tempfile::tempdir().unwrap();
        server.pair_home(home.path().to_owned()).await;
        let root = home.path().to_owned();
        let (built, _vocabulary) = tokio::task::spawn_blocking(move || {
            let home_dir = root.clone();
            let audio = root.join("speech.wav");
            std::fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../aria-wake/tests/fixtures/negative.wav"),
                &audio,
            )
            .unwrap();
            let vocabulary = VocabularyRuntime::new(
                root.join("vocabulary"),
                None,
                false,
                settings_store(&root),
                aria_voice::vocabulary::discover::LocalCollector::new(
                    aria_voice::vocabulary::extract::LocalExtractor,
                ),
            );
            let mut dictation = DictationConfig::load(&aria_settings::testing::file(""), &home_dir);
            dictation.transcription = TranscriptionBackend::Server;
            dictation.capture = CaptureConfig::WavFile {
                path: audio,
                pace: ReplayPace::Immediate,
            };
            let built = build_dictation(
                DictationSetup {
                    dictation,
                    speaker: SpeakerConfig::load(
                        &aria_settings::SettingsFile::default().speaker,
                        &home_dir,
                    ),
                    vocabulary: Arc::clone(&vocabulary.0),
                    wake: WakeConfig::Disabled,
                    delivery: aria_platform::delivery::DeliveryFactory::detect(&root),
                },
                &root,
                &root,
                Arc::new(TranscriptStore::new()),
                ModelLoading::Eager,
                Gate::new(),
                SoundMuting::Never,
            )
            .unwrap();
            (built, vocabulary)
        })
        .await
        .unwrap();
        let engine = built.slot.current();
        assert_eq!(engine.name, aria_settings::config::backend::SERVER);
        let coordinator = Arc::new(built.coordinator);
        let dictate = move || {
            let mut settings = DictationSettings::resolve(
                aria_settings::Settings::default(),
                aria_voice::dictation::DictationRequest::new(
                    aria_voice::dictation::StopMode::Manual,
                ),
                None,
            );
            settings.own_voice_only = false;
            settings.transcript_only = true;
            settings.delivery_mode = aria_settings::DeliveryMode::Complete;
            let coordinator = Arc::clone(&coordinator);
            tokio::task::spawn_blocking(move || {
                coordinator.execute(
                    settings,
                    CancelToken::never(),
                    &DictationEventEmitter::unsubscribed(),
                )
            })
        };
        let outcome = dictate().await.unwrap().unwrap();
        let aria_voice::dictation::DictationOutcome::Completed(result) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(result.text.contains("native words"), "{}", result.text);
        assert!(
            result
                .text
                .split_whitespace()
                .all(|word| matches!(word, "native" | "words"))
        );
        assert!(!home.path().join(".aria/models").exists());
        server.stop().await;
        let missed = dictate().await.unwrap();
        assert!(
            matches!(missed, Err(DictationError::Transcription(_))),
            "{missed:?}"
        );
    }

    #[test]
    fn capture_is_validated_before_the_model_loads() {
        let home = tempfile::tempdir().unwrap();
        let home_dir = home.path().to_owned();
        let mut config = DictationConfig::load(&aria_settings::testing::file(""), &home_dir);
        config.transcription = TranscriptionBackend::Local(TranscriptionConfig::Whisper {
            model: home.path().join("missing-model.bin"),
        });
        config.capture = CaptureConfig::WavFile {
            path: home.path().join("missing-capture.wav"),
            pace: ReplayPace::Immediate,
        };
        let (vocabulary, _worker) = VocabularyRuntime::new(
            home.path().join("vocabulary"),
            None,
            false,
            settings_store(home.path()),
            aria_voice::vocabulary::discover::LocalCollector::new(
                aria_voice::vocabulary::extract::LocalExtractor,
            ),
        );
        let setup = DictationSetup {
            dictation: config,
            wake: WakeConfig::Disabled,
            speaker: SpeakerConfig::load(
                &aria_settings::SettingsFile::default().speaker,
                &home_dir,
            ),
            vocabulary,
            delivery: aria_platform::delivery::DeliveryFactory::detect(home.path()),
        };

        let outcome = build_dictation(
            setup,
            home.path(),
            home.path(),
            Arc::new(TranscriptStore::new()),
            ModelLoading::Eager,
            Gate::new(),
            SoundMuting::Never,
        );

        assert!(matches!(
            outcome,
            Err(DictationInitializationError::Capture(_))
        ));
    }

    #[test]
    fn capture_free_retry_validates_cleanup_before_model_loading() {
        let home = tempfile::tempdir().unwrap();
        let prompts = home.path().join(".aria/prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("cleanup.md"), [0xff]).unwrap();
        let home_dir = home.path().to_owned();
        let mut config = DictationConfig::load(&aria_settings::testing::file(""), &home_dir);
        config.cleanup_llm = Some(
            aria_settings::CleanupTarget::new("http://127.0.0.1:8080/v1/chat/completions", "test")
                .unwrap(),
        );
        let speaker =
            SpeakerConfig::load(&aria_settings::SettingsFile::default().speaker, &home_dir);
        let (vocabulary, _worker) = VocabularyRuntime::new(
            home.path().join("vocabulary"),
            None,
            false,
            settings_store(home.path()),
            aria_voice::vocabulary::discover::LocalCollector::new(
                aria_voice::vocabulary::extract::LocalExtractor,
            ),
        );
        let audio = aria_kit::audio::canonical_fixture(home.path());
        let mut recording = RecordingReader::from_wav_file(&audio).unwrap();
        let settings = DictationSettings::resolve(
            aria_settings::Settings::default(),
            aria_voice::dictation::DictationRequest::new(
                aria_voice::dictation::StopMode::OnSilence,
            ),
            None,
        );
        let TranscriptionBackend::Local(engine) = config.transcription.clone() else {
            panic!("the default backend is local");
        };
        let target = RetryTarget::Local(engine);
        let result = transcribe_retry(
            &super::super::runtime::RetrySetup {
                dictation: config,
                speaker,
                vocabulary,
                home: home.path().to_owned(),
            },
            target,
            &mut recording,
            &settings,
            &CancelToken::never(),
        );
        assert!(matches!(
            result,
            Err(DictationError::Transcription(TranscriptionError::Processing(
                aria_kit::transcription::layer::Error::Failed(error)
            ))) if matches!(error.downcast_ref::<CleanupError>(), Some(CleanupError::PromptRead))
        ));
    }
}
