//! Composition of a dictation capability: backend selection, capture and wake
//! pairing, model-loading policy, and the coordinator the runtime executes.

use std::path::Path;
use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::Layered;

use super::super::capture::CaptureBackend;
use super::super::vocabulary::Runtime as VocabularyRuntime;
use super::DictationSetup;
use crate::audio::{CaptureError, RecordingReader, WakeWordControl, WakeWordMonitor};
use crate::config::{
    CaptureConfig, ConfigError, DictationConfig, SpeakerConfig, TranscriptionConfig, WakeBackend,
    WakeConfig,
};
use crate::delivery::{DeliveryError, PlatformTextDelivery};
use crate::dictation::{
    CleanupError, Cursor, DictationCoordinator, DictationError, DictationEventEmitter,
    DictationExecutor, DictationPipeline, DictationPolicy, DictationPolicyError, DictationResult,
    DictationSettings, ExecutionControl, LlmCleanup,
};
use crate::rpc::RpcErrorCode;
use crate::transcription::{
    Engine, Language, Mode, Streaming, StreamingEngine, Transcript, Transcription,
    TranscriptionError, Whole,
};
use crate::vocabulary::{Directory, Vocabulary};

use super::super::transcripts::{History, TranscriptStore};
use super::model_load::ModelLoad;

use aria_speaker::wespeaker::WeSpeaker;
use aria_stt::moonshine::{Moonshine, MoonshineConfig};
use aria_stt::nemotron::{Nemotron, NemotronConfig};
use aria_stt::parakeet::{Parakeet, ParakeetConfig};
#[cfg(target_os = "macos")]
use aria_stt::parakeet_mlx::{ParakeetMlx, ParakeetMlxConfig};
use aria_wake::livekit::{Config as LiveKitConfig, LiveKitWakeWord};
use aria_wake::openwakeword::{Config as OpenWakeWordConfig, OpenWakeWord};

use crate::microphone::Gate;
use crate::speaker;
use aria_stt::whisper::{Whisper, WhisperConfig};
use aria_stt::whisper_candle::{WhisperCandle, WhisperCandleConfig};

pub(super) type RuntimeExecutor = Box<dyn DictationExecutor>;

/// The sinks a dictation publishes to, in order: history, then the cursor,
/// so a transcript the app refuses is still on record.
type Sinks = (History, Cursor<PlatformTextDelivery>);
type CompletionLayers = Layered<Option<LlmCleanup>, ()>;

struct EngineExecution<E: Engine, M: Mode> {
    pipeline:
        DictationPipeline<Arc<CaptureBackend>, ModelLoad<E>, M, WeSpeaker, Sinks, CompletionLayers>,
    wake: Option<WakeWordControl>,
    /// Read before every press: a voice enrolled since the daemon started
    /// must be recognized without restarting it.
    speaker: SpeakerConfig,
    vocabulary: Arc<VocabularyRuntime>,
}

impl<E, M> EngineExecution<E, M>
where
    E: Engine,
    M: Mode,
{
    /// Runs `work` against the current vocabulary and enrolled voices, with
    /// the wake word suspended until it returns.
    fn prepared<T>(
        &self,
        project: Option<&Directory>,
        work: impl FnOnce(&Vocabulary) -> Result<T, DictationError>,
    ) -> Result<T, DictationError> {
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

impl<E, M> DictationExecutor for EngineExecution<E, M>
where
    E: Engine,
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
                let engine = E::load_from(config, &CancelToken::never())
                    .map_err(DictationInitializationError::Transcription)?;
                Ok(ModelLoad::loaded(engine))
            }
            Self::OnFirstUse => Ok(ModelLoad::deferred(move |cancel| {
                E::load_from(config.clone(), cancel)
            })),
        }
    }
}

/// Everything a dictation pipeline needs except the engine.
struct ExecutionParts {
    capture: Arc<CaptureBackend>,
    delivery: PlatformTextDelivery,
    transcripts: Arc<TranscriptStore>,
    wake: Option<WakeWordControl>,
    loading: ModelLoading,
    speaker: SpeakerConfig,
    vocabulary: Arc<VocabularyRuntime>,
    cleanup: bool,
    backtrack: bool,
    cleanup_llm: Option<LlmCleanup>,
}

impl ExecutionParts {
    fn build(
        self,
        config: TranscriptionConfig,
    ) -> Result<RuntimeExecutor, DictationInitializationError> {
        match config {
            TranscriptionConfig::Whisper { model } => {
                self.streaming::<Whisper>(WhisperConfig::new(model))
            }
            TranscriptionConfig::WhisperCandle { model_directory } => {
                self.whole::<WhisperCandle>(WhisperCandleConfig::new(model_directory))
            }
            TranscriptionConfig::Parakeet {
                model_directory,
                device,
            } => self.streaming::<Parakeet>(ParakeetConfig::new(model_directory, device)),
            TranscriptionConfig::Moonshine { model_directory } => {
                self.streaming::<Moonshine>(MoonshineConfig::new(model_directory))
            }
            TranscriptionConfig::Nemotron { model_directory } => {
                self.streaming::<Nemotron>(NemotronConfig::new(model_directory))
            }
            #[cfg(target_os = "macos")]
            TranscriptionConfig::ParakeetMlx { model, interpreter } => {
                self.whole::<ParakeetMlx>(ParakeetMlxConfig { interpreter, model })
            }
            #[cfg(not(target_os = "macos"))]
            TranscriptionConfig::ParakeetMlx { .. } => {
                Err(DictationInitializationError::Transcription(
                    TranscriptionError::UnsupportedBackend {
                        backend: crate::config::backend::PARAKEET_MLX,
                    },
                ))
            }
        }
    }

    /// A whole-mode engine reads the finished recording, so its input kind
    /// decides whether the pipeline hands it samples or a WAV file.
    fn whole<E>(self, config: E::Config) -> Result<RuntimeExecutor, DictationInitializationError>
    where
        E: Engine,
        E::Config: Clone + Send + Sync + 'static,
    {
        let model = self.loading.load::<E>(config)?;
        Ok(self.finish::<E, Whole>(Transcription::complete(E::INPUT, model)))
    }

    fn streaming<E>(
        self,
        config: E::Config,
    ) -> Result<RuntimeExecutor, DictationInitializationError>
    where
        E: StreamingEngine,
        E::Config: Clone + Send + Sync + 'static,
    {
        let model = self.loading.load::<E>(config)?;
        let transcription = Transcription::stream(E::STREAM, model);
        Ok(self.finish::<E, Streaming>(transcription))
    }

    /// The one place the concrete pipeline becomes a boxed executor, so the
    /// engine, its mode, and its cadence stay static until here.
    fn finish<E, M>(self, transcription: Transcription<ModelLoad<E>, M>) -> RuntimeExecutor
    where
        E: Engine,
        M: Mode,
    {
        Box::new(EngineExecution::<E, M> {
            pipeline: DictationPipeline::new(
                self.capture,
                transcription,
                speaker::reader(&self.speaker),
                (
                    History::new(self.transcripts, E::NAME),
                    Cursor::new(self.delivery),
                ),
            )
            .cleaning(self.cleanup)
            .backtracking(self.backtrack)
            .with(self.cleanup_llm),
            wake: self.wake,
            speaker: self.speaker,
            vocabulary: self.vocabulary,
        })
    }
}

pub(super) struct BuiltDictation {
    pub(super) coordinator: DictationCoordinator<RuntimeExecutor>,
    pub(super) wake: Option<WakeWordMonitor>,
    /// The one capture backend, for the other operations that record.
    pub(super) capture: Arc<CaptureBackend>,
    pub(super) wake_control: Option<WakeWordControl>,
}

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
    let (capture, wake) = build_capture(capture, wake, spool_dir, timeouts.recording)?;
    let capture = Arc::new(capture);
    let wake_control = wake.as_ref().map(WakeWordMonitor::control);
    let delivery = PlatformTextDelivery::new(timeouts.delivery)
        .map_err(DictationInitializationError::Delivery)?;
    let policy =
        DictationPolicy::new(timeouts.overall).map_err(DictationInitializationError::Policy)?;
    let execution = ExecutionParts {
        capture: Arc::clone(&capture),
        delivery,
        transcripts,
        wake: wake_control.clone(),
        loading,
        speaker,
        vocabulary,
        cleanup,
        backtrack,
        cleanup_llm,
    }
    .build(transcription)?;
    Ok(BuiltDictation {
        coordinator: DictationCoordinator::new(policy, execution, microphone),
        wake,
        capture,
        wake_control,
    })
}

pub(super) fn transcribe_retry(
    config: DictationConfig,
    speaker: &SpeakerConfig,
    vocabulary: &VocabularyRuntime,
    home: &Path,
    recording: &mut RecordingReader,
    settings: &DictationSettings,
    cancel: &CancelToken,
) -> Result<Transcript, DictationError> {
    let cleanup_llm = config
        .cleanup_layer(home)
        .map_err(|error| DictationError::Transcription(error.into()))?;
    let transcription = match config.transcription {
        TranscriptionConfig::Whisper { model } => {
            retry_model::<Whisper>(WhisperConfig::new(model), cancel)
        }
        TranscriptionConfig::WhisperCandle { model_directory } => {
            retry_model::<WhisperCandle>(WhisperCandleConfig::new(model_directory), cancel)
        }
        TranscriptionConfig::Parakeet {
            model_directory,
            device,
        } => retry_model::<Parakeet>(ParakeetConfig::new(model_directory, device), cancel),
        TranscriptionConfig::Moonshine { model_directory } => {
            retry_model::<Moonshine>(MoonshineConfig::new(model_directory), cancel)
        }
        TranscriptionConfig::Nemotron { model_directory } => {
            retry_model::<Nemotron>(NemotronConfig::new(model_directory), cancel)
        }
        #[cfg(target_os = "macos")]
        TranscriptionConfig::ParakeetMlx { model, interpreter } => {
            retry_model::<ParakeetMlx>(ParakeetMlxConfig { model, interpreter }, cancel)
        }
        #[cfg(not(target_os = "macos"))]
        TranscriptionConfig::ParakeetMlx { .. } => Err(TranscriptionError::UnsupportedBackend {
            backend: crate::config::backend::PARAKEET_MLX,
        }),
    }
    .map_err(DictationError::from)?;
    let snapshot = vocabulary.snapshot(settings.project.as_ref());
    let pipeline = DictationPipeline::new((), transcription, speaker::reader(speaker), ())
        .cleaning(config.cleanup)
        .backtracking(config.backtrack)
        .with(cleanup_llm);
    pipeline.refresh_enrolled(speaker::enrolled(speaker));
    pipeline.transcribe(
        recording,
        &settings.recognition_language,
        settings.own_voice_only,
        Some(snapshot.matcher.as_ref()),
        cancel,
    )
}

fn retry_model<E: Engine>(
    config: E::Config,
    cancel: &CancelToken,
) -> Result<Transcription<Box<dyn crate::transcription::Transcriber>, Whole>, TranscriptionError> {
    let model: Box<dyn crate::transcription::Transcriber> = Box::new(E::load_from(config, cancel)?);
    Ok(Transcription::complete(E::INPUT, model))
}

fn build_capture(
    capture: CaptureConfig,
    wake: WakeConfig,
    spool_dir: &Path,
    recording_timeout: std::time::Duration,
) -> Result<(CaptureBackend, Option<WakeWordMonitor>), DictationInitializationError> {
    match wake {
        WakeConfig::Disabled => CaptureBackend::new(capture, spool_dir, recording_timeout)
            .map(|capture| (capture, None))
            .map_err(DictationInitializationError::Capture),
        WakeConfig::Enabled {
            backend,
            classifier,
            threshold,
        } => {
            if !matches!(capture, CaptureConfig::Microphone) {
                return Err(DictationInitializationError::Config(
                    ConfigError::WakeRequiresMicrophone,
                ));
            }
            let (capture, monitor) = match backend {
                WakeBackend::OpenWakeWord => {
                    let config = OpenWakeWordConfig::new(classifier, threshold)
                        .map_err(DictationInitializationError::WakeConfiguration)?;
                    let detector = OpenWakeWord::load_from(&config)
                        .map_err(DictationInitializationError::WakeModel)?;
                    build_wake_word_capture(detector, spool_dir, recording_timeout)
                }
                WakeBackend::LiveKit => {
                    let config = LiveKitConfig::new(classifier, threshold)
                        .map_err(DictationInitializationError::WakeConfiguration)?;
                    let detector = LiveKitWakeWord::load_from(&config)
                        .map_err(DictationInitializationError::WakeModel)?;
                    build_wake_word_capture(detector, spool_dir, recording_timeout)
                }
            }?;
            Ok((capture, Some(monitor)))
        }
    }
}

fn build_wake_word_capture<W>(
    detector: W,
    spool_dir: &Path,
    recording_timeout: std::time::Duration,
) -> Result<(CaptureBackend, WakeWordMonitor), DictationInitializationError>
where
    W: aria_kit::wake_word::WakeWordDetector + Send + 'static,
{
    CaptureBackend::with_wake_word(detector, spool_dir, recording_timeout)
        .map_err(DictationInitializationError::Capture)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            crate::vocabulary::extract::LocalExtractor,
        );
        let audio = crate::audio::canonical_fixture(home.path());
        let mut recording = RecordingReader::from_wav_file(&audio).unwrap();
        let settings = crate::preferences::UserPreferences::default()
            .resolve_dictation(crate::dictation::DictationRequest::default());
        let result = transcribe_retry(
            config,
            &speaker,
            &vocabulary,
            home.path(),
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
