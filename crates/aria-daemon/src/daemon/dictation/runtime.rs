//! Dictation initialization state and request execution owned by the daemon.

use super::super::context::Context;
use super::super::subscriptions::Subscriptions;
use super::super::transcripts::TranscriptStore;
use super::super::vocabulary::Runtime as VocabularyRuntime;
use super::assembly::{
    BuiltDictation, DictationInitializationError, EngineSlot, ModelLoading, RetryTarget,
    RuntimeExecutor, build_dictation, transcribe_retry,
};
use super::follower::EngineFollower;
use crate::daemon::config::WakeConfig;
use crate::daemon::inference::server_pin;
use aria_kit::audio::{RecordingReader, WakeWordMonitor};
use aria_kit::cancel::{CancelToken, Deadline};
use aria_platform::delivery::DeliveryFactory;
use aria_primitives::lazy::Lazy;
use aria_settings::config::{ConfigError, backend};
use aria_settings::event::EventEmitter;
use aria_settings::rpc::{Categorized, RpcErrorCode};
use aria_settings::{MAX_DICTATION_TIMEOUT, SettingsCache, SettingsStore, StoreError};
use aria_voice::config::{DictationConfig, SpeakerConfig, TranscriptionBackend};
use aria_voice::dictation::{
    DictationCoordinator, DictationError, DictationEvent, DictationEventEmitter, DictationOutcome,
    DictationRequest, DictationSettings, DictationStage, RecordingId, StopMode, Transcribed,
    subscribers::DictationSubscribers,
};
use aria_voice::history::{HistoryEntry, HistoryError, HistorySource, RetryOptions};
use aria_voice::microphone::Gate;
use aria_voice::transcription::Language;
use std::sync::Arc;

const CALL_EVENT_THREAD: &str = "aria-dictation-call";

/// The engine a deferred runtime names before settings are read. A retry
/// that asks for it runs on the configured backend.
const DEFERRED_ENGINE: &str = "local";

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
    pub(in crate::daemon) wake: Option<aria_kit::audio::WakeWordControl>,
}

/// Whether the stored auto-stop may end a manual recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum AutoStopRule {
    Setting,
    /// A hold: its release is the stop.
    Off,
}

pub(in crate::daemon) struct DictationRuntime {
    emitter: EventEmitter<DictationEvent>,
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
    slot: EngineSlot,
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
        let sound_muting = context.sound_muting();
        let retry_home = home.clone();
        let retry_vocabulary = Arc::clone(&vocabulary);
        let retry_cache = Arc::clone(&cache);
        Self {
            retry: Lazy::new(move || {
                let settings = retry_cache.effective();
                Ok(RetrySetup {
                    dictation: DictationConfig::load(
                        &settings,
                        &retry_home,
                        server_pin(&retry_home),
                    ),
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
                    let config = DictationConfig::load(&settings, &home, server_pin(&home));
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
                        sound_muting,
                    )
                    .map(|built| ReadyDictation {
                        coordinator: built.coordinator,
                        slot: built.slot,
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
                    aria_kit::audio::CaptureError::SourceGone,
                ))
            }))),
        }
    }

    #[cfg(test)]
    pub(in crate::daemon) fn execute_with(&mut self, executor: RuntimeExecutor) {
        let policy = aria_voice::dictation::DictationPolicy::new(MAX_DICTATION_TIMEOUT)
            .expect("the longest timeout is a valid policy");
        let slot = EngineSlot::of(DEFERRED_ENGINE, executor);
        let execution: RuntimeExecutor = Box::new(slot.clone());
        self.coordinator = CoordinatorState::Ready(ReadyDictation {
            coordinator: DictationCoordinator::new(policy, execution, Arc::clone(&self.microphone)),
            slot,
        });
    }

    /// Startup waits for the model here, so a daemon that cannot transcribe
    /// says so before it accepts a press. The caller runs the returned monitor
    /// and engine follower. A runtime dictates but watches neither the wake
    /// word nor settings.
    pub(in crate::daemon) fn eager(
        context: &Context<'_>,
        subscriptions: Arc<Subscriptions>,
        settings: Arc<SettingsStore>,
        cache: Arc<SettingsCache>,
        transcripts: Arc<TranscriptStore>,
        setup: DictationSetup,
        microphone: Arc<Gate>,
    ) -> Result<
        (Self, Option<WakeWordMonitor>, Recorder, EngineFollower),
        DictationInitializationError,
    > {
        let retry_dictation = setup.dictation.clone();
        let retry_speaker = setup.speaker.clone();
        let retry_vocabulary = Arc::clone(&setup.vocabulary);
        let retry_home = context.home.to_path_buf();
        let BuiltDictation {
            slot,
            parts,
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
            context.sound_muting(),
        )?;
        let runtime = Self {
            emitter: event_emitter(context, subscriptions),
            settings,
            coordinator: CoordinatorState::Ready(ReadyDictation {
                coordinator,
                slot: slot.clone(),
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
            microphone: Arc::clone(&microphone),
        };
        let follower = EngineFollower::new(slot, parts, microphone, cache, context.home.clone());
        Ok((
            runtime,
            monitor,
            Recorder {
                capture,
                wake: wake_control,
            },
            follower,
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

    pub(in crate::daemon) fn emitter_for_call(
        &self,
        sink: impl Fn(DictationStage) + Send + 'static,
    ) -> DictationEventEmitter {
        DictationEventEmitter::subscribed_by(CALL_EVENT_THREAD, move |event: DictationEvent| {
            // Some callers reject unknown progress stages. Voice feedback
            // and queued segments reach the overlay subscription instead.
            if let DictationEvent::Stage(stage) = event
                && !matches!(
                    stage,
                    DictationStage::SpeakerChanged(_) | DictationStage::SegmentQueued
                )
            {
                sink(stage);
            }
        })
    }

    fn operation_emitter(&self, sink: Option<&DictationEventEmitter>) -> DictationEventEmitter {
        let daemon = self.emitter.clone();
        let sink = sink.cloned();
        DictationEventEmitter::forwarded_by(move |event: DictationEvent| {
            daemon.emit(event.clone());
            if let Some(sink) = &sink {
                sink.emit(event);
            }
        })
    }

    /// A retry re-transcribes a stored recording, so no app is in front.
    pub(in crate::daemon) fn history_settings(
        &self,
    ) -> Result<DictationSettings, DictationRuntimeError> {
        self.settings_for(None, HistorySource::Local)
    }

    /// A phone's dictation lands in whatever app is in front on this Mac.
    pub(in crate::daemon) fn relay_settings(
        &self,
    ) -> Result<DictationSettings, DictationRuntimeError> {
        self.settings_for(
            aria_platform::delivery::frontmost_application(),
            HistorySource::Relay,
        )
    }

    fn settings_for(
        &self,
        application: Option<aria_platform::delivery::Application>,
        source: HistorySource,
    ) -> Result<DictationSettings, DictationRuntimeError> {
        let snapshot = self
            .settings
            .snapshot()
            .map_err(DictationRuntimeError::Settings)?;
        let mut settings = DictationSettings::resolve(
            snapshot.settings,
            DictationRequest::new(StopMode::OnSilence),
            application,
        );
        settings.source = source;
        Ok(settings)
    }

    /// Announces a failure the pipeline never saw: a relay job that panicked.
    pub(in crate::daemon) fn announce_failure(&self, code: RpcErrorCode) {
        self.emitter.emit(DictationStage::Failed(code));
    }

    /// Delivers a recording captured elsewhere through the press pipeline.
    /// The same stages reach every subscriber, so the Mac's status pill
    /// follows a relayed recording as it follows a press.
    pub(in crate::daemon) fn dictate_recording(
        &self,
        recording: &mut RecordingReader,
        settings: DictationSettings,
        cancel: CancelToken,
    ) -> Result<String, DictationRuntimeError> {
        if cancel.is_cancelled() {
            return Err(DictationError::Cancelled.into());
        }
        let events = self.operation_emitter(None);
        let coordinator = match self.loaded() {
            Ok(ready) => &ready.coordinator,
            Err(error) => {
                events.emit(DictationStage::Failed((&error).into()));
                return Err(error.into());
            }
        };
        coordinator
            .deliver_recording(recording, settings, cancel, &events)
            .map_err(DictationRuntimeError::Execution)
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
        self.loaded()?
            .coordinator
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
            let settings = self.history_settings().map_err(transcription)?;
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
                store.attempt_failed(id, &error)?;
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
        let running = self.ready().map(|ready| ready.slot.current());
        if running
            .as_ref()
            .is_some_and(|engine| engine.name == requested || engine.name == backend::SERVER)
        {
            let transcribed = self
                .transcribe(recording, settings, cancel)
                .map_err(transcription)?;
            if names_an_engine(requested) && transcribed.engine != requested {
                return Err(HistoryError::transcription(
                    RpcErrorCode::TranscriptionFailed,
                    EngineMismatch {
                        requested: requested.to_owned(),
                        ran: transcribed.engine,
                    },
                ));
            }
            return Ok(transcribed);
        }
        let setup = self.retry.get().map_err(|error| {
            HistoryError::transcription(RpcErrorCode::TranscriptionFailed, error)
        })?;
        // A swap may have changed the engine since the retry setup was read.
        let backend = running.map_or_else(
            || setup.dictation.transcription.clone(),
            |engine| engine.backend.clone(),
        );
        let target = retry_target(&backend, &setup.home, requested)?;
        transcribe_retry(setup, target, recording, settings, cancel).map_err(transcription)
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
        let events = self.operation_emitter(events);
        let ready = match self.loaded() {
            Ok(ready) => ready,
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
        let mut settings = DictationSettings::resolve(
            snapshot.settings,
            request,
            aria_platform::delivery::frontmost_application(),
        );
        match auto_stop {
            AutoStopRule::Setting => {}
            AutoStopRule::Off => settings.auto_stop = None,
        }

        ready
            .coordinator
            .execute(settings, cancellation, &events)
            .map_err(DictationRuntimeError::Execution)
    }

    fn loaded(&self) -> Result<&ReadyDictation, DictationInitializationError> {
        match &self.coordinator {
            CoordinatorState::Ready(ready) => Ok(ready),
            CoordinatorState::Deferred(deferred) => deferred.get(),
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
    backend: &TranscriptionBackend,
    home: &std::path::Path,
    requested: &'a str,
) -> Result<RetryTarget<'a>, HistoryError> {
    let named = names_an_engine(requested).then_some(requested);
    match (backend, named) {
        (TranscriptionBackend::Server(_), engine) => Ok(RetryTarget::Server { engine }),
        (TranscriptionBackend::Local(local), None) => Ok(RetryTarget::Local(local.clone())),
        (TranscriptionBackend::Local(local), Some(engine)) => local
            .for_backend(engine, home)
            .map(RetryTarget::Local)
            .map_err(|error| HistoryError::transcription(RpcErrorCode::TranscriptionFailed, error)),
    }
}

fn transcription<E>(error: E) -> HistoryError
where
    E: Categorized + Send + Sync + 'static,
{
    HistoryError::transcription(error.category(), error)
}

#[derive(Debug, thiserror::Error)]
#[error("the retry ran on {ran}, not the requested {requested}")]
struct EngineMismatch {
    requested: String,
    ran: &'static str,
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
            aria_settings::Directory::new(path.clone())
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
            DictationRuntimeError::Settings(error) => error.into(),
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
) -> EventEmitter<DictationEvent> {
    let local = context
        .attended
        .then(|| DictationSubscribers::new(context.paths.status_path()));
    forward_events(subscriptions, move |event| {
        if let (Some(local), DictationEvent::Stage(stage)) = (&local, &event) {
            local.observe(stage);
        }
    })
}

fn forward_events(
    subscriptions: Arc<Subscriptions>,
    local: impl Fn(DictationEvent) + Send + 'static,
) -> EventEmitter<DictationEvent> {
    let local = EventEmitter::subscribed_by(aria_voice::dictation::EVENT_THREAD, local);
    EventEmitter::forwarded_by(move |event: DictationEvent| match &event {
        DictationEvent::Level(_) | DictationEvent::Ghost(_) => {
            subscriptions.broadcast_lossy(event);
        }
        DictationEvent::Stage(_) => {
            subscriptions.broadcast(event.clone());
            local.emit(event);
        }
        DictationEvent::Caption(_) => subscriptions.broadcast(event),
    })
}

#[cfg(test)]
mod event_tests {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;

    use crate::daemon::subscriptions::EVENT_BUFFER;

    use super::*;

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
        emitter.emit(DictationStage::Recording {
            id: RecordingId(1),
            delivery: aria_settings::DeliveryMode::Complete,
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        let voice = DictationStage::SpeakerChanged(aria_voice::dictation::HeardSpeaker::KeptOut);
        emitter.emit(voice.clone());
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
            emitter.emit(aria_voice::dictation::MicLevel {
                recording: RecordingId(1),
                level: 0.5,
            });
        }
        emitter.emit(DictationStage::Transcribing);
        let closed = tokio::time::timeout(Duration::from_millis(100), async {
            while stream.next().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "a full subscription outlived a stage event");
    }

    #[test]
    fn voice_updates_and_queued_segments_reach_subscribers_without_changing_call_progress() {
        let directory = tempfile::tempdir().unwrap();
        let settings =
            Arc::new(SettingsStore::open(directory.path().join("settings.toml")).unwrap());
        let mut runtime = DictationRuntime::detached(settings);
        let (broadcast, received) = channel();
        runtime.emitter = EventEmitter::forwarded_by(move |event: DictationEvent| {
            broadcast.send(event).unwrap();
        });
        let (call, progress) = channel();
        let events = runtime.emitter_for_call(move |stage| call.send(stage).unwrap());
        let operation = runtime.operation_emitter(Some(&events));
        let voice = DictationStage::SpeakerChanged(aria_voice::dictation::HeardSpeaker::KeptOut);
        operation.emit(voice.clone());
        operation.emit(DictationStage::SegmentQueued);
        operation.emit(DictationStage::Transcribing);
        assert_eq!(received.recv().unwrap(), DictationEvent::Stage(voice));
        assert_eq!(
            received.recv().unwrap(),
            DictationEvent::Stage(DictationStage::SegmentQueued)
        );
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
        let first = runtime.operation_emitter(None);
        let second = runtime.operation_emitter(None);
        let delivered = DictationStage::Delivered {
            outcome: None,
            words: 0,
        };
        let recording = DictationStage::Recording {
            id: RecordingId(2),
            delivery: aria_settings::DeliveryMode::Complete,
        };
        first.emit(delivered.clone());
        second.emit(recording.clone());

        for stage in [delivered, recording] {
            let (thread, event) = received.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(thread, std::thread::current().id());
            assert_eq!(event, DictationEvent::Stage(stage));
        }
    }

    #[test]
    fn delayed_events_reach_each_sink_once() {
        let directory = tempfile::tempdir().unwrap();
        let settings =
            Arc::new(SettingsStore::open(directory.path().join("settings.toml")).unwrap());
        let mut runtime = DictationRuntime::detached(settings);
        let (entered, waiting) = channel();
        let (release, blocked) = channel();
        let (daemon, daemon_events) = channel();
        runtime.emitter = EventEmitter::subscribed_by("aria-test-delayed-events", move |event| {
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
        runtime.emitter.emit(DictationStage::Delivered {
            outcome: None,
            words: 0,
        });
        drop(events);
        drop(runtime);
        for _ in 0..3 {
            release.send(()).unwrap();
        }
        let mut failures = 0;
        let mut relay = 0;
        for _ in 0..3 {
            match daemon_events.recv_timeout(Duration::from_secs(2)).unwrap() {
                DictationEvent::Stage(DictationStage::Failed(_)) => failures += 1,
                DictationEvent::Stage(DictationStage::Delivered { .. }) => relay += 1,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(failures, 2);
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
    struct AutoStopSpy(Arc<std::sync::Mutex<Vec<Option<aria_kit::audio::AutoStop>>>>);

    impl aria_voice::dictation::DictationExecutor for AutoStopSpy {
        fn execute(
            &self,
            settings: DictationSettings,
            _: &Arc<aria_voice::dictation::ExecutionControl>,
            _: &DictationEventEmitter,
        ) -> Result<aria_voice::dictation::DictationResult, DictationError> {
            self.0.lock().unwrap().push(settings.auto_stop);
            Err(DictationError::NoSpeech)
        }

        fn transcribe(
            &self,
            _: &mut RecordingReader,
            _: &Language,
            _: bool,
            _: Option<&aria_settings::Directory>,
            _: aria_voice::dictation::Rewriting,
            _: &CancelToken,
        ) -> Result<Transcribed, DictationError> {
            panic!("a press never retries history");
        }

        fn deliver_recording(
            &self,
            _: &mut RecordingReader,
            _: DictationSettings,
            _: &Arc<aria_voice::dictation::ExecutionControl>,
            _: &DictationEventEmitter,
        ) -> Result<String, DictationError> {
            panic!("this test never delivers a relayed recording");
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
            [Some(aria_kit::audio::AutoStop::default()), None]
        );
    }
}

#[cfg(test)]
mod history_tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use aria_settings::Settings;
    use aria_voice::dictation::{
        DictationExecutor, DictationPolicy, DictationResult, ExecutionControl,
    };
    use aria_voice::history::{AudioAvailability, HistorySource, HistoryStatus};

    use super::*;

    struct RetrySpy {
        seen: Arc<Mutex<Vec<(String, bool)>>>,
        cancel: Option<CancelToken>,
        silent: bool,
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
            project: Option<&aria_settings::Directory>,
            _: aria_voice::dictation::Rewriting,
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
            if self.silent {
                return Err(DictationError::NoSpeech);
            }
            Ok(Transcribed {
                transcript: aria_voice::transcription::Transcript::new("recovered words").unwrap(),
                engine: "test",
            })
        }

        fn deliver_recording(
            &self,
            _: &mut RecordingReader,
            _: DictationSettings,
            _: &Arc<aria_voice::dictation::ExecutionControl>,
            _: &DictationEventEmitter,
        ) -> Result<String, DictationError> {
            panic!("this test never delivers a relayed recording");
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
            Self::build(cancel, false)
        }

        fn silent() -> Self {
            Self::build(None, true)
        }

        fn build(cancel: Option<CancelToken>, silent: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let transcripts = Arc::new(TranscriptStore::open(&directory.path().join("history")));
            let audio = aria_kit::audio::canonical_fixture(directory.path());
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
                silent,
            });
            let slot = EngineSlot::of("test", executor);
            let execution: RuntimeExecutor = Box::new(slot.clone());
            let coordinator = DictationCoordinator::new(
                DictationPolicy::new(MAX_DICTATION_TIMEOUT).unwrap(),
                execution,
                Arc::clone(&microphone),
            );
            Self {
                _directory: directory,
                original,
                seen,
                runtime: DictationRuntime {
                    emitter: EventEmitter::unsubscribed(),
                    settings,
                    coordinator: CoordinatorState::Ready(ReadyDictation { coordinator, slot }),
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
            .modify(aria_settings::SettingsPatch {
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
            .modify(aria_settings::SettingsPatch {
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
    fn a_retry_that_hears_no_speech_reports_no_speech() {
        let fixture = Fixture::silent();
        let Err(error) = fixture.runtime.retry_history(
            fixture.original.id,
            RetryOptions::default(),
            CancelToken::never(),
        ) else {
            panic!("a silent retry must fail");
        };
        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::NoSpeech);
        let entry = fixture
            .runtime
            .transcripts
            .get(fixture.original.id)
            .unwrap();
        assert_eq!(
            entry.error.map(|failure| failure.code),
            Some(RpcErrorCode::NoSpeech)
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
            Err(HistoryError::Transcription { .. })
        ));
        let unchanged = fixture
            .runtime
            .transcripts
            .get(fixture.original.id)
            .unwrap();
        assert_eq!(unchanged.text, fixture.original.text);
        assert_eq!(unchanged.engine, fixture.original.engine);
        assert_eq!(unchanged.language, fixture.original.language);
        assert_eq!(
            unchanged.error,
            Some(aria_settings::rpc::RpcFailure::new(
                RpcErrorCode::TranscriptionFailed,
                "dictation history transcription failed"
            ))
        );
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
                .modify(aria_settings::SettingsPatch {
                    history_keep_audio: Some(true),
                    ..Default::default()
                })
                .unwrap();
            let home_dir = home.clone();
            let mut config =
                DictationConfig::load(&aria_settings::testing::file(""), &home_dir, None);
            config.transcription =
                TranscriptionBackend::Server(crate::daemon::inference::server_pin(&home_dir));
            config.capture = aria_voice::config::CaptureConfig::WavFile {
                path: audio,
                pace: aria_kit::audio::ReplayPace::Immediate,
            };
            let speaker =
                SpeakerConfig::load(&aria_settings::SettingsFile::default().speaker, &home_dir);
            let (vocabulary, _worker) = VocabularyRuntime::new(
                home.join("vocabulary"),
                None,
                false,
                Arc::clone(&fixture.runtime.settings),
                aria_voice::vocabulary::discover::LocalCollector::new(
                    aria_voice::vocabulary::extract::LocalExtractor,
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
                aria_voice::dictation::SoundMuting::Never,
            )
            .unwrap();
            assert_eq!(built.slot.current().name, backend::SERVER);
            fixture.runtime.coordinator = CoordinatorState::Ready(ReadyDictation {
                coordinator: built.coordinator,
                slot: built.slot,
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
                Err(HistoryError::Transcription { .. })
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
        assert_eq!(entry.error, Some((&HistoryError::Cancelled).into()));
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
                aria_settings::Directory::new(directory.path().to_path_buf()).unwrap(),
            ),
            ..Default::default()
        };
        let mut original = entry();
        original.own_voice_only = false;
        let resolved = retry_settings(
            &original,
            DictationSettings::resolve(
                current.clone(),
                DictationRequest::new(StopMode::OnSilence),
                None,
            ),
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
                DictationSettings::resolve(
                    current,
                    DictationRequest::new(StopMode::OnSilence),
                    None,
                ),
                None,
            ),
            Err(HistoryError::ProjectUnavailable)
        ));
        assert!(matches!(
            check_retry(&CancelToken::never(), Deadline::after(Duration::ZERO)),
            Err(HistoryError::Timeout)
        ));
    }
}
