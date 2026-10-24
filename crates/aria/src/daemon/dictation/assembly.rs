//! Composition of a dictation capability: engine selection, model-loading
//! policy, and the coordinator the runtime executes.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::Layered;
use aria_kit::voice_activity::SileroVad;

use super::super::capture::{CaptureBackend, PreparedCapture};
use super::super::inference::client::RemoteTranscriber;
use super::super::vocabulary::Runtime as VocabularyRuntime;
use super::DictationSetup;
use crate::audio::{CaptureError, RecordingReader, WakeWordControl, WakeWordMonitor};
use crate::config::{
    ConfigError, DictationConfig, ServerEndpoint, SpeakerConfig, TranscriptionBackend,
    TranscriptionConfig,
};
use crate::delivery::{DeliveryError, PlatformTextDelivery};
use crate::dictation::{
    CleanupError, Cursor, DictationCoordinator, DictationError, DictationEventEmitter,
    DictationExecutor, DictationPipeline, DictationPolicy, DictationPolicyError, DictationResult,
    DictationSettings, ExecutionControl, LlmCleanup,
};
use crate::rpc::RpcErrorCode;
use crate::transcription::{
    Engine, Language, Mode, Streaming, StreamingEngine, Transcriber, Transcript, Transcription,
    TranscriptionError, Whole,
};
use crate::vocabulary::{Directory, Vocabulary};

use super::super::transcripts::{History, TranscriptStore};
use super::model_load::ModelLoad;

use aria_speaker::wespeaker::WeSpeaker;
use aria_stt::EngineConfig;
use aria_stt::moonshine::{Moonshine, MoonshineConfig};
use aria_stt::nemotron::{Nemotron, NemotronConfig};
use aria_stt::parakeet::{Parakeet, ParakeetConfig};
#[cfg(target_os = "macos")]
use aria_stt::parakeet_mlx::{ParakeetMlx, ParakeetMlxConfig};

use crate::microphone::Gate;
use crate::speaker;
use aria_stt::whisper::{Whisper, WhisperConfig};
use aria_stt::whisper_candle::{WhisperCandle, WhisperCandleConfig};

pub(super) type RuntimeExecutor = Box<dyn DictationExecutor>;

/// The sinks a dictation publishes to, in order: history, then the cursor,
/// so a transcript the app refuses is still on record.
type Sinks = (History, Cursor<PlatformTextDelivery>);
type CompletionLayers = Layered<Option<LlmCleanup>, ()>;

struct EngineExecution<T: Transcriber, M: Mode> {
    pipeline:
        DictationPipeline<Arc<CaptureBackend>, T, M, WeSpeaker, SileroVad, Sinks, CompletionLayers>,
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
        cancel: &CancelToken,
    ) -> Result<Transcript, DictationError> {
        self.prepared(project, |vocabulary| {
            self.pipeline.transcribe(
                recording,
                language,
                own_voice_only,
                Some(vocabulary),
                cancel,
            )
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
    #[error("failed to load dictation configuration")]
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
    fn load<E>(self, config: E::Config) -> Result<ModelLoad<E>, DictationInitializationError>
    where
        E: Engine,
        E::Config: Clone + Send + Sync + 'static,
    {
        match self {
            Self::Eager => {
                let engine = load_engine::<E>(config, &CancelToken::never())
                    .map_err(DictationInitializationError::Transcription)?;
                Ok(ModelLoad::loaded(engine))
            }
            Self::OnFirstUse => Ok(ModelLoad::deferred(move |cancel| {
                load_engine::<E>(config.clone(), cancel)
            })),
        }
    }
}

fn load_engine<E: Engine>(
    config: E::Config,
    cancel: &CancelToken,
) -> Result<E, TranscriptionError> {
    let started = Instant::now();
    let engine = E::load_from(config, cancel)?;
    tracing::info!(
        engine = E::NAME,
        load_ms = started.elapsed().as_millis(),
        "transcription model loaded"
    );
    Ok(engine)
}

/// Type-erased so capture can open between selecting the engine and building
/// its pipeline.
struct SelectedEngine {
    name: &'static str,
    build: Box<dyn FnOnce(ExecutionParts) -> RuntimeExecutor>,
}

impl SelectedEngine {
    fn new<T: Transcriber, M: Mode>(
        name: &'static str,
        transcription: Transcription<T, M>,
    ) -> Self {
        Self {
            name,
            build: Box::new(move |parts: ExecutionParts| parts.finish(name, transcription)),
        }
    }
}

fn select_engine(
    backend: TranscriptionBackend,
    loading: ModelLoading,
    home: &Path,
) -> Result<SelectedEngine, DictationInitializationError> {
    match backend {
        TranscriptionBackend::Local(config) => select_local(config, loading),
        TranscriptionBackend::Server { endpoint } => select_remote(home, &endpoint),
    }
}

/// Asks the server what it runs, so an unreachable server fails the build.
fn select_remote(
    home: &Path,
    endpoint: &ServerEndpoint,
) -> Result<SelectedEngine, DictationInitializationError> {
    let remote = RemoteTranscriber::connect(home, endpoint, &CancelToken::never())
        .map_err(DictationInitializationError::Transcription)?;
    let engine = remote.engine_name();
    Ok(if remote.supports_streaming() {
        SelectedEngine::new(engine, Transcription::<_, Streaming>::delegated(remote))
    } else {
        SelectedEngine::new(engine, Transcription::<_, Whole>::delegated(remote))
    })
}

fn select_local(
    config: TranscriptionConfig,
    loading: ModelLoading,
) -> Result<SelectedEngine, DictationInitializationError> {
    match config {
        TranscriptionConfig::Whisper { model } => {
            streaming::<Whisper>(WhisperConfig::new(model), loading)
        }
        TranscriptionConfig::WhisperCandle { model_directory } => {
            whole::<WhisperCandle>(WhisperCandleConfig::new(model_directory), loading)
        }
        TranscriptionConfig::Parakeet {
            model_directory,
            device,
        } => streaming::<Parakeet>(ParakeetConfig::new(model_directory, device), loading),
        TranscriptionConfig::Moonshine { model_directory } => {
            streaming::<Moonshine>(MoonshineConfig::new(model_directory), loading)
        }
        TranscriptionConfig::Nemotron { model_directory } => {
            streaming::<Nemotron>(NemotronConfig::new(model_directory), loading)
        }
        #[cfg(target_os = "macos")]
        TranscriptionConfig::ParakeetMlx { model, interpreter } => {
            whole::<ParakeetMlx>(ParakeetMlxConfig { interpreter, model }, loading)
        }
        #[cfg(not(target_os = "macos"))]
        TranscriptionConfig::ParakeetMlx { .. } => Err(
            DictationInitializationError::Transcription(TranscriptionError::UnsupportedBackend {
                backend: crate::config::backend::PARAKEET_MLX,
            }),
        ),
    }
}

/// A whole-mode engine reads the finished recording, so its input kind
/// decides whether the pipeline hands it samples or a WAV file.
fn whole<E>(
    config: E::Config,
    loading: ModelLoading,
) -> Result<SelectedEngine, DictationInitializationError>
where
    E: Engine,
    E::Config: Clone + Send + Sync + 'static,
{
    let model = loading.load::<E>(config)?;
    Ok(SelectedEngine::new(
        E::NAME,
        Transcription::complete(E::INPUT, model),
    ))
}

fn streaming<E>(
    config: E::Config,
    loading: ModelLoading,
) -> Result<SelectedEngine, DictationInitializationError>
where
    E: StreamingEngine,
    E::Config: Clone + Send + Sync + 'static,
{
    let model = loading.load::<E>(config)?;
    Ok(SelectedEngine::new(
        E::NAME,
        Transcription::stream(E::STREAM, model),
    ))
}

/// Everything a dictation pipeline needs except the engine.
struct ExecutionParts {
    capture: Arc<CaptureBackend>,
    delivery: PlatformTextDelivery,
    transcripts: Arc<TranscriptStore>,
    wake: Option<WakeWordControl>,
    speaker: SpeakerConfig,
    vocabulary: Arc<VocabularyRuntime>,
    cleanup: bool,
    backtrack: bool,
    cleanup_llm: Option<LlmCleanup>,
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
        Box::new(EngineExecution::<T, M> {
            pipeline: DictationPipeline::new(
                self.capture,
                transcription,
                speaker::reader(&self.speaker),
                (
                    History::new(self.transcripts, engine),
                    Cursor::new(self.delivery),
                ),
            )
            .cleaning(self.cleanup)
            .backtracking(self.backtrack)
            .hearing_speech(speaker::speech_gate())
            .with(self.cleanup_llm),
            wake: self.wake,
            speaker: self.speaker,
            vocabulary: self.vocabulary,
        })
    }
}

pub(super) struct BuiltDictation {
    pub(super) engine: &'static str,
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
) -> Result<BuiltDictation, DictationInitializationError> {
    let cleanup_llm = setup
        .dictation
        .cleanup_layer(home)
        .map_err(DictationInitializationError::Cleanup)?;
    let DictationSetup {
        dictation:
            DictationConfig {
                capture,
                transcription,
                dictation_timeouts: timeouts,
                cleanup,
                backtrack,
                cleanup_llm: _,
            },
        wake,
        speaker,
        vocabulary,
    } = setup;
    let delivery = PlatformTextDelivery::new(timeouts.delivery)
        .map_err(DictationInitializationError::Delivery)?;
    let policy =
        DictationPolicy::new(timeouts.overall).map_err(DictationInitializationError::Policy)?;
    let capture = PreparedCapture::load(capture, wake, spool_dir, timeouts.recording)?;
    let engine = select_engine(transcription, loading, home)?;
    let (capture, wake) = capture
        .open()
        .map_err(DictationInitializationError::Capture)?;
    let capture = Arc::new(capture);
    let wake_control = wake.as_ref().map(WakeWordMonitor::control);
    let execution = (engine.build)(ExecutionParts {
        capture: Arc::clone(&capture),
        delivery,
        transcripts,
        wake: wake_control.clone(),
        speaker,
        vocabulary,
        cleanup,
        backtrack,
        cleanup_llm,
    });
    Ok(BuiltDictation {
        engine: engine.name,
        coordinator: DictationCoordinator::new(policy, execution, microphone),
        wake,
        capture,
        wake_control,
    })
}

/// What a history retry runs on. A named server `engine` must match what the
/// server runs.
pub(super) enum RetryTarget<'a> {
    Local(TranscriptionConfig),
    Server {
        endpoint: ServerEndpoint,
        engine: Option<&'a str>,
    },
}

pub(super) struct Retried {
    pub(super) transcript: Transcript,
    pub(super) engine: &'static str,
}

pub(super) fn transcribe_retry(
    setup: &super::runtime::RetrySetup,
    target: RetryTarget<'_>,
    recording: &mut RecordingReader,
    settings: &DictationSettings,
    cancel: &CancelToken,
) -> Result<Retried, DictationError> {
    let config = &setup.dictation;
    let cleanup_llm = config
        .cleanup_layer(&setup.home)
        .map_err(|error| DictationError::Transcription(error.into()))?;
    let (transcription, engine) = retry_transcription(&setup.home, target, cancel)?;
    let snapshot = setup.vocabulary.snapshot(settings.project.as_ref());
    let pipeline = DictationPipeline::new((), transcription, speaker::reader(&setup.speaker), ())
        .cleaning(config.cleanup)
        .backtracking(config.backtrack)
        .hearing_speech(speaker::speech_gate())
        .with(cleanup_llm);
    pipeline.refresh_enrolled(speaker::enrolled(&setup.speaker));
    let transcript = pipeline.transcribe(
        recording,
        &settings.recognition_language,
        settings.own_voice_only,
        Some(snapshot.matcher.as_ref()),
        cancel,
    )?;
    Ok(Retried { transcript, engine })
}

/// A retry decodes the stored recording once, so every engine runs whole.
type RetryTranscription = Transcription<Box<dyn Transcriber>, Whole>;

fn retry_transcription(
    home: &Path,
    target: RetryTarget<'_>,
    cancel: &CancelToken,
) -> Result<(RetryTranscription, &'static str), TranscriptionError> {
    match target {
        RetryTarget::Server { endpoint, engine } => {
            let mut remote = RemoteTranscriber::connect(home, &endpoint, cancel)?;
            if let Some(engine) = engine {
                remote = remote.serving(engine)?;
            }
            let engine = remote.engine_name();
            let remote: Box<dyn Transcriber> = Box::new(remote);
            Ok((Transcription::delegated(remote), engine))
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

    use crate::audio::ReplayPace;
    use crate::config::{CaptureConfig, WakeConfig};

    #[tokio::test]
    async fn server_dictation_transcribes_replayed_audio_without_local_stt_models() {
        let server = crate::daemon::inference::host::tests::Fixture::start().await;
        let home = tempfile::tempdir().unwrap();
        server.pair_home(home.path().to_owned()).await;
        let endpoint = server.endpoint().to_owned();
        tokio::task::spawn_blocking(move || {
            let environment = crate::config::Environment::in_home(home.path().to_owned());
            let audio = home.path().join("speech.wav");
            std::fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../aria-wake/tests/fixtures/negative.wav"),
                &audio,
            )
            .unwrap();
            let (vocabulary, _worker) = VocabularyRuntime::new(
                home.path().join("vocabulary"),
                None,
                false,
                crate::vocabulary::discover::LocalCollector::new(
                    crate::vocabulary::extract::LocalExtractor,
                ),
            );
            let mut dictation = DictationConfig::load(&environment).unwrap();
            dictation.transcription = TranscriptionBackend::Server {
                endpoint: endpoint.clone(),
            };
            dictation.capture = CaptureConfig::WavFile {
                path: audio.clone(),
                pace: ReplayPace::Immediate,
            };
            let built = build_dictation(
                DictationSetup {
                    dictation,
                    speaker: SpeakerConfig::load(&environment).unwrap(),
                    vocabulary: Arc::clone(&vocabulary),
                    wake: WakeConfig::Disabled,
                },
                home.path(),
                home.path(),
                Arc::new(TranscriptStore::new()),
                ModelLoading::Eager,
                Gate::new(),
            )
            .unwrap();
            assert_eq!(built.engine, crate::config::backend::WHISPER);
            let mut settings = crate::preferences::UserPreferences::default().resolve_dictation(
                crate::dictation::DictationRequest::new(crate::dictation::StopMode::Manual),
                None,
            );
            settings.own_voice_only = false;
            settings.transcript_only = true;
            settings.delivery_mode = crate::dictation::DeliveryMode::Complete;
            let outcome = built
                .coordinator
                .execute(
                    settings,
                    CancelToken::never(),
                    &DictationEventEmitter::unsubscribed(),
                )
                .unwrap();
            let crate::dictation::DictationOutcome::Completed(result) = outcome else {
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
        })
        .await
        .unwrap();
        server.stop().await;
    }

    #[test]
    fn capture_is_validated_before_the_model_loads() {
        let home = tempfile::tempdir().unwrap();
        let environment = crate::config::Environment::in_home(home.path().to_owned());
        let mut config = DictationConfig::load(&environment).unwrap();
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
            crate::vocabulary::discover::LocalCollector::new(
                crate::vocabulary::extract::LocalExtractor,
            ),
        );
        let setup = DictationSetup {
            dictation: config,
            wake: WakeConfig::Disabled,
            speaker: SpeakerConfig::load(&environment).unwrap(),
            vocabulary,
        };

        let outcome = build_dictation(
            setup,
            home.path(),
            home.path(),
            Arc::new(TranscriptStore::new()),
            ModelLoading::Eager,
            Gate::new(),
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
        let environment = crate::config::Environment::in_home(home.path().to_owned());
        let mut config = DictationConfig::load(&environment).unwrap();
        config.cleanup_llm = Some(crate::config::LlmCleanupConfig {
            endpoint: "http://127.0.0.1:8080/v1/chat/completions".into(),
            model: "test".into(),
        });
        let speaker = SpeakerConfig::load(&environment).unwrap();
        let (vocabulary, _worker) = VocabularyRuntime::new(
            home.path().join("vocabulary"),
            None,
            false,
            crate::vocabulary::discover::LocalCollector::new(
                crate::vocabulary::extract::LocalExtractor,
            ),
        );
        let audio = crate::audio::canonical_fixture(home.path());
        let mut recording = RecordingReader::from_wav_file(&audio).unwrap();
        let settings = crate::preferences::UserPreferences::default().resolve_dictation(
            crate::dictation::DictationRequest::new(crate::dictation::StopMode::OnSilence),
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
