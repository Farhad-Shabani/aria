use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aria_kit::cancel::{CancelToken, Deadline};
use aria_kit::synthesis::{SpeakingRate, SpeechText, SynthesisError};

use super::{SpeechError, SpeechEventEmitter, SpeechOutcome, SpeechSettings, SpeechStage};

/// Grace for a replaced utterance to wind down before the replacement gives
/// up as Busy.
const REPLACE_GRACE: Duration = Duration::from_secs(2);

/// Slack on top of the estimated speaking time before an utterance is over
/// its deadline. Compiled policy, not a setting.
const DEADLINE_BASE: Duration = Duration::from_secs(30);

/// How many bytes of text the deadline allows per second of speech, at the
/// natural pace.
const DEADLINE_BYTES_PER_SECOND: u32 = 15;

/// Runs one admitted utterance to completion on an engine. The coordinator,
/// not an implementation, turns the outcome into the terminal event and the
/// log line.
pub(crate) trait SpeechExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        settings: SpeechSettings,
        control: &Arc<ExecutionControl>,
        events: &SpeechEventEmitter,
    ) -> Result<SpeechOutcome, SpeechError>;
}

/// Process-local control signals for one utterance.
#[derive(Debug)]
pub(crate) struct ExecutionControl {
    cancellation: CancelToken,
    /// Set when a stop request or a replacing utterance ends this one; the
    /// utterance then finishes as [`SpeechOutcome::Stopped`], not as a
    /// failure.
    interrupted: AtomicBool,
    deadline: Deadline,
}

impl ExecutionControl {
    fn new(cancellation: CancelToken, settings: &SpeechSettings) -> Self {
        Self {
            cancellation,
            interrupted: AtomicBool::new(false),
            deadline: Deadline::after(deadline_for(&settings.text, settings.rate)),
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub(crate) fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Acquire)
    }

    pub(crate) fn deadline_expired(&self) -> bool {
        self.deadline.expired()
    }

    /// The deadline this utterance was given, for error reporting.
    pub(crate) fn timeout(&self) -> Duration {
        self.deadline.timeout()
    }

    /// Whether the utterance should stop promptly, for whatever reason. This
    /// is what feeds the engine's cancel token.
    pub(crate) fn should_stop(&self) -> bool {
        self.is_cancelled() || self.is_interrupted() || self.deadline_expired()
    }

    fn interrupt(&self) {
        self.interrupted.store(true, Ordering::Release);
    }

    pub(super) fn stopped_outcome(&self) -> Result<SpeechOutcome, SpeechError> {
        if self.is_cancelled() {
            Err(SpeechError::Cancelled)
        } else if self.is_interrupted() {
            Ok(SpeechOutcome::Stopped)
        } else if self.deadline_expired() {
            Err(SpeechError::Timeout(self.timeout()))
        } else {
            Err(SpeechError::Cancelled)
        }
    }
}

/// How much time an utterance gets before it is presumed wedged: generous
/// slack plus time proportional to the text. Half speed takes twice as long
/// to say, so the allowance scales with the rate.
fn deadline_for(text: &SpeechText, rate: SpeakingRate) -> Duration {
    let bytes_per_second = f64::from(DEADLINE_BYTES_PER_SECOND) * f64::from(rate.get());
    let speaking = text.as_str().len() as f64 / bytes_per_second;
    DEADLINE_BASE + Duration::from_secs_f64(speaking)
}

/// Owns admission and execution control for spoken responses. A new
/// utterance replaces the one playing: the running utterance is interrupted,
/// given [`REPLACE_GRACE`] to wind down, and only then is its replacement
/// admitted.
pub(crate) struct SpeechCoordinator<E> {
    executor: E,
    active: Mutex<Option<Arc<ExecutionControl>>>,
    /// Signalled each time an utterance releases the slot.
    released: Condvar,
}

impl<E> SpeechCoordinator<E>
where
    E: SpeechExecutor,
{
    pub(crate) fn new(executor: E) -> Self {
        Self {
            executor,
            active: Mutex::new(None),
            released: Condvar::new(),
        }
    }

    pub(crate) fn execute(
        &self,
        settings: SpeechSettings,
        cancellation: CancelToken,
        events: &SpeechEventEmitter,
    ) -> Result<SpeechOutcome, SpeechError> {
        let guard = self.admit(&settings, cancellation)?;
        let started = Instant::now();
        let result = self.executor.execute(settings, guard.control(), events);
        let result = match result {
            Err(SpeechError::Synthesis(SynthesisError::Cancelled))
                if guard.control().should_stop() =>
            {
                guard.control().stopped_outcome()
            }
            result => result,
        };
        report_outcome(&result, events, started);
        result
    }

    pub(crate) fn stop(&self) -> bool {
        match self.lock_active().as_ref() {
            Some(running) => {
                running.interrupt();
                tracing::info!(status = "stop_accepted", "speech stop request accepted");
                true
            }
            None => false,
        }
    }

    fn admit(
        &self,
        settings: &SpeechSettings,
        cancellation: CancelToken,
    ) -> Result<ExecutionGuard<'_, E>, SpeechError> {
        let mut active = self.lock_active();
        // Each occupant is interrupted in turn: a replacement that loses the
        // slot to another replacement must still displace that one, or it
        // would wait out the grace on an utterance nobody told to stop.
        let deadline = Instant::now() + REPLACE_GRACE;
        loop {
            if cancellation.is_cancelled() {
                return Err(SpeechError::Cancelled);
            }
            let Some(running) = active.as_ref() else {
                break;
            };
            running.interrupt();
            tracing::info!(status = "replacing", "interrupting the playing utterance");
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                tracing::warn!(
                    status = "rejected",
                    reason = "busy",
                    "the playing utterance did not wind down in time"
                );
                return Err(SpeechError::Busy);
            };
            // One wait per turn, not `wait_timeout_while`: a predicate loop
            // would never let a new occupant reach the interrupt above.
            let (waited, _timeout) = self
                .released
                .wait_timeout(active, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            active = waited;
        }

        let control = Arc::new(ExecutionControl::new(cancellation, settings));
        *active = Some(Arc::clone(&control));
        tracing::info!(status = "admitted", "utterance admitted");

        Ok(ExecutionGuard {
            coordinator: self,
            control,
        })
    }

    fn lock_active(&self) -> MutexGuard<'_, Option<Arc<ExecutionControl>>> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Exactly one terminal stage per finished utterance.
fn report_outcome(
    result: &Result<SpeechOutcome, SpeechError>,
    events: &SpeechEventEmitter,
    started: Instant,
) {
    let elapsed_ms = started.elapsed().as_millis();
    match result {
        Ok(SpeechOutcome::Spoken) => {
            events.emit(SpeechStage::Spoken);
            tracing::info!(status = "spoken", elapsed_ms, "utterance finished");
        }
        Ok(SpeechOutcome::Stopped) => {
            events.emit(SpeechStage::Stopped);
            tracing::info!(status = "stopped", elapsed_ms, "utterance finished");
        }
        Err(SpeechError::Cancelled) => {
            events.emit(SpeechStage::Cancelled);
            tracing::info!(status = "cancelled", elapsed_ms, "utterance finished");
        }
        Err(error) => {
            events.emit(SpeechStage::Failed(error.into()));
            tracing::warn!(
                status = error.status(),
                elapsed_ms,
                %error,
                "utterance finished"
            );
        }
    }
}

struct ExecutionGuard<'coordinator, E: SpeechExecutor> {
    coordinator: &'coordinator SpeechCoordinator<E>,
    control: Arc<ExecutionControl>,
}

impl<E: SpeechExecutor> ExecutionGuard<'_, E> {
    fn control(&self) -> &Arc<ExecutionControl> {
        &self.control
    }
}

impl<E: SpeechExecutor> Drop for ExecutionGuard<'_, E> {
    fn drop(&mut self) {
        *self.coordinator.lock_active() = None;
        self.coordinator.released.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    use super::*;

    /// Long past anything these coordinators take, so only a hang reaches it.
    const WAIT_LIMIT: Duration = Duration::from_secs(10);

    /// Polls `condition` until it holds, and fails rather than hangs when it
    /// never does.
    fn wait_until(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + WAIT_LIMIT;
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn settings(text: &str) -> SpeechSettings {
        SpeechSettings {
            text: SpeechText::new(text).expect("test text is valid"),
            voice: None,
            rate: aria_kit::synthesis::SpeakingRate::standard(),
        }
    }

    type Speaking = std::thread::JoinHandle<Result<SpeechOutcome, SpeechError>>;

    /// Starts one utterance on its own thread and returns once it is
    /// actually speaking, so a test that stops or replaces it is not racing
    /// its admission.
    fn speaking(
        coordinator: &Arc<SpeechCoordinator<SpeakUntilStopped>>,
        text: &str,
        cancellation: CancelToken,
    ) -> Speaking {
        let started = coordinator.executor.executions.load(Ordering::SeqCst);
        let utterance = {
            let coordinator = Arc::clone(coordinator);
            let text = settings(text);
            std::thread::spawn(move || {
                coordinator.execute(text, cancellation, &SpeechEventEmitter::unsubscribed())
            })
        };
        wait_until("the utterance speaks", || {
            coordinator.executor.executions.load(Ordering::SeqCst) != started
        });
        utterance
    }

    /// Speaks until interrupted, so a replacement has something to stop.
    struct SpeakUntilStopped {
        executions: AtomicUsize,
    }

    impl SpeakUntilStopped {
        fn new() -> Self {
            Self {
                executions: AtomicUsize::new(0),
            }
        }
    }

    impl SpeechExecutor for SpeakUntilStopped {
        fn execute(
            &self,
            _settings: SpeechSettings,
            control: &Arc<ExecutionControl>,
            _events: &SpeechEventEmitter,
        ) -> Result<SpeechOutcome, SpeechError> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            while !control.should_stop() {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(SpeechError::Synthesis(
                aria_kit::synthesis::SynthesisError::Cancelled,
            ))
        }
    }

    #[test]
    fn a_new_utterance_replaces_the_one_playing() {
        let coordinator = Arc::new(SpeechCoordinator::new(SpeakUntilStopped::new()));
        let first = speaking(&coordinator, "first", CancelToken::new());

        let second = {
            let coordinator = Arc::clone(&coordinator);
            std::thread::spawn(move || {
                let cancellation = CancelToken::new();
                let admitted = coordinator.admit(&settings("second"), cancellation);
                admitted.map(|_guard| ())
            })
        };

        let first = first.join().expect("the first utterance finishes");
        assert!(matches!(first, Ok(SpeechOutcome::Stopped)));
        second
            .join()
            .expect("the second utterance is admitted")
            .expect("the slot frees within the grace");
    }

    #[test]
    fn a_cancelled_replacement_leaves_the_playing_utterance_alone() {
        let coordinator = SpeechCoordinator::new(SpeakUntilStopped::new());
        let playing = coordinator
            .admit(&settings("playing"), CancelToken::new())
            .expect("the utterance is admitted");
        let cancellation = CancelToken::new();
        cancellation.cancel();

        let result = coordinator.admit(&settings("cancelled"), cancellation);

        assert!(!playing.control().is_interrupted());
        assert!(matches!(result, Err(SpeechError::Cancelled)));
    }

    #[test]
    fn a_replacement_cancelled_while_waiting_does_not_claim_the_slot() {
        let coordinator = Arc::new(SpeechCoordinator::new(SpeakUntilStopped::new()));
        let playing = coordinator
            .admit(&settings("playing"), CancelToken::new())
            .expect("the utterance is admitted");
        let cancellation = CancelToken::new();
        let replacement = {
            let coordinator = Arc::clone(&coordinator);
            let cancellation = cancellation.clone();
            std::thread::spawn(move || {
                coordinator
                    .admit(&settings("replacement"), cancellation)
                    .map(|_guard| ())
            })
        };
        wait_until("the replacement requests the slot", || {
            playing.control().is_interrupted()
        });

        cancellation.cancel();
        drop(playing);

        assert!(matches!(
            replacement.join().expect("the replacement finishes"),
            Err(SpeechError::Cancelled)
        ));
    }

    #[test]
    fn stop_interrupts_the_playing_utterance() {
        let coordinator = Arc::new(SpeechCoordinator::new(SpeakUntilStopped::new()));
        let utterance = speaking(&coordinator, "stop me", CancelToken::new());

        assert!(coordinator.stop(), "an utterance was playing");
        let outcome = utterance.join().expect("the utterance finishes");
        assert!(matches!(outcome, Ok(SpeechOutcome::Stopped)));
        assert!(!coordinator.stop(), "nothing is left to stop");
    }

    #[test]
    fn stop_acknowledges_interruption_before_execution_finishes() {
        let coordinator = SpeechCoordinator::new(SpeakUntilStopped::new());
        let guard = coordinator
            .admit(&settings("not playing yet"), CancelToken::new())
            .expect("the utterance is admitted");

        assert!(coordinator.stop());
        assert!(guard.control().is_interrupted());
        assert!(!guard.control().is_cancelled());
        assert!(coordinator.lock_active().is_some());
        assert_eq!(coordinator.executor.executions.load(Ordering::SeqCst), 0);

        drop(guard);
        assert!(!coordinator.stop());
    }

    #[test]
    fn a_cancelled_utterance_reports_cancelled_not_stopped() {
        let coordinator = Arc::new(SpeechCoordinator::new(SpeakUntilStopped::new()));
        let cancellation = CancelToken::new();
        let utterance = speaking(&coordinator, "cancel me", cancellation.clone());

        cancellation.cancel();
        let outcome = utterance.join().expect("the utterance finishes");
        assert!(matches!(outcome, Err(SpeechError::Cancelled)));
    }

    #[test]
    fn caller_cancellation_precedes_interruption_and_deadline() {
        let cancellation = CancelToken::new();
        let mut control = ExecutionControl::new(cancellation.clone(), &settings("priority"));
        control.deadline = Deadline::after(Duration::ZERO);
        assert!(matches!(
            control.stopped_outcome(),
            Err(SpeechError::Timeout(_))
        ));
        control.interrupt();
        assert!(matches!(
            control.stopped_outcome(),
            Ok(SpeechOutcome::Stopped)
        ));
        cancellation.cancel();
        assert!(matches!(
            control.stopped_outcome(),
            Err(SpeechError::Cancelled)
        ));
    }

    #[test]
    fn engine_cancellation_without_a_control_signal_stays_a_synthesis_failure() {
        struct CancelImmediately;
        impl SpeechExecutor for CancelImmediately {
            fn execute(
                &self,
                _settings: SpeechSettings,
                _control: &Arc<ExecutionControl>,
                _events: &SpeechEventEmitter,
            ) -> Result<SpeechOutcome, SpeechError> {
                Err(SpeechError::Synthesis(SynthesisError::Cancelled))
            }
        }
        let coordinator = SpeechCoordinator::new(CancelImmediately);
        let result = coordinator.execute(
            settings("not cancelled"),
            CancelToken::new(),
            &SpeechEventEmitter::unsubscribed(),
        );
        assert!(matches!(
            result,
            Err(SpeechError::Synthesis(SynthesisError::Cancelled))
        ));
    }

    #[test]
    fn the_deadline_grows_with_the_text_and_with_slower_speech() {
        let text = SpeechText::new("word ".repeat(500)).expect("valid");
        let short = deadline_for(
            &SpeechText::new("hi").expect("valid"),
            SpeakingRate::standard(),
        );
        let long = deadline_for(&text, SpeakingRate::standard());
        let slow = deadline_for(&text, SpeakingRate::new(0.5).expect("valid"));

        assert!(long > short);
        assert!(short >= DEADLINE_BASE);
        assert!(
            slow > long,
            "half speed takes twice as long to say and must be allowed it"
        );
    }

    #[test]
    fn a_replacement_that_loses_the_slot_displaces_the_new_occupant() {
        let coordinator = Arc::new(SpeechCoordinator::new(SpeakUntilStopped::new()));
        let running = speaking(&coordinator, "first", CancelToken::new());

        // Two replacements race; both must end up admitted rather than one
        // waiting out the grace on an utterance it never interrupted.
        let replacements: Vec<_> = (0..2)
            .map(|_replacement| {
                let coordinator = Arc::clone(&coordinator);
                std::thread::spawn(move || {
                    coordinator.execute(
                        settings("replacement"),
                        CancelToken::new(),
                        &SpeechEventEmitter::unsubscribed(),
                    )
                })
            })
            .collect();

        assert!(matches!(
            running.join().expect("the first utterance finishes"),
            Ok(SpeechOutcome::Stopped)
        ));
        // Nothing replaces the last one, so the test stops it.
        let stopper = {
            let coordinator = Arc::clone(&coordinator);
            std::thread::spawn(move || {
                wait_until("both replacements speak", || {
                    coordinator.executor.executions.load(Ordering::SeqCst) >= 3
                });
                wait_until("nothing is left speaking", || !coordinator.stop());
            })
        };
        for replacement in replacements {
            let outcome = replacement.join().expect("the replacement finishes");
            assert!(
                !matches!(outcome, Err(SpeechError::Busy)),
                "a replacement must displace whoever holds the slot"
            );
        }
        stopper.join().expect("the stopper finishes");
    }
}
