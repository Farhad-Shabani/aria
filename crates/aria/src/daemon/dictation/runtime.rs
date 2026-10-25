//! Dictation initialization state and request execution owned by the daemon.

use std::sync::Arc;

use aria_kit::cancel::{CancelToken, Deadline};

use super::super::vocabulary::Runtime as VocabularyRuntime;
use crate::audio::{RecordingReader, WakeWordMonitor};
use crate::config::{
    ConfigError, DictationConfig, SpeakerConfig, TranscriptionBackend, WakeConfig, backend,
};
use crate::dictation::subscribers::DictationSubscribers;
use crate::dictation::{
    DictationCoordinator, DictationError, DictationEvent, DictationEventEmitter, DictationOutcome,
    DictationRequest, DictationSettings, DictationStage, MAX_DICTATION_TIMEOUT, RecordingId,
    StopMode, Transcribed,
};
use crate::event::EventEmitter;
use crate::history::{HistoryEntry, HistoryError, RetryOptions};
use crate::microphone::Gate;
use crate::preferences::{PreferenceStore, PreferenceStoreError};
use crate::primitives::lazy::Lazy;
use crate::rpc::RpcErrorCode;
use crate::transcription::Language;

use super::super::context::Context;
use super::super::subscriptions::Subscriptions;
use super::super::transcripts::{History, TranscriptStore};
use super::assembly::{
    BuiltDictation, DictationInitializationError, ModelLoading, RetryTarget, RuntimeExecutor,
    build_dictation, transcribe_retry,
};

const CALL_EVENT_THREAD: &str = "aria-dictation-call";

/// The engine a deferred runtime names before configuration is read. A retry
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
}

/// The microphone parts every other recording operation borrows: the one
/// capture backend, and the wake word's attention to suspend.
pub(in crate::daemon) struct Recorder {
    pub(in crate::daemon) capture: Arc<crate::daemon::capture::CaptureBackend>,
    pub(in crate::daemon) wake: Option<crate::audio::WakeWordControl>,
}

pub(in crate::daemon) struct DictationRuntime {
    emitter: EventEmitter<DaemonEvent>,
    preferences: Arc<PreferenceStore>,
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
    /// Configuration and the model are read when the coordinator is built, so
    /// a fault in either reaches the press that triggered it and is announced.
    pub(in crate::daemon) fn deferred(
        context: &Context<'_>,
        subscriptions: Arc<Subscriptions>,
        preferences: Arc<PreferenceStore>,
        transcripts: Arc<TranscriptStore>,
        microphone: Arc<Gate>,
        vocabulary: Arc<VocabularyRuntime>,
    ) -> Self {
        let spool_dir = context.paths.spool_dir().to_path_buf();
        let config_environment = context.config_environment.clone();
        let retry_environment = config_environment.clone();
        let retry_vocabulary = Arc::clone(&vocabulary);
        Self {
            retry: Lazy::new(move || {
                Ok(RetrySetup {
                    dictation: DictationConfig::load(&retry_environment)?,
                    speaker: SpeakerConfig::load(&retry_environment)?,
                    vocabulary: Arc::clone(&retry_vocabulary),
                    home: retry_environment.home().to_path_buf(),
                })
            }),
            emitter: event_emitter(context, subscriptions),
            transcripts: Arc::clone(&transcripts),
            microphone: Arc::clone(&microphone),
            preferences,
            coordinator: CoordinatorState::Deferred(Box::new(DeferredCoordinator::new(
                move || {
                    let config = DictationConfig::load(&config_environment)
                        .map_err(DictationInitializationError::Config)?;
                    let speaker = SpeakerConfig::load(&config_environment)
                        .map_err(DictationInitializationError::Config)?;
                    // Only a daemon that deferred its build reaches here, and
                    // one of those lives for a single press. A wake-word monitor
                    // would have nothing to serve, so none is built whatever
                    // the configuration says.
                    build_dictation(
                        DictationSetup {
                            dictation: config,
                            wake: WakeConfig::Disabled,
                            speaker,
                            vocabulary: Arc::clone(&vocabulary),
                        },
                        &spool_dir,
                        config_environment.home(),
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
    pub(in crate::daemon) fn detached(preferences: Arc<PreferenceStore>) -> Self {
        Self {
            emitter: EventEmitter::unsubscribed(),
            transcripts: Arc::new(TranscriptStore::new()),
            microphone: Gate::new(),
            retry: Lazy::new(|| Err(ConfigError::WakeRequiresMicrophone)),
            preferences,
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
        preferences: Arc<PreferenceStore>,
        transcripts: Arc<TranscriptStore>,
        setup: DictationSetup,
        microphone: Arc<Gate>,
    ) -> Result<(Self, Option<WakeWordMonitor>, Recorder), DictationInitializationError> {
        let retry_dictation = setup.dictation.clone();
        let retry_speaker = setup.speaker.clone();
        let retry_vocabulary = Arc::clone(&setup.vocabulary);
        let retry_home = context.config_environment.home().to_path_buf();
        let BuiltDictation {
            engine,
            coordinator,
            wake: monitor,
            capture,
            wake_control,
        } = build_dictation(
            setup,
            context.paths.spool_dir(),
            context.config_environment.home(),
            Arc::clone(&transcripts),
            ModelLoading::Eager,
            Arc::clone(&microphone),
        )?;
        let runtime = Self {
            emitter: event_emitter(context, subscriptions),
            preferences,
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
            if let DictationEvent::Stage(stage) = event {
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
        self.preferences
            .snapshot()
            .map(|snapshot| {
                snapshot
                    .preferences
                    .resolve_dictation(DictationRequest::new(StopMode::OnSilence), application)
            })
            .map_err(DictationRuntimeError::Preferences)
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
                &settings.recognition_language,
                settings.own_voice_only,
                settings.project.as_ref(),
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
                    settings.recognition_language.as_str(),
                    settings.history_audio_retention,
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

    /// The live coordinator serves a retry for its own engine. Anything else
    /// builds a one-off pipeline, so a retry never initializes capture.
    fn retry_on(
        &self,
        requested: &str,
        recording: &mut RecordingReader,
        settings: &DictationSettings,
        cancel: &CancelToken,
    ) -> Result<Transcribed, HistoryError> {
        if self.ready().is_some_and(|ready| ready.engine == requested) {
            return self
                .transcribe(recording, settings, cancel)
                .map_err(|_| HistoryError::Transcription);
        }
        let setup = self.retry.get().map_err(|_| HistoryError::Transcription)?;
        let target = retry_target(setup, requested)?;
        transcribe_retry(setup, target, recording, settings, cancel)
            .map_err(|_| HistoryError::Transcription)
    }

    pub(in crate::daemon) fn execute(
        &self,
        request: DictationRequest,
        cancellation: CancelToken,
        events: Option<&DictationEventEmitter>,
    ) -> Result<DictationOutcome, DictationRuntimeError> {
        if cancellation.is_cancelled() {
            return Err(DictationRuntimeError::Execution(DictationError::Cancelled));
        }
        let snapshot = self
            .preferences
            .snapshot()
            .map_err(DictationRuntimeError::Preferences)?;
        let events = self.operation_emitter(snapshot.preferences.mute_output, events);
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
        let settings = snapshot
            .preferences
            .resolve_dictation(request, crate::delivery::frontmost_application());

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

/// A placeholder name asks for whatever is configured: the deferred one, or
/// the server's before it named its engine.
fn retry_target<'a>(
    setup: &RetrySetup,
    requested: &'a str,
) -> Result<RetryTarget<'a>, HistoryError> {
    let named = (requested != DEFERRED_ENGINE && requested != backend::SERVER).then_some(requested);
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
    settings.recognition_language = if let Some(language) = language {
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
    #[error("failed to read preferences")]
    Preferences(#[source] PreferenceStoreError),

    #[error(transparent)]
    Initialization(#[from] DictationInitializationError),

    #[error(transparent)]
    Execution(#[from] DictationError),
}

impl From<&DictationRuntimeError> for RpcErrorCode {
    fn from(error: &DictationRuntimeError) -> Self {
        match error {
            // Reading preferences is the daemon's own fault, not the caller's.
            DictationRuntimeError::Preferences(_) => Self::Internal,
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
    EventEmitter::subscribed_by(
        crate::dictation::EVENT_THREAD,
        move |message: DaemonEvent| {
            let event = message.event;
            if let (Some(local), DictationEvent::Stage(stage)) = (&local, &event) {
                local.observe(stage, message.suppress_cues);
            }
            if matches!(event, DictationEvent::Level(_) | DictationEvent::Ghost(_)) {
                subscriptions.broadcast_lossy(event);
            } else {
                subscriptions.broadcast(event);
            }
        },
    )
}

#[cfg(test)]
mod event_tests {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;

    use crate::preferences::PreferencePatch;

    use super::*;

    #[test]
    fn successive_operations_enqueue_before_returning_to_the_caller() {
        let directory = tempfile::tempdir().unwrap();
        let preferences =
            Arc::new(PreferenceStore::open(directory.path().join("preferences.json")).unwrap());
        let mut runtime = DictationRuntime::detached(preferences);
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
        let preferences =
            Arc::new(PreferenceStore::open(directory.path().join("preferences.json")).unwrap());
        preferences
            .update(
                0,
                PreferencePatch {
                    mute_output: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut runtime = DictationRuntime::detached(Arc::clone(&preferences));
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
                    CancelToken::never(),
                    Some(&events)
                )
                .is_err()
        );
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        preferences
            .update(
                1,
                PreferencePatch {
                    mute_output: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            runtime
                .execute(
                    DictationRequest::new(StopMode::OnSilence),
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
}

#[cfg(test)]
mod history_tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::dictation::{DictationExecutor, DictationPolicy, DictationResult, ExecutionControl};
    use crate::history::{AudioAvailability, HistorySource, HistoryStatus};
    use crate::preferences::UserPreferences;

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
            let preferences =
                Arc::new(PreferenceStore::open(directory.path().join("preferences.json")).unwrap());
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
                    preferences,
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
            .preferences
            .update(
                0,
                crate::preferences::PreferencePatch {
                    dictation_history: Some(false),
                    ..Default::default()
                },
            )
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
            .preferences
            .update(
                0,
                crate::preferences::PreferencePatch {
                    history_audio_retention: Some(true),
                    ..Default::default()
                },
            )
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
                .preferences
                .update(
                    0,
                    crate::preferences::PreferencePatch {
                        history_audio_retention: Some(true),
                        ..Default::default()
                    },
                )
                .unwrap();
            let environment = crate::config::Environment::in_home(home.clone());
            let mut config = DictationConfig::load(&environment).unwrap();
            config.transcription = TranscriptionBackend::Server { endpoint };
            config.capture = crate::config::CaptureConfig::WavFile {
                path: audio,
                pace: crate::audio::ReplayPace::Immediate,
            };
            let speaker = SpeakerConfig::load(&environment).unwrap();
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
                if named.is_none() {
                    assert!(result.text.starts_with("native words"));
                    assert_eq!(observed.native.load(Ordering::SeqCst), 1);
                    assert_eq!(observed.calls.load(Ordering::SeqCst), 0);
                } else {
                    assert_eq!(result.text, "raw model words");
                    assert_eq!(observed.calls.load(Ordering::SeqCst), 1);
                    assert_eq!(*observed.languages.lock().unwrap(), ["fr"]);
                }
            }
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
        let current = UserPreferences {
            own_voice_only: true,
            project_vocabulary: true,
            vocabulary_directory: Some(
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
