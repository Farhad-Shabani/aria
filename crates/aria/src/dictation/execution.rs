use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aria_kit::cancel::{CancelToken, Deadline};

use crate::audio::{CaptureError, RecordingReader};
use crate::microphone::{Admitted, Claim, Gate};
use crate::primitives::error_chain;
use crate::rpc::RpcErrorCode;
use crate::transcription::{Language, Transcript, TranscriptionError};

#[cfg(target_os = "macos")]
use super::output::OutputMute;

use super::{
    DictationEventEmitter, DictationOutcome, DictationResult, DictationSettings, DictationStage,
    MAX_DICTATION_TIMEOUT, RecordingId, StopMode,
};

pub(crate) trait DictationExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Result<DictationResult, DictationError>;

    /// Hears a recording captured somewhere else — a phone or watch on the
    /// LAN — through the engine this executor already holds. No capture and
    /// no sinks: the caller brings the audio and takes the text.
    ///
    /// Applies the same speaker filtering as local dictation when enabled.
    fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        own_voice_only: bool,
        project: Option<&crate::vocabulary::Directory>,
        cancel: &CancelToken,
    ) -> Result<Transcript, DictationError>;
}

impl<E: DictationExecutor + ?Sized> DictationExecutor for Box<E> {
    fn execute(
        &self,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Result<DictationResult, DictationError> {
        (**self).execute(settings, control, events)
    }

    fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        own_voice_only: bool,
        project: Option<&crate::vocabulary::Directory>,
        cancel: &CancelToken,
    ) -> Result<Transcript, DictationError> {
        (**self).transcribe(recording, language, own_voice_only, project, cancel)
    }
}

/// Immutable policy applied to every execution admitted by a dictation coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DictationPolicy {
    overall_timeout: Duration,
}

impl DictationPolicy {
    pub(crate) fn new(overall_timeout: Duration) -> Result<Self, DictationPolicyError> {
        if overall_timeout.is_zero() || overall_timeout > MAX_DICTATION_TIMEOUT {
            return Err(DictationPolicyError::InvalidOverallTimeout {
                actual: overall_timeout,
                maximum: MAX_DICTATION_TIMEOUT,
            });
        }
        Ok(Self { overall_timeout })
    }

    fn deadline(self) -> Deadline {
        Deadline::after(self.overall_timeout)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DictationPolicyError {
    #[error(
        "overall dictation timeout must be greater than zero and at most {maximum:?}; got {actual:?}"
    )]
    InvalidOverallTimeout { actual: Duration, maximum: Duration },
}

// Several variants restate what a stage below already reported. That is
// deliberate. `status` and the RPC error code are derived from this enum, so a
// fault a client must distinguish needs its own variant here.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DictationError {
    #[error("another dictation operation is already active")]
    Busy,

    #[error("recording has already started for this dictation execution")]
    RecordingAlreadyStarted,

    #[error("streaming delivery requires a chunked transcription strategy")]
    StreamingDeliveryUnavailable,

    #[error("dictation execution was cancelled")]
    Cancelled,

    #[error("dictation execution exceeded its overall timeout of {0:?}")]
    OverallTimeout(std::time::Duration),

    #[error("recording exceeded its timeout of {0:?}")]
    RecordingTimeout(std::time::Duration),

    #[error("text delivery exceeded its timeout of {0:?}")]
    DeliveryTimeout(std::time::Duration),

    #[error("text delivery target changed before insertion")]
    DeliveryTargetChanged,

    #[error("typing stopped partway because focus or the caret moved")]
    DeliveryInterrupted,

    #[error("audio capture failed: {0}")]
    Capture(#[source] crate::audio::CaptureError),

    #[error("no speech was detected")]
    NoSpeech,

    #[error("transcription failed: {0}")]
    Transcription(#[source] crate::transcription::TranscriptionError),

    #[error("text delivery failed: {0}")]
    Delivery(#[source] crate::delivery::DeliveryError),
}

impl From<crate::delivery::DeliveryError> for DictationError {
    fn from(error: crate::delivery::DeliveryError) -> Self {
        use crate::delivery::DeliveryError;
        match error {
            DeliveryError::Timeout(timeout) => Self::DeliveryTimeout(timeout),
            DeliveryError::Cancelled => Self::Cancelled,
            DeliveryError::TargetChanged => Self::DeliveryTargetChanged,
            error => Self::Delivery(error),
        }
    }
}

/// What a fault below means for the press, before the press's own cancel
/// and deadline are taken into account.
impl From<TranscriptionError> for DictationError {
    fn from(error: TranscriptionError) -> Self {
        match error {
            TranscriptionError::Cancelled => Self::Cancelled,
            TranscriptionError::NoSpeech => Self::NoSpeech,
            TranscriptionError::Capture(error) => error.into(),
            error => Self::Transcription(error),
        }
    }
}

impl From<CaptureError> for DictationError {
    fn from(error: CaptureError) -> Self {
        match error {
            CaptureError::DeadlineExceeded(timeout) => Self::RecordingTimeout(timeout),
            CaptureError::NoSpeech(_timeout) => Self::NoSpeech,
            error => Self::Capture(error),
        }
    }
}

impl From<&DictationError> for RpcErrorCode {
    fn from(error: &DictationError) -> Self {
        match error {
            DictationError::Busy => Self::Busy,
            DictationError::RecordingAlreadyStarted => Self::Internal,
            DictationError::StreamingDeliveryUnavailable => Self::InvalidRequest,
            DictationError::Cancelled => Self::Cancelled,
            DictationError::OverallTimeout(_) => Self::OverallTimeout,
            DictationError::RecordingTimeout(_) => Self::RecordingTimeout,
            DictationError::DeliveryTimeout(_) => Self::DeliveryTimeout,
            DictationError::DeliveryTargetChanged => Self::DeliveryTargetChanged,
            DictationError::DeliveryInterrupted => Self::DeliveryInterrupted,
            DictationError::Capture(_) => Self::CaptureFailed,
            DictationError::NoSpeech => Self::NoSpeech,
            DictationError::Transcription(_) => Self::TranscriptionFailed,
            DictationError::Delivery(_) => Self::DeliveryFailed,
        }
    }
}

impl DictationError {
    pub(crate) fn status(&self) -> &'static str {
        RpcErrorCode::from(self).label()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum RecordingState {
    NotStarted,
    Recording,
    StopRequested,
    Finished,
}

/// How long a terminal stage waits for capture to let go before it goes out
/// anyway. Capture polls its stop every chunk, so this is only ever a
/// backstop against a wedged worker.
const CAPTURE_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
const CAPTURE_RELEASE_POLL: Duration = Duration::from_millis(5);

/// Process-local control signals for one dictation execution.
#[derive(Debug)]
pub(crate) struct ExecutionControl {
    id: RecordingId,
    stop_mode: StopMode,
    cancellation: CancelToken,
    deadline: Deadline,
    recording_state: AtomicU8,
    /// Set by the first poll of the stop signal, which only a live capture
    /// makes. A scope that was never polled never captured anything.
    capture_polled: AtomicBool,
    /// Set when the terminal stage stopped waiting for capture, so the
    /// scope's late end says nothing after it.
    capture_abandoned: AtomicBool,
    #[cfg(target_os = "macos")]
    mute_output: bool,
    #[cfg(target_os = "macos")]
    output: std::sync::Mutex<Option<OutputMute>>,
}

impl ExecutionControl {
    fn new(
        id: RecordingId,
        stop_mode: StopMode,
        cancellation: CancelToken,
        deadline: Deadline,
        _mute_output: bool,
    ) -> Self {
        Self {
            id,
            stop_mode,
            cancellation,
            deadline,
            recording_state: AtomicU8::new(RecordingState::NotStarted as u8),
            capture_polled: AtomicBool::new(false),
            capture_abandoned: AtomicBool::new(false),
            #[cfg(target_os = "macos")]
            mute_output: _mute_output,
            #[cfg(target_os = "macos")]
            output: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(stop_mode: StopMode) -> Arc<Self> {
        let policy = DictationPolicy::new(Duration::from_secs(5)).expect("test policy is valid");
        Arc::new(Self::new(
            RecordingId(0),
            stop_mode,
            CancelToken::new(),
            policy.deadline(),
            false,
        ))
    }

    pub(crate) fn recording_id(&self) -> RecordingId {
        self.id
    }

    pub(crate) fn stop_mode(&self) -> StopMode {
        self.stop_mode
    }

    /// Whether the caller cancelled the operation.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Returns whether the operation has exhausted its total execution time.
    pub(crate) fn deadline_expired(&self) -> bool {
        self.deadline.expired()
    }

    /// Whether the press must end now: cancelled, or out of time.
    pub(crate) fn should_stop(&self) -> bool {
        self.is_cancelled() || self.deadline_expired()
    }

    /// Returns the configured total execution timeout for error reporting.
    pub(crate) fn overall_timeout(&self) -> Duration {
        self.deadline.timeout()
    }

    /// Starts the single recording scope owned by this execution.
    ///
    /// Create this scope before any recording preparation that a manual stop
    /// should be allowed to interrupt. Dropping it makes later stop requests
    /// ineligible. The guard travels to the capture thread, so it drops when
    /// capture ends rather than when the whole execution does.
    ///
    /// The scope announces both recording boundaries: `Recording` as it
    /// opens, and `Transcribing` as capture lets go of it, when it captured
    /// at all. That fixes their order by construction, whatever the capture
    /// backend does.
    pub(crate) fn start_recording(
        self: &Arc<Self>,
        events: &DictationEventEmitter,
    ) -> Result<RecordingGuard, DictationError> {
        self.recording_state
            .compare_exchange(
                RecordingState::NotStarted as u8,
                RecordingState::Recording as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_state| DictationError::RecordingAlreadyStarted)?;

        let guard = RecordingGuard {
            control: Arc::clone(self),
            events: events.clone(),
        };
        #[cfg(target_os = "macos")]
        if self.mute_output {
            *self.output() = OutputMute::acquire();
        }
        events.emit(DictationStage::Recording(self.id));
        Ok(guard)
    }

    #[cfg(target_os = "macos")]
    fn output(&self) -> std::sync::MutexGuard<'_, Option<OutputMute>> {
        self.output.lock().unwrap_or_else(|poisoned| {
            tracing::error!("the output mute lock was poisoned; recovering");
            poisoned.into_inner()
        })
    }

    fn restore_output(&self) {
        #[cfg(target_os = "macos")]
        {
            // Keep admission blocked until the native restore finishes.
            let mut output = self.output();
            drop(output.take());
        }
    }

    pub(crate) fn request_recording_stop(&self) -> bool {
        self.recording_state
            .compare_exchange(
                RecordingState::Recording as u8,
                RecordingState::StopRequested as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Asks capture to stop and waits for it to let go of its scope, up to
    /// `CAPTURE_RELEASE_TIMEOUT`, so a terminal stage never lands ahead of
    /// the `Transcribing` the guard announces.
    pub(crate) fn await_capture_release(&self) {
        self.request_recording_stop();
        if self.capture_abandoned.load(Ordering::Acquire) {
            return;
        }
        let waited = Instant::now();
        while self.recording_open() && waited.elapsed() < CAPTURE_RELEASE_TIMEOUT {
            thread::sleep(CAPTURE_RELEASE_POLL);
        }
        if self.recording_open() {
            self.capture_abandoned.store(true, Ordering::Release);
            tracing::warn!(
                recording = %self.id,
                "capture did not let go of the recording in time"
            );
        }
    }

    fn recording_open(&self) -> bool {
        let state = self.recording_state.load(Ordering::Acquire);
        state == RecordingState::Recording as u8 || state == RecordingState::StopRequested as u8
    }
}

/// RAII ownership of the stage during which manual recording stop is valid.
pub(crate) struct RecordingGuard {
    control: Arc<ExecutionControl>,
    events: DictationEventEmitter,
}

impl RecordingGuard {
    pub(crate) fn should_stop(&self) -> bool {
        self.control.capture_polled.store(true, Ordering::Release);
        self.control.should_stop()
            || self.control.recording_state.load(Ordering::Acquire)
                == RecordingState::StopRequested as u8
    }

    /// Whether the end of this scope is the end of a capture somebody is
    /// still waiting on. A scope that never captured, a cancelled run, or
    /// one whose terminal stage already went out says nothing: "working"
    /// would be a lie.
    fn ends_a_capture(&self) -> bool {
        let control = &self.control;
        control.capture_polled.load(Ordering::Acquire)
            && !control.is_cancelled()
            && !control.capture_abandoned.load(Ordering::Acquire)
    }
}

impl Drop for RecordingGuard {
    /// Announced before the state lands, so whoever waits on the state sees
    /// `Transcribing` ahead of their own terminal stage.
    fn drop(&mut self) {
        self.control.restore_output();
        if self.ends_a_capture() {
            self.events.emit(DictationStage::Transcribing);
        }
        self.control
            .recording_state
            .store(RecordingState::Finished as u8, Ordering::Release);
    }
}

struct ExecutionGuard<'a>(&'a ExecutionControl);

impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        self.0.await_capture_release();
        self.0.restore_output();
    }
}

/// Owns execution control for dictation operations. Admission is the
/// microphone's, which every recording operation shares.
pub(crate) struct DictationCoordinator<E> {
    policy: DictationPolicy,
    executor: E,
    microphone: Arc<Gate>,
    /// Counts from one; zero is what the wire refuses.
    next_recording: AtomicU64,
}

impl<E> DictationCoordinator<E>
where
    E: DictationExecutor,
{
    pub(crate) fn new(policy: DictationPolicy, executor: E, microphone: Arc<Gate>) -> Self {
        Self {
            policy,
            executor,
            microphone,
            next_recording: AtomicU64::new(1),
        }
    }

    pub(crate) fn execute(
        &self,
        settings: DictationSettings,
        cancellation: CancelToken,
        events: &DictationEventEmitter,
    ) -> Result<DictationOutcome, DictationError> {
        match self.admit(&settings, cancellation)? {
            Admission::Execute(control, _claim) => {
                // Capture and the level meter log from their own threads,
                // outside this span.
                let _span =
                    tracing::info_span!("dictation", recording = %control.recording_id()).entered();
                let _execution = ExecutionGuard(&control);
                let started = Instant::now();
                let result = self.executor.execute(settings, &control, events);
                log_execution_result(&result, started);
                result.map(DictationOutcome::Completed)
            }
            Admission::StopAccepted => Ok(DictationOutcome::StopAccepted),
        }
    }

    fn admit(
        &self,
        settings: &DictationSettings,
        cancellation: CancelToken,
    ) -> Result<Admission, DictationError> {
        let control = Arc::new(ExecutionControl::new(
            RecordingId(self.next_recording.fetch_add(1, Ordering::Relaxed)),
            settings.stop_mode,
            cancellation,
            self.policy.deadline(),
            settings.mute_output,
        ));
        match self
            .microphone
            .admit_dictation(settings.stop_mode, Arc::clone(&control))
        {
            Admitted::Claimed(claim) => {
                tracing::info!(
                    status = "admitted",
                    recording = %control.recording_id(),
                    stop_mode = ?settings.stop_mode,
                    delivery_mode = ?settings.delivery_mode,
                    output_mode = ?settings.output_mode,
                    "dictation execution admitted"
                );
                Ok(Admission::Execute(control, claim))
            }
            Admitted::StopAccepted => {
                tracing::info!(
                    status = "stop_accepted",
                    "dictation recording stop request accepted"
                );
                Ok(Admission::StopAccepted)
            }
            Admitted::Busy => {
                tracing::info!(
                    status = "rejected",
                    reason = "busy",
                    "dictation request rejected"
                );
                Err(DictationError::Busy)
            }
        }
    }

    /// Hears audio the caller captured, through the engine already loaded.
    pub(crate) fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        own_voice_only: bool,
        project: Option<&crate::vocabulary::Directory>,
        cancel: &CancelToken,
    ) -> Result<Transcript, DictationError> {
        self.executor
            .transcribe(recording, language, own_voice_only, project, cancel)
    }

    /// Ends one recording, whatever stop mode started it. The execution
    /// goes on to transcribe and deliver what it heard, so this asks for the
    /// end of the utterance, not the end of the dictation. False when that
    /// recording is not the one running.
    pub(crate) fn stop_recording(&self, recording: RecordingId) -> bool {
        self.microphone.stop_recording(recording)
    }
}

fn log_execution_result(result: &Result<DictationResult, DictationError>, started: Instant) {
    let elapsed_ms = started.elapsed().as_millis();
    match result {
        Ok(_) => tracing::info!(
            status = "completed",
            elapsed_ms,
            "dictation execution finished"
        ),
        Err(DictationError::Cancelled) => {
            tracing::info!(
                status = "cancelled",
                elapsed_ms,
                "dictation execution finished"
            );
        }
        Err(error) => tracing::warn!(
            status = error.status(),
            elapsed_ms,
            error = %error_chain(error),
            "dictation execution finished"
        ),
    }
}

/// The claim rides along so the microphone is released when the execution
/// ends, however it ends.
enum Admission {
    Execute(Arc<ExecutionControl>, Claim),
    StopAccepted,
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::thread;

    use crate::audio::SilenceDuration;
    use crate::delivery::OutputMode;
    use crate::dictation::DeliveryMode;
    use crate::transcription::LanguageCode;

    use super::*;

    /// Records until asked to stop, then hands back what it "heard". Stands
    /// in for the real pipeline, which stops the same way.
    struct RecordingExecutor {
        started: SyncSender<RecordingId>,
    }

    impl DictationExecutor for RecordingExecutor {
        fn execute(
            &self,
            _settings: DictationSettings,
            control: &Arc<ExecutionControl>,
            events: &DictationEventEmitter,
        ) -> Result<DictationResult, DictationError> {
            let recording = control.start_recording(events)?;
            self.started
                .send(control.recording_id())
                .expect("the test waits for the start");
            while !recording.should_stop() {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(DictationResult {
                text: "heard".to_owned(),
                own_voice: false,
            })
        }

        fn transcribe(
            &self,
            _recording: &mut RecordingReader,
            _language: &Language,
            _own_voice_only: bool,
            _project: Option<&crate::vocabulary::Directory>,
            _cancel: &CancelToken,
        ) -> Result<Transcript, DictationError> {
            Transcript::new("heard").map_err(DictationError::Transcription)
        }
    }

    fn settings(stop_mode: StopMode) -> DictationSettings {
        DictationSettings {
            dictation_history: true,
            history_audio_retention: false,
            mute_output: false,
            recognition_language: Language::Specific(LanguageCode::english()),
            output_mode: OutputMode::Copy,
            delivery_mode: DeliveryMode::Complete,
            stop_mode,
            silence_duration: SilenceDuration::default(),
            own_voice_only: false,
            transcript_only: true,
            project: None,
        }
    }

    fn coordinator() -> (
        Arc<DictationCoordinator<RecordingExecutor>>,
        Receiver<RecordingId>,
        DictationEventEmitter,
    ) {
        let (started, recording) = sync_channel(1);
        let policy = DictationPolicy::new(Duration::from_secs(5)).expect("a valid policy");
        let coordinator = Arc::new(DictationCoordinator::new(
            policy,
            RecordingExecutor { started },
            crate::microphone::Gate::new(),
        ));
        (
            coordinator,
            recording,
            DictationEventEmitter::unsubscribed(),
        )
    }

    #[test]
    fn a_recording_that_stops_on_silence_still_stops_on_request() {
        let (coordinator, recording, events) = coordinator();
        let running = {
            let coordinator = Arc::clone(&coordinator);
            thread::spawn(move || {
                coordinator.execute(settings(StopMode::OnSilence), CancelToken::never(), &events)
            })
        };
        let id = recording.recv().expect("the recording starts");

        assert!(coordinator.stop_recording(id));

        let outcome = running.join().expect("the run ends").expect("it succeeds");
        assert_eq!(
            outcome,
            DictationOutcome::Completed(DictationResult {
                text: "heard".to_owned(),
                own_voice: false,
            })
        );
    }

    #[test]
    fn transcription_errors_keep_their_meaning() {
        let heard_nothing = CaptureError::NoSpeech(Duration::from_secs(1));

        assert!(matches!(
            DictationError::from(TranscriptionError::NoSpeech),
            DictationError::NoSpeech
        ));
        assert!(matches!(
            DictationError::from(TranscriptionError::Cancelled),
            DictationError::Cancelled
        ));
        assert!(matches!(
            DictationError::from(TranscriptionError::Capture(heard_nothing)),
            DictationError::NoSpeech
        ));
        assert!(matches!(
            DictationError::from(TranscriptionError::NoNativeStream),
            DictationError::Transcription(TranscriptionError::NoNativeStream)
        ));
    }

    #[test]
    fn stopping_with_nothing_recording_says_so() {
        let (coordinator, _recording, _events) = coordinator();

        assert!(!coordinator.stop_recording(RecordingId(1)));
    }

    #[test]
    fn a_stop_naming_another_recording_is_refused() {
        let (coordinator, recording, events) = coordinator();
        let running = {
            let coordinator = Arc::clone(&coordinator);
            thread::spawn(move || {
                coordinator.execute(settings(StopMode::Manual), CancelToken::never(), &events)
            })
        };
        let id = recording.recv().expect("the recording starts");

        assert!(!coordinator.stop_recording(RecordingId(id.0 + 1)));
        assert!(coordinator.stop_recording(id));
        running.join().expect("the run ends").expect("it succeeds");
    }

    /// An emitter whose every stage lands in the receiver, in order.
    fn announcing() -> (DictationEventEmitter, Receiver<DictationStage>) {
        let (heard, stages) = sync_channel(4);
        let events = DictationEventEmitter::subscribed_by("aria-test-stages", move |event| {
            if let super::super::DictationEvent::Stage(stage) = event {
                let _ = heard.send(stage);
            }
        });
        (events, stages)
    }

    /// The two boundaries a client acts on, in the order it needs them.
    #[test]
    fn the_recording_scope_announces_both_of_its_ends() {
        let (events, stages) = announcing();
        let control = ExecutionControl::for_test(StopMode::Manual);

        let guard = control.start_recording(&events).expect("recording starts");
        assert_eq!(
            stages.recv().expect("recording is announced"),
            DictationStage::Recording(RecordingId(0))
        );
        assert!(!guard.should_stop(), "capture polls, as a live one does");
        drop(guard);
        drop(events);
        assert_eq!(
            stages.recv().expect("the end is announced"),
            DictationStage::Transcribing
        );
    }

    #[test]
    fn a_cancelled_recording_does_not_pretend_to_transcribe() {
        let (events, stages) = announcing();
        let cancel = CancelToken::new();
        let policy = DictationPolicy::new(Duration::from_secs(5)).expect("a valid policy");
        let control = Arc::new(ExecutionControl::new(
            RecordingId(0),
            StopMode::Manual,
            cancel.clone(),
            policy.deadline(),
            false,
        ));

        let guard = control.start_recording(&events).expect("recording starts");
        stages.recv().expect("recording is announced");
        cancel.cancel();
        assert!(guard.should_stop());
        drop(guard);
        drop(events);

        assert!(
            stages.recv().is_err(),
            "nothing follows a cancelled recording"
        );
    }

    /// A capture that failed to start never polled its stop. Its scope
    /// ends with no `Transcribing`: there was nothing to transcribe.
    #[test]
    fn a_capture_that_never_ran_does_not_pretend_to_transcribe() {
        let (events, stages) = announcing();
        let control = ExecutionControl::for_test(StopMode::Manual);

        let guard = control.start_recording(&events).expect("recording starts");
        stages.recv().expect("recording is announced");
        drop(guard);
        drop(events);

        assert!(
            stages.recv().is_err(),
            "nothing follows a capture that never ran"
        );
    }

    #[cfg(target_os = "macos")]
    fn fake_output(writes: &Arc<std::sync::Mutex<Vec<u32>>>) -> OutputMute {
        let writes = Arc::clone(writes);
        OutputMute::with_state(0, move |value| {
            writes.lock().unwrap().push(value);
            Ok(())
        })
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recording_cleanup_restores_without_a_terminal_event() {
        for ending in ["stop", "cancel", "deadline", "never_started", "panic"] {
            let events = DictationEventEmitter::unsubscribed();
            let timeout = if ending == "deadline" {
                Duration::ZERO
            } else {
                Duration::from_secs(5)
            };
            let control = Arc::new(ExecutionControl::new(
                RecordingId(0),
                StopMode::Manual,
                CancelToken::new(),
                Deadline::after(timeout),
                false,
            ));
            let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
            *control.output() = Some(fake_output(&writes));
            let guard = control.start_recording(&events).unwrap();
            match ending {
                "stop" => assert!(control.request_recording_stop()),
                "cancel" => control.cancellation.cancel(),
                "deadline" => assert!(guard.should_stop()),
                _ => {}
            }
            if ending == "panic" {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _recording = guard;
                    panic!("capture failed");
                }));
                assert!(result.is_err());
            } else {
                drop(guard);
            }
            assert_eq!(*writes.lock().unwrap(), [1, 0], "{ending}");
            drop(ExecutionGuard(&control));
            assert_eq!(*writes.lock().unwrap(), [1, 0], "{ending}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn executor_panic_restores_before_admission_and_late_capture_cleanup() {
        use std::sync::Mutex;

        struct PanickingExecutor {
            retained: Arc<Mutex<Option<RecordingGuard>>>,
            writes: Arc<Mutex<Vec<u32>>>,
        }

        impl DictationExecutor for PanickingExecutor {
            fn execute(
                &self,
                _: DictationSettings,
                control: &Arc<ExecutionControl>,
                events: &DictationEventEmitter,
            ) -> Result<DictationResult, DictationError> {
                *control.output() = Some(fake_output(&self.writes));
                let guard = control.start_recording(events)?;
                assert!(!guard.should_stop());
                *self.retained.lock().unwrap() = Some(guard);
                panic!("executor failed while capture still owns its guard");
            }

            fn transcribe(
                &self,
                _: &mut RecordingReader,
                _: &Language,
                _: bool,
                _: Option<&crate::vocabulary::Directory>,
                _: &CancelToken,
            ) -> Result<Transcript, DictationError> {
                panic!("this test executes local recording");
            }
        }

        let writes = Arc::new(Mutex::new(Vec::new()));
        let retained = Arc::new(Mutex::new(None));
        let microphone = Gate::new();
        let coordinator = DictationCoordinator::new(
            DictationPolicy::new(Duration::from_secs(5)).unwrap(),
            PanickingExecutor {
                retained: Arc::clone(&retained),
                writes: Arc::clone(&writes),
            },
            Arc::clone(&microphone),
        );
        let (events, stages) = announcing();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            coordinator.execute(settings(StopMode::Manual), CancelToken::new(), &events)
        }));
        assert!(result.is_err());
        assert_eq!(*writes.lock().unwrap(), [1, 0]);

        let next = ExecutionControl::for_test(StopMode::Manual);
        let Admitted::Claimed(_claim) =
            microphone.admit_dictation(StopMode::Manual, Arc::clone(&next))
        else {
            panic!("the failed execution must release admission");
        };
        *next.output() = Some(fake_output(&writes));
        let next_recording = next
            .start_recording(&DictationEventEmitter::unsubscribed())
            .unwrap();
        drop(retained.lock().unwrap().take());
        assert_eq!(*writes.lock().unwrap(), [1, 0, 1]);
        drop(next_recording);
        assert_eq!(*writes.lock().unwrap(), [1, 0, 1, 0]);
        drop(events);
        assert_eq!(
            stages.iter().collect::<Vec<_>>(),
            [DictationStage::Recording(RecordingId(1))]
        );
    }
}
