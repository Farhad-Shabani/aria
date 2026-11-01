//! Dictation initialization state and request execution owned by the daemon.

use std::sync::Arc;

use aria_kit::cancel::{CancelToken, Deadline};

use super::super::vocabulary::Runtime as VocabularyRuntime;
use crate::audio::{RecordingReader, WakeWordMonitor};
use crate::config::{
    ConfigError, DictationConfig, SpeakerConfig, TranscriptionBackend, WakeConfig, backend,
};
use crate::delivery::DeliveryFactory;
use crate::dictation::subscribers::DictationSubscribers;
use crate::dictation::{
    DictationCoordinator, DictationError, DictationEvent, DictationEventEmitter, DictationOutcome,
    DictationRequest, DictationSettings, DictationStage, MAX_DICTATION_TIMEOUT, RecordingId,
    StopMode, Transcribed,
};
use crate::event::EventEmitter;
use crate::history::{HistoryEntry, HistoryError, RetryOptions};
use crate::microphone::Gate;
use crate::primitives::lazy::Lazy;
use crate::rpc::RpcErrorCode;
use crate::settings::SettingsCache;
use crate::settings::{SettingsStore, StoreError};
use crate::transcription::Language;

use super::super::context::Context;
use super::super::subscriptions::Subscriptions;
use super::super::transcripts::{History, TranscriptStore};
use super::assembly::{
    BuiltDictation, DictationInitializationError, ModelLoading, RetryTarget, RuntimeExecutor,
    build_dictation, transcribe_retry,
};

const CALL_EVENT_THREAD: &str = "aria-dictation-call";

/// The engine a deferred runtime names before settings are read. A retry
/// that asks for it runs on the configured backend.
const DEFERRED_ENGINE: &str = "local";

#[derive(Clone)]
struct DaemonEvent {
    event: DictationEvent,
    suppress_cues: bool,
}

/// What one dictation runtime is built from. Read together at startup, so
/// they travel together.
pub(in crate::daemon) struct DictationSetup {
    pub(in crate::daemon) dictation: DictationConfig,
    pub(in crate::daemon) wake: WakeConfig,
    pub(in crate::daemon) speaker: SpeakerConfig,
    pub(in crate::daemon) vocabulary: Arc<VocabularyRuntime>,
    pub(in crate::daemon) delivery: DeliveryFactory,
}

/// The microphone parts every other recording operation borrows: the one
/// capture backend, and the wake word's attention to suspend.
pub(in crate::daemon) struct Recorder {
    pub(in crate::daemon) capture: Arc<crate::daemon::capture::CaptureBackend>,
    pub(in crate::daemon) wake: Option<crate::audio::WakeWordControl>,
}

/// Whether the stored auto-stop may end a manual recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum AutoStopRule {
    Setting,
    /// A hold: its release is the stop.
    Off,
}

pub(in crate::daemon) struct DictationRuntime {
    emitter: EventEmitter<DaemonEvent>,
    settings: Arc<SettingsStore>,
    coordinator: CoordinatorState,
    transcripts: Arc<TranscriptStore>,
    microphone: Arc<Gate>,
    retry: Lazy<RetrySetup, ConfigError>,
}

pub(super) struct RetrySetup {
    pub(super) dictation: DictationConfig,
    pub(super) speaker: SpeakerConfig,
    pub(super) vocabulary: Arc<VocabularyRuntime>,
    pub(super) home: std::path::PathBuf,
}

struct ReadyDictation {
    coordinator: DictationCoordinator<RuntimeExecutor>,
    engine: &'static str,
}

/// A coordinator the daemon already holds, or how to build one when the first
/// press asks for it.
enum CoordinatorState {
    Ready(ReadyDictation),
    /// Boxed: the empty slot inside is as wide as the coordinator it will
    /// hold, and a ready runtime should not pay for that.
    Deferred(Box<DeferredCoordinator>),
}

type DeferredCoordinator = Lazy<ReadyDictation, DictationInitializationError>;

impl DictationRuntime {
    /// The emitter exists from daemon construction, not from the first press.
    /// Building the local subscribers clears a stale status file at startup,
    /// and a press that fails before the pipeline exists still has somewhere
    /// to announce it.
    ///
    /// Settings and the model are read when the coordinator is built, so
    /// a fault in either reaches the press that triggered it and is announced.
    pub(in crate::daemon) fn deferred(
        context: &Context<'_>,
        subscriptions: Arc<Subscriptions>,
        settings: Arc<SettingsStore>,
        cache: Arc<SettingsCache>,
        transcripts: Arc<TranscriptStore>,
        microphone: Arc<Gate>,
        vocabulary: Arc<VocabularyRuntime>,
    ) -> Self {
        let delivery = context.delivery.clone();
        let spool_dir = context.paths.spool_dir().to_path_buf();
        let home = context.home.clone();
        let retry_home = home.clone();
        let retry_vocabulary = Arc::clone(&vocabulary);
        let retry_cache = Arc::clone(&cache);
        Self {
            retry: Lazy::new(move || {
                let settings = retry_cache.effective();
                Ok(RetrySetup {
                    dictation: DictationConfig::load(&settings, &retry_home)?,
                    speaker: SpeakerConfig::load(&settings.speaker, &retry_home),
                    vocabulary: Arc::clone(&retry_vocabulary),
                    home: retry_home.clone(),
                })
            }),
            emitter: event_emitter(context, subscriptions),
            transcripts: Arc::clone(&transcripts),
            microphone: Arc::clone(&microphone),
            settings,
            coordinator: CoordinatorState::Deferred(Box::new(DeferredCoordinator::new(
                move || {
                    let settings = cache.effective();
                    let config = DictationConfig::load(&settings, &home)
                        .map_err(DictationInitializationError::Config)?;
                    let speaker = SpeakerConfig::load(&settings.speaker, &home);
                    // Only a daemon that deferred its build reaches here, and
                    // one of those lives for a single press. A wake-word monitor
                    // would have nothing to serve, so none is built whatever
                    // the settings say.
                    build_dictation(
                        DictationSetup {
                            dictation: config,
                            wake: WakeConfig::Disabled,
                            speaker,
                            vocabulary: Arc::clone(&vocabulary),
                            delivery: delivery.clone(),
                        },
                        &spool_dir,
                        &home,
                        Arc::clone(&transcripts),
                        ModelLoading::OnFirstUse,
                        Arc::clone(&microphone),
                    )
                    .map(|built| ReadyDictation {
                        coordinator: built.coordinator,
                        engine: built.engine,
                    })
                },
            ))),
        }
    }

    /// A runtime nobody listens to and no press can build; every call fails
    /// before it records.
    #[cfg(test)]
    pub(in crate::daemon) fn detached(settings: Arc<SettingsStore>) -> Self {
        Self {
            emitter: EventEmitter::unsubscribed(),
            transcripts: Arc::new(TranscriptStore::new()),
            microphone: Gate::new(),
            retry: Lazy::new(|| Err(ConfigError::WakeRequiresMicrophone)),
            settings,
            coordinator: CoordinatorState::Deferred(Box::new(DeferredCoordinator::new(|| {
                Err(DictationInitializationError::Capture(
                    crate::audio::CaptureError::SourceGone,
                ))
            }))),
        }
    }

    #[cfg(test)]
    pub(in crate::daemon) fn execute_with(&mut self, executor: RuntimeExecutor) {
        let policy = crate::dictation::DictationPolicy::new(MAX_DICTATION_TIMEOUT)
            .expect("the longest timeout is a valid policy");
        self.coordinator = CoordinatorState::Ready(ReadyDictation {
            coordinator: DictationCoordinator::new(policy, executor, Arc::clone(&self.microphone)),
            engine: DEFERRED_ENGINE,
        });
    }

    /// Startup waits for the model here, so a daemon that cannot transcribe
    /// says so before it accepts a press. The monitor comes back for the
    /// caller to run: a runtime dictates; it does not watch for the wake word.
    pub(in crate::daemon) fn eager(
        context: &Context<'_>,
        subscriptions: Arc<Subscriptions>,
        settings: Arc<SettingsStore>,
        transcripts: Arc<TranscriptStore>,
        setup: DictationSetup,
        microphone: Arc<Gate>,
    ) -> Result<(Self, Option<WakeWordMonitor>, Recorder), DictationInitializationError> {
        let retry_dictation = setup.dictation.clone();
        let retry_speaker = setup.speaker.clone();
        let retry_vocabulary = Arc::clone(&setup.vocabulary);
        let retry_home = context.home.to_path_buf();
        let BuiltDictation {
            engine,
            coordinator,
            wake: monitor,
            capture,
            wake_control,
        } = build_dictation(
            setup,
            context.paths.spool_dir(),
            &context.home,
            Arc::clone(&transcripts),
            ModelLoading::Eager,
            Arc::clone(&microphone),
        )?;
        let runtime = Self {
            emitter: event_emitter(context, subscriptions),
            settings,
            coordinator: CoordinatorState::Ready(ReadyDictation {
                coordinator,
                engine,
            }),
            retry: Lazy::new(move || {
                Ok(RetrySetup {
                    dictation: retry_dictation.clone(),
                    speaker: retry_speaker.clone(),
                    vocabulary: Arc::clone(&retry_vocabulary),
                    home: retry_home.clone(),
                })
            }),
            transcripts,
            microphone,
        };
        Ok((
            runtime,
            monitor,
            Recorder {
                capture,
                wake: wake_control,
            },
        ))
    }

    pub(in crate::daemon) fn stop_manual_recording(&self) -> bool {
        self.microphone.stop_manual_recording()
    }

    /// Ends the named recording and lets transcription finish. Returns false
    /// if it is not running, without loading a deferred coordinator.
    pub(in crate::daemon) fn stop_recording(&self, recording: RecordingId) -> bool {
        self.ready()
            .is_some_and(|ready| ready.coordinator.stop_recording(recording))
    }

    /// Announces a stage on the same channel a press uses, so the Mac's
    /// status pill and captions see a relayed recording too. Callers never
    /// announce Recording: nothing was captured here.
    pub(in crate::daemon) fn announce(&self, stage: DictationStage) {
        self.emitter.emit(DaemonEvent {
            event: stage.into(),
            suppress_cues: false,
        });
    }

    pub(in crate::daemon) fn emitter_for_call(
        &self,
        sink: impl Fn(DictationStage) + Send + 'static,
    ) -> DictationEventEmitter {
        DictationEventEmitter::subscribed_by(CALL_EVENT_THREAD, move |event: DictationEvent| {
            // Some callers reject unknown progress stages. Voice feedback
            // uses the overlay subscription.
            if let DictationEvent::Stage(stage) = event
                && !matches!(stage, DictationStage::SpeakerChanged(_))
            {
                sink(stage);
            }
        })
    }

    fn operation_emitter(
        &self,
        suppress_cues: bool,
        sink: Option<&DictationEventEmitter>,
    ) -> DictationEventEmitter {
        let daemon = self.emitter.clone();
        let sink = sink.cloned();
        DictationEventEmitter::forwarded_by(move |event: DictationEvent| {
            daemon.emit(DaemonEvent {
                event: event.clone(),
                suppress_cues,
            });
            if let Some(sink) = &sink {
                sink.emit(event);
            }
        })
    }

    /// A retry re-transcribes a stored recording, so no app is in front.
    pub(in crate::daemon) fn history_settings(
        &self,
    ) -> Result<DictationSettings, DictationRuntimeError> {
        self.settings_for(None)
    }

    /// A phone's dictation lands in whatever app is in front on this Mac.
    pub(in crate::daemon) fn relay_settings(
        &self,
    ) -> Result<DictationSettings, DictationRuntimeError> {
        self.settings_for(crate::delivery::frontmost_application())
    }

    fn settings_for(
        &self,
        application: Option<crate::history::Application>,
    ) -> Result<DictationSettings, DictationRuntimeError> {
        self.settings
            .snapshot()
            .map(|snapshot| {
                snapshot
                    .settings
                    .resolve_dictation(DictationRequest::new(StopMode::OnSilence), application)
            })
            .map_err(DictationRuntimeError::Settings)
    }

    pub(in crate::daemon) fn history(&self) -> History {
        let engine = self.ready().map_or(DEFERRED_ENGINE, |ready| ready.engine);
        History::new(Arc::clone(&self.transcripts), engine)
    }

    pub(in crate::daemon) fn transcribe(
        &self,
        recording: &mut RecordingReader,
        settings: &DictationSettings,
        cancel: &CancelToken,
    ) -> Result<Transcribed, DictationRuntimeError> {
        if cancel.is_cancelled() {
            return Err(DictationError::Cancelled.into());
        }
        self.coordinator()?
            .transcribe(
                recording,
                &settings.stt_language,
                settings.own_voice_only,
                settings.project.as_ref(),
                settings.rewriting,
                cancel,
            )
            .map_err(DictationRuntimeError::Execution)
    }

    pub(in crate::daemon) fn retry_history(
        &self,
        id: u64,
        options: RetryOptions,
        cancel: CancelToken,
    ) -> Result<HistoryEntry, HistoryError> {
        let deadline = Deadline::after(MAX_DICTATION_TIMEOUT);
        check_retry(&cancel, deadline)?;
        let store = self.transcripts.storage()?;
        let _admission = self
            .microphone
            .admit_history_retry()
            .ok_or(HistoryError::Busy)?;
        let mut lease = store.begin_retry(id)?;
        let requested = options.engine.unwrap_or_else(|| lease.entry.engine.clone());
        let attempt = (|| {
            let settings = self
                .history_settings()
                .map_err(|_| HistoryError::Unavailable)?;
            let settings = retry_settings(&lease.entry, settings, options.language)?;
            check_retry(&cancel, deadline)?;
            let request = cancel.clone();
            let operation =
                CancelToken::from_fn(move || request.is_cancelled() || deadline.expired());
            let retried = self.retry_on(&requested, &mut lease.recording, &settings, &operation);
            check_retry(&cancel, deadline)?;
            Ok((retried?, settings))
        })();
        match attempt {
            Ok((retried, settings)) => {
                let mut recovered = store.recover(
                    id,
                    retried.transcript.as_str(),
                    retried.engine,
                    settings.stt_language.as_str(),
                    settings.history_keep_audio,
                )?;
                drop(lease);
                recovered.retrying = false;
                Ok(recovered)
            }
            Err(error) => {
                store.attempt_failed(id, error)?;
                Err(error)
            }
        }
    }

    /// The live coordinator serves a retry for its own engine, or for any
    /// engine its server runs. Anything else builds a one-off pipeline, so a
    /// retry never initializes capture.
    fn retry_on(
        &self,
        requested: &str,
        recording: &mut RecordingReader,
        settings: &DictationSettings,
        cancel: &CancelToken,
    ) -> Result<Transcribed, HistoryError> {
        if self
            .ready()
            .is_some_and(|ready| ready.engine == requested || ready.engine == backend::SERVER)
        {
            let transcribed = self
                .transcribe(recording, settings, cancel)
                .map_err(|_| HistoryError::Transcription)?;
            if names_an_engine(requested) && transcribed.engine != requested {
                return Err(HistoryError::Transcription);
            }
            return Ok(transcribed);
        }
        let setup = self.retry.get().map_err(|_| HistoryError::Transcription)?;
        let target = retry_target(setup, requested)?;
        transcribe_retry(setup, target, recording, settings, cancel)
            .map_err(|_| HistoryError::Transcription)
    }

    pub(in crate::daemon) fn execute(
        &self,
        request: DictationRequest,
        auto_stop: AutoStopRule,
        cancellation: CancelToken,
        events: Option<&DictationEventEmitter>,
    ) -> Result<DictationOutcome, DictationRuntimeError> {
        if cancellation.is_cancelled() {
            return Err(DictationRuntimeError::Execution(DictationError::Cancelled));
        }
        let snapshot = self
            .settings
            .snapshot()
            .map_err(DictationRuntimeError::Settings)?;
        let events = self.operation_emitter(snapshot.settings.mute_sound, events);
        let coordinator = match self.coordinator() {
            Ok(coordinator) => coordinator,
            Err(_) if cancellation.is_cancelled() => {
                return Err(DictationRuntimeError::Execution(DictationError::Cancelled));
            }
            Err(error) => {
                events.emit(DictationStage::Failed((&error).into()));
                return Err(error.into());
            }
        };
        // Read once the coordinator is built, which may load a model, so the
        // app judged is the one in front as recording starts.
        let mut settings = snapshot
            .settings
            .resolve_dictation(request, crate::delivery::frontmost_application());
        match auto_stop {
            AutoStopRule::Setting => {}
            AutoStopRule::Off => settings.auto_stop = None,
        }

        coordinator
            .execute(settings, cancellation, &events)
            .map_err(DictationRuntimeError::Execution)
    }

    fn coordinator(
        &self,
    ) -> Result<&DictationCoordinator<RuntimeExecutor>, DictationInitializationError> {
        match &self.coordinator {
            CoordinatorState::Ready(ready) => Ok(&ready.coordinator),
            CoordinatorState::Deferred(deferred) => deferred.get().map(|ready| &ready.coordinator),
        }
    }

    /// The coordinator once it exists, without building a deferred one.
    fn ready(&self) -> Option<&ReadyDictation> {
        match &self.coordinator {
            CoordinatorState::Ready(ready) => Some(ready),
            CoordinatorState::Deferred(deferred) => deferred.built(),
        }
    }
}

/// `DEFERRED_ENGINE` and `backend::SERVER` label a dictation, not an engine,
/// so a retry of either runs on whatever is configured.
fn names_an_engine(label: &str) -> bool {
    label != DEFERRED_ENGINE && label != backend::SERVER
}

fn retry_target<'a>(
    setup: &RetrySetup,
    requested: &'a str,
) -> Result<RetryTarget<'a>, HistoryError> {
    let named = names_an_engine(requested).then_some(requested);
    match (&setup.dictation.transcription, named) {
        (TranscriptionBackend::Server { endpoint }, engine) => Ok(RetryTarget::Server {
            endpoint: endpoint.clone(),
            engine,
        }),
        (TranscriptionBackend::Local(local), None) => Ok(RetryTarget::Local(local.clone())),
        (TranscriptionBackend::Local(local), Some(engine)) => local
            .for_backend(engine, &setup.home)
            .map(RetryTarget::Local)
            .map_err(|_| HistoryError::Transcription),
    }
}

fn check_retry(cancel: &CancelToken, deadline: Deadline) -> Result<(), HistoryError> {
    if cancel.is_cancelled() {
        return Err(HistoryError::Cancelled);
    }
    if deadline.expired() {
        return Err(HistoryError::Timeout);
    }
    Ok(())
}

fn retry_settings(
    entry: &HistoryEntry,
    mut settings: DictationSettings,
    language: Option<Language>,
) -> Result<DictationSettings, HistoryError> {
    settings.stt_language = if let Some(language) = language {
        language
    } else {
        entry
            .language
            .parse()
            .map_err(|_| HistoryError::InvalidRequest)?
    };
    settings.own_voice_only |= entry.own_voice_only;
    settings.project = entry
        .project
        .as_ref()
        .map(|path| {
            if !path.is_dir() {
                return Err(HistoryError::ProjectUnavailable);
            }
            crate::vocabulary::Directory::new(path.clone())
                .map_err(|_| HistoryError::ProjectUnavailable)
        })
        .transpose()?;
    Ok(settings)
}

#[derive(Debug, thiserror::Error)]
pub(in crate::daemon) enum DictationRuntimeError {
    #[error("failed to read settings")]
    Settings(#[source] StoreError),

    #[error(transparent)]
    Initialization(#[from] DictationInitializationError),

    #[error(transparent)]
    Execution(#[from] DictationError),
}

impl From<&DictationRuntimeError> for RpcErrorCode {
    fn from(error: &DictationRuntimeError) -> Self {
        match error {
            // Reading settings is the daemon's own fault, not the caller's.
            DictationRuntimeError::Settings(_) => Self::Internal,
            DictationRuntimeError::Initialization(error) => error.into(),
            DictationRuntimeError::Execution(error) => error.into(),
        }
    }
}

/// Nobody watches an unattended run, so it subscribes no sound, banner, or
/// status file. A connected client asked to be told, and still is.
fn event_emitter(
    context: &Context<'_>,
    subscriptions: Arc<Subscriptions>,
) -> EventEmitter<DaemonEvent> {
    let local = context
        .attended
        .then(|| DictationSubscribers::new(context.paths.status_path()));
    forward_events(subscriptions, move |message| {
        if let (Some(local), DictationEvent::Stage(stage)) = (&local, &message.event) {
            local.observe(stage, message.suppress_cues);
        }
    })
}

fn forward_events(
    subscriptions: Arc<Subscriptions>,
    local: impl Fn(DaemonEvent) + Send + 'static,
) -> EventEmitter<DaemonEvent> {
    let local = EventEmitter::subscribed_by(crate::dictation::EVENT_THREAD, local);
    EventEmitter::forwarded_by(move |message: DaemonEvent| match &message.event {
        DictationEvent::Level(_) | DictationEvent::Ghost(_) => {
            subscriptions.broadcast_lossy(message.event);
        }
        DictationEvent::Stage(_) => {
            subscriptions.broadcast(message.event.clone());
            local.emit(message);
        }
        DictationEvent::Caption(_) => subscriptions.broadcast(message.event),
    })
}

#[cfg(test)]
mod event_tests {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;

    use crate::daemon::subscriptions::EVENT_BUFFER;
    use crate::settings::SettingsPatch;

    use super::*;

    fn uncued(event: impl Into<DictationEvent>) -> DaemonEvent {
        DaemonEvent {
            event: event.into(),
            suppress_cues: true,
        }
    }

    #[tokio::test]
    async fn stage_events_reach_subscribers_while_local_feedback_is_blocked() {
        use tokio_stream::StreamExt;

        let subscriptions = Arc::new(Subscriptions::new());
        let mut stream = subscriptions.subscribe().unwrap();
        let (entered, waiting) = channel();
        let (release, blocked) = channel::<()>();
        let emitter = forward_events(Arc::clone(&subscriptions), move |_| {
            if entered.send(()).is_ok() {
                let _ = blocked.recv();
            }
        });
        emitter.emit(uncued(DictationStage::Recording(RecordingId(1))));
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        let voice = DictationStage::SpeakerChanged(crate::dictation::HeardSpeaker::KeptOut);
        emitter.emit(uncued(voice.clone()));
        let observed = tokio::time::timeout(Duration::from_millis(100), async {
            stream.next().await.unwrap().unwrap();
            stream.next().await.unwrap().unwrap()
        })
        .await;
        drop(release);
        assert_eq!(observed.unwrap(), crate::ipc::wire::Event::from(voice));
    }

    #[tokio::test]
    async fn a_stage_event_closes_a_subscription_full_of_levels() {
        use tokio_stream::StreamExt;

        let subscriptions = Arc::new(Subscriptions::new());
        let mut stream = subscriptions.subscribe().unwrap();
        let emitter = forward_events(Arc::clone(&subscriptions), |_| {});
        for _level in 0..EVENT_BUFFER {
            emitter.emit(uncued(crate::dictation::MicLevel {
                recording: RecordingId(1),
                level: 0.5,
            }));
        }
        emitter.emit(uncued(DictationStage::Transcribing));
        let closed = tokio::time::timeout(Duration::from_millis(100), async {
            while stream.next().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "a full subscription outlived a stage event");
    }

    #[test]
    fn voice_updates_reach_subscribers_without_changing_call_progress() {
        let directory = tempfile::tempdir().unwrap();
        let settings =
            Arc::new(SettingsStore::open(directory.path().join("settings.toml")).unwrap());
        let mut runtime = DictationRuntime::detached(settings);
        let (broadcast, received) = channel();
        runtime.emitter = EventEmitter::forwarded_by(move |message: DaemonEvent| {
            broadcast.send(message.event).unwrap();
        });
        let (call, progress) = channel();
        let events = runtime.emitter_for_call(move |stage| call.send(stage).unwrap());
        let operation = runtime.operation_emitter(false, Some(&events));
        let voice = DictationStage::SpeakerChanged(crate::dictation::HeardSpeaker::KeptOut);
        operation.emit(voice.clone());
        operation.emit(DictationStage::Transcribing);
        assert_eq!(received.recv().unwrap(), DictationEvent::Stage(voice));
        assert_eq!(
            received.recv().unwrap(),
            DictationEvent::Stage(DictationStage::Transcribing)
        );
        assert_eq!(
            progress.recv_timeout(Duration::from_secs(2)).unwrap(),
            DictationStage::Transcribing
        );
    }

    #[test]
    fn successive_operations_enqueue_before_returning_to_the_caller() {
        let directory = tempfile::tempdir().unwrap();
        let settings =
            Arc::new(SettingsStore::open(directory.path().join("settings.toml")).unwrap());
        let mut runtime = DictationRuntime::detached(settings);
        let (sent, received) = channel();
        runtime.emitter = EventEmitter::forwarded_by(move |message| {
            sent.send((std::thread::current().id(), message)).unwrap();
        });
        let first = runtime.operation_emitter(true, None);
        let second = runtime.operation_emitter(false, None);
        let delivered = DictationStage::Delivered {
            outcome: None,
            words: 0,
        };
        let recording = DictationStage::Recording(RecordingId(2));
        first.emit(delivered.clone());
        second.emit(recording.clone());

        for (stage, suppress_cues) in [(delivered, true), (recording, false)] {
            let (thread, message) = received.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(thread, std::thread::current().id());
            assert_eq!(message.event, DictationEvent::Stage(stage));
            assert_eq!(message.suppress_cues, suppress_cues);
        }
    }

    #[test]
    fn delayed_events_keep_operation_policy_and_reach_each_sink_once() {
        let directory = tempfile::tempdir().unwrap();
        let settings =
            Arc::new(SettingsStore::open(directory.path().join("settings.toml")).unwrap());
        settings
            .modify(SettingsPatch {
                mute_sound: Some(true),
                ..Default::default()
            })
            .unwrap();
        let mut runtime = DictationRuntime::detached(Arc::clone(&settings));
        let (entered, waiting) = channel();
        let (release, blocked) = channel();
        let (daemon, daemon_events) = channel();
        runtime.emitter = EventEmitter::subscribed_by("aria-test-cue-policy", move |event| {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
            daemon.send(event).unwrap();
        });
        let (call, call_events) = channel();
        let events = runtime.emitter_for_call(move |stage| call.send(stage).unwrap());
        assert!(
            runtime
                .execute(
                    DictationRequest::new(StopMode::OnSilence),
                    AutoStopRule::Setting,
                    CancelToken::never(),
                    Some(&events)
                )
                .is_err()
        );
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        settings
            .modify(SettingsPatch {
                mute_sound: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert!(
            runtime
                .execute(
                    DictationRequest::new(StopMode::OnSilence),
                    AutoStopRule::Setting,
                    CancelToken::never(),
                    Some(&events)
                )
                .is_err()
        );
        runtime.announce(DictationStage::Delivered {
            outcome: None,
            words: 0,
        });
        drop(events);
        drop(runtime);
        for _ in 0..3 {
            release.send(()).unwrap();
        }
        let mut failures = Vec::new();
        let mut relay = 0;
        for _ in 0..3 {
            let message = daemon_events.recv_timeout(Duration::from_secs(2)).unwrap();
            match message.event {
                DictationEvent::Stage(DictationStage::Failed(_)) => {
                    failures.push(message.suppress_cues);
                }
                DictationEvent::Stage(DictationStage::Delivered { .. }) => {
                    assert!(!message.suppress_cues);
                    relay += 1;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        failures.sort();
        assert_eq!(failures, [false, true]);
        assert_eq!(relay, 1);
        assert!(matches!(
            daemon_events.recv_timeout(Duration::from_secs(2)),
            Err(RecvTimeoutError::Disconnected)
        ));
        for _ in 0..2 {
            assert!(matches!(
                call_events.recv_timeout(Duration::from_secs(2)).unwrap(),
                DictationStage::Failed(_)
            ));
        }
        assert!(matches!(
            call_events.recv_timeout(Duration::from_secs(2)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    /// Records the auto-stop each press runs with, then ends the press.
    struct AutoStopSpy(Arc<std::sync::Mutex<Vec<Option<crate::audio::AutoStop>>>>);

    impl crate::dictation::DictationExecutor for AutoStopSpy {
        fn execute(
            &self,
            settings: DictationSettings,
            _: &Arc<crate::dictation::ExecutionControl>,
            _: &DictationEventEmitter,
        ) -> Result<crate::dictation::DictationResult, DictationError> {
            self.0.lock().unwrap().push(settings.auto_stop);
            Err(DictationError::NoSpeech)
        }

        fn transcribe(
            &self,
            _: &mut RecordingReader,
            _: &Language,
            _: bool,
            _: Option<&crate::vocabulary::Directory>,
            _: crate::dictation::Rewriting,
            _: &CancelToken,
        ) -> Result<Transcribed, DictationError> {
            panic!("a press never retries history");
        }
    }

    #[test]
    fn a_hold_runs_without_the_stored_auto_stop() {
        let directory = tempfile::tempdir().unwrap();
        let settings =
            Arc::new(SettingsStore::open(directory.path().join("settings.toml")).unwrap());
        let mut runtime = DictationRuntime::detached(settings);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        runtime.execute_with(Box::new(AutoStopSpy(Arc::clone(&seen))));

        for rule in [AutoStopRule::Setting, AutoStopRule::Off] {
            let _no_speech = runtime.execute(
                DictationRequest::new(StopMode::Manual),
                rule,
                CancelToken::never(),
                None,
            );
        }

        assert_eq!(
            *seen.lock().unwrap(),
            [Some(crate::audio::AutoStop::default()), None]
        );
    }
}

#[cfg(test)]
mod history_tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::dictation::{DictationExecutor, DictationPolicy, DictationResult, ExecutionControl};
    use crate::history::{AudioAvailability, HistorySource, HistoryStatus};
    use crate::settings::Settings;

    use super::*;

    struct RetrySpy {
        seen: Arc<Mutex<Vec<(String, bool)>>>,
        cancel: Option<CancelToken>,
    }

    impl DictationExecutor for RetrySpy {
        fn execute(
            &self,
            _: DictationSettings,
            _: &Arc<ExecutionControl>,
            _: &DictationEventEmitter,
        ) -> Result<DictationResult, DictationError> {
            panic!("history retry must not capture or deliver");
        }

        fn transcribe(
            &self,
            recording: &mut RecordingReader,
            language: &Language,
            own_voice_only: bool,
            project: Option<&crate::vocabulary::Directory>,
            _: crate::dictation::Rewriting,
            _: &CancelToken,
        ) -> Result<Transcribed, DictationError> {
            assert!(recording.ended_at().is_some());
            assert!(project.is_none());
            self.seen
                .lock()
                .unwrap()
                .push((language.as_str().to_owned(), own_voice_only));
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
                return Err(DictationError::Cancelled);
            }
            Ok(Transcribed {
                transcript: crate::transcription::Transcript::new("recovered words").unwrap(),
                engine: "test",
            })
        }
    }

    fn entry() -> HistoryEntry {
        HistoryEntry {
            id: 0,
            created_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            source: HistorySource::Local,
            application: None,
            duration_ms: Some(1_000),
            text: "partial".into(),
            complete: false,
            status: HistoryStatus::TranscriptionFailed,
            error: None,
            audio: AudioAvailability::Unavailable,
            retrying: false,
            deletion_pending: false,
            language: "fr".into(),
            engine: "test".into(),
            own_voice_only: true,
            project: None,
            warning: None,
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        runtime: DictationRuntime,
        original: HistoryEntry,
        seen: Arc<Mutex<Vec<(String, bool)>>>,
    }

    impl Fixture {
        fn new(cancel: Option<CancelToken>) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let transcripts = Arc::new(TranscriptStore::open(&directory.path().join("history")));
            let audio = crate::audio::canonical_fixture(directory.path());
            std::fs::set_permissions(&audio, std::fs::Permissions::from_mode(0o600)).unwrap();
            let original = transcripts
                .storage()
                .unwrap()
                .insert(entry(), Some(&audio))
                .unwrap();
            let settings =
                Arc::new(SettingsStore::open(directory.path().join("settings.toml")).unwrap());
            let microphone = Gate::new();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let executor: RuntimeExecutor = Box::new(RetrySpy {
                seen: Arc::clone(&seen),
                cancel,
            });
            let coordinator = DictationCoordinator::new(
                DictationPolicy::new(MAX_DICTATION_TIMEOUT).unwrap(),
                executor,
                Arc::clone(&microphone),
            );
            Self {
                _directory: directory,
                original,
                seen,
                runtime: DictationRuntime {
                    emitter: EventEmitter::unsubscribed(),
                    settings,
                    coordinator: CoordinatorState::Ready(ReadyDictation {
                        coordinator,
                        engine: "test",
                    }),
                    transcripts,
                    microphone,
                    retry: Lazy::new(|| Err(ConfigError::WakeRequiresMicrophone)),
                },
            }
        }
    }

    #[test]
    fn retry_recovers_the_same_entry_without_capture_or_delivery() {
        let fixture = Fixture::new(None);
        fixture
            .runtime
            .settings
            .modify(crate::settings::SettingsPatch {
                history_enabled: Some(false),
                ..Default::default()
            })
            .unwrap();
        let entry = fixture
            .runtime
            .retry_history(
                fixture.original.id,
                RetryOptions::default(),
                CancelToken::never(),
            )
            .unwrap();
        assert_eq!(entry.id, fixture.original.id);
        assert_eq!(entry.created_at_ms, fixture.original.created_at_ms);
        assert_eq!(entry.text, "recovered words");
        assert_eq!(entry.status, HistoryStatus::Recovered);
        assert!(entry.complete);
        assert!(!entry.retrying);
        assert!(!matches!(entry.audio, AudioAvailability::Available { .. }));
        assert_eq!(*fixture.seen.lock().unwrap(), [("fr".to_owned(), true)]);
        assert!(fixture.runtime.microphone.admit_relay().is_some());
    }

    #[test]
    fn language_override_is_saved_and_retained_audio_can_regenerate_again() {
        let fixture = Fixture::new(None);
        fixture
            .runtime
            .settings
            .modify(crate::settings::SettingsPatch {
                history_keep_audio: Some(true),
                ..Default::default()
            })
            .unwrap();
        let original_audio = fixture
            .runtime
            .transcripts
            .get(fixture.original.id)
            .unwrap()
            .audio;
        let options = RetryOptions {
            engine: Some("test".to_owned()),
            language: Some(Language::Automatic),
        };
        let updated = fixture
            .runtime
            .retry_history(fixture.original.id, options, CancelToken::never())
            .unwrap();
        assert_eq!(updated.language, "auto");
        assert_eq!(updated.engine, "test");
        assert_eq!(updated.audio, original_audio);
        let again = fixture
            .runtime
            .retry_history(
                fixture.original.id,
                RetryOptions::default(),
                CancelToken::never(),
            )
            .unwrap();
        assert_eq!(again.audio, original_audio);
        assert_eq!(
            *fixture.seen.lock().unwrap(),
            [("auto".to_owned(), true), ("auto".to_owned(), true)]
        );
    }

    #[test]
    fn failed_engine_override_preserves_original_text_and_metadata() {
        let fixture = Fixture::new(None);
        let options = RetryOptions {
            engine: Some("whisper".to_owned()),
            language: Some(Language::Automatic),
        };
        assert!(matches!(
            fixture
                .runtime
                .retry_history(fixture.original.id, options, CancelToken::never()),
            Err(HistoryError::Transcription)
        ));
        let unchanged = fixture
            .runtime
            .transcripts
            .get(fixture.original.id)
            .unwrap();
        assert_eq!(unchanged.text, fixture.original.text);
        assert_eq!(unchanged.engine, fixture.original.engine);
        assert_eq!(unchanged.language, fixture.original.language);
        assert_eq!(unchanged.error, Some(HistoryError::Transcription.into()));
        assert!(matches!(
            unchanged.audio,
            AudioAvailability::Available { .. }
        ));
        assert!(!unchanged.retrying);
        assert!(fixture.seen.lock().unwrap().is_empty());
        assert!(fixture.runtime.microphone.admit_relay().is_some());
    }

    #[tokio::test]
    async fn remote_history_retry_records_the_engine_the_server_ran() {
        use std::sync::atomic::Ordering;
        let server = crate::daemon::inference::host::tests::Fixture::start().await;
        let mut fixture = Fixture::new(None);
        let home = fixture._directory.path().to_owned();
        server.pair_home(home.clone()).await;
        let endpoint = server.endpoint().to_owned();
        let observed = Arc::clone(&server.observed);
        tokio::task::spawn_blocking(move || {
            let audio = home.join("speech.wav");
            std::fs::copy(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../aria-wake/tests/fixtures/negative.wav"),
                &audio,
            )
            .unwrap();
            std::fs::set_permissions(&audio, std::fs::Permissions::from_mode(0o600)).unwrap();
            let project = home.join("original-project");
            std::fs::create_dir(&project).unwrap();
            let mut original = entry();
            original.engine = backend::SERVER.into();
            original.own_voice_only = false;
            original.project = Some(project.clone());
            let original = fixture
                .runtime
                .transcripts
                .storage()
                .unwrap()
                .insert(original, Some(&audio))
                .unwrap();
            fixture
                .runtime
                .settings
                .modify(crate::settings::SettingsPatch {
                    history_keep_audio: Some(true),
                    ..Default::default()
                })
                .unwrap();
            let home_dir = home.clone();
            let mut config =
                DictationConfig::load(&crate::settings::SettingsFile::default(), &home_dir)
                    .unwrap();
            config.transcription = TranscriptionBackend::Server { endpoint };
            config.capture = crate::config::CaptureConfig::WavFile {
                path: audio,
                pace: crate::audio::ReplayPace::Immediate,
            };
            let speaker =
                SpeakerConfig::load(&crate::settings::SettingsFile::default().speaker, &home_dir);
            let (vocabulary, _worker) = VocabularyRuntime::new(
                home.join("vocabulary"),
                None,
                false,
                crate::vocabulary::discover::LocalCollector::new(
                    crate::vocabulary::extract::LocalExtractor,
                ),
            );
            let built = build_dictation(
                DictationSetup {
                    dictation: config.clone(),
                    speaker: speaker.clone(),
                    vocabulary: Arc::clone(&vocabulary),
                    wake: WakeConfig::Disabled,
                    delivery: DeliveryFactory::detect(&home),
                },
                &home,
                &home,
                Arc::clone(&fixture.runtime.transcripts),
                ModelLoading::Eager,
                Arc::clone(&fixture.runtime.microphone),
            )
            .unwrap();
            assert_eq!(built.engine, backend::SERVER);
            fixture.runtime.coordinator = CoordinatorState::Ready(ReadyDictation {
                coordinator: built.coordinator,
                engine: built.engine,
            });
            fixture.runtime.retry = Lazy::new(move || {
                Ok(RetrySetup {
                    dictation: config.clone(),
                    speaker: speaker.clone(),
                    vocabulary: Arc::clone(&vocabulary),
                    home: home.clone(),
                })
            });
            for named in [None, Some("whisper")] {
                let result = fixture
                    .runtime
                    .retry_history(
                        original.id,
                        RetryOptions {
                            engine: named.map(Into::into),
                            language: None,
                        },
                        CancelToken::never(),
                    )
                    .unwrap();
                assert_eq!(result.id, original.id);
                assert_eq!(result.created_at_ms, original.created_at_ms);
                assert_eq!(result.engine, "whisper");
                assert_eq!(result.language, "fr");
                assert_eq!(result.project.as_deref(), Some(project.as_path()));
                assert_eq!(result.audio, original.audio);
                assert_eq!(result.status, HistoryStatus::Recovered);
                assert!(!result.retrying);
                assert!(result.text.starts_with("native words"));
            }
            assert_eq!(observed.native.load(Ordering::SeqCst), 2);
            assert_eq!(observed.calls.load(Ordering::SeqCst), 0);
            assert!(matches!(
                fixture.runtime.retry_history(
                    original.id,
                    RetryOptions {
                        engine: Some("parakeet".into()),
                        language: None,
                    },
                    CancelToken::never(),
                ),
                Err(HistoryError::Transcription)
            ));
        })
        .await
        .unwrap();
        server.stop().await;
    }

    #[test]
    fn cancellation_preserves_partial_text_and_releases_audio_and_gate() {
        let cancel = CancelToken::new();
        let fixture = Fixture::new(Some(cancel.clone()));
        assert!(matches!(
            fixture
                .runtime
                .retry_history(fixture.original.id, RetryOptions::default(), cancel),
            Err(HistoryError::Cancelled)
        ));
        let entry = fixture
            .runtime
            .transcripts
            .get(fixture.original.id)
            .unwrap();
        assert_eq!(entry.text, "partial");
        assert_eq!(entry.error, Some(HistoryError::Cancelled.into()));
        assert_eq!(entry.status, HistoryStatus::TranscriptionFailed);
        assert!(!entry.retrying);
        assert!(matches!(entry.audio, AudioAvailability::Available { .. }));
        assert!(fixture.runtime.microphone.admit_relay().is_some());
        assert!(
            fixture
                .runtime
                .transcripts
                .storage()
                .unwrap()
                .begin_retry(entry.id)
                .is_ok()
        );
    }

    #[test]
    fn admission_failure_never_pins_audio_or_calls_the_engine() {
        let fixture = Fixture::new(None);
        let _claim = fixture.runtime.microphone.admit_enrollment().unwrap();
        assert!(matches!(
            fixture.runtime.retry_history(
                fixture.original.id,
                RetryOptions::default(),
                CancelToken::never()
            ),
            Err(HistoryError::Busy)
        ));
        assert!(
            !fixture
                .runtime
                .transcripts
                .get(fixture.original.id)
                .unwrap()
                .retrying
        );
        assert!(fixture.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn retry_preserves_original_context_and_never_weakens_voice_restriction() {
        let directory = tempfile::tempdir().unwrap();
        let current = Settings {
            own_voice_only: true,
            vocabulary_enabled: true,
            vocabulary_dir: Some(
                crate::vocabulary::Directory::new(directory.path().to_path_buf()).unwrap(),
            ),
            ..Default::default()
        };
        let mut original = entry();
        original.own_voice_only = false;
        let resolved = retry_settings(
            &original,
            current
                .clone()
                .resolve_dictation(DictationRequest::new(StopMode::OnSilence), None),
            None,
        )
        .unwrap();
        assert!(
            resolved.project.is_none(),
            "explicit original no-project stays disabled"
        );
        assert!(resolved.own_voice_only, "current stronger restriction wins");
        original.project = Some(directory.path().join("missing"));
        assert!(matches!(
            retry_settings(
                &original,
                current.resolve_dictation(DictationRequest::new(StopMode::OnSilence), None),
                None,
            ),
            Err(HistoryError::ProjectUnavailable)
        ));
        assert_eq!(
            check_retry(&CancelToken::never(), Deadline::after(Duration::ZERO)),
            Err(HistoryError::Timeout)
        );
    }
}
