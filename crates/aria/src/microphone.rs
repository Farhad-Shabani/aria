//! Who may open the microphone.
//!
//! One operation records at a time. Nothing below enforces that: the capture
//! worker serves one recording per thread and would queue a second one
//! silently, so a press during an enrollment would wait without saying why.
//! Refusing it here is the honest answer, and the claim is released by drop
//! on every exit path.

use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(test)]
use crate::dictation::DictationEventEmitter;
use crate::dictation::{ExecutionControl, RecordingId, StopMode};

/// The one gate in front of the capture backend. Named for what it
/// does, not the device: `aria_kit::audio::Microphone` is the device.
pub(crate) struct Gate {
    holder: Mutex<Option<Holder>>,
}

/// What is recording. A dictation is kept whole because a second manual press
/// is how the first one is stopped.
enum Holder {
    Dictation(Arc<ExecutionControl>),
    /// Ends on its own terms: a stop names a recording, and an enrollment
    /// has none.
    Enrollment,
    /// A recording relayed off a phone or watch. It holds the gate without
    /// using the device: the audio was captured on the phone or watch and
    /// arrives finished. Nothing local can stop it, because nothing local
    /// started it.
    Relay,
    HistoryRetry,
}

/// What asking for the microphone on behalf of a dictation did.
pub(crate) enum Admitted {
    /// The microphone is this dictation's until the claim drops.
    Claimed(Claim),
    /// A manual-stop dictation was recording, and this press ended it. No
    /// new dictation starts; the running one goes on to transcribe.
    StopAccepted,
    /// Something else is recording.
    Busy,
}

impl Gate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            holder: Mutex::new(None),
        })
    }

    /// Admits a dictation, applying the rule that a manual press interrupts a
    /// manual recording. The decision is made under the lock, so a dictation
    /// that ends mid-decision cannot have the next one stopped in its place.
    pub(crate) fn admit_dictation(
        self: &Arc<Self>,
        stop_mode: StopMode,
        control: Arc<ExecutionControl>,
    ) -> Admitted {
        let mut holder = self.hold();
        match holder.as_ref() {
            Some(Holder::Dictation(running))
                if running.stop_mode() == StopMode::Manual
                    && stop_mode == StopMode::Manual
                    && running.request_recording_stop() =>
            {
                Admitted::StopAccepted
            }
            Some(_) => Admitted::Busy,
            None => {
                *holder = Some(Holder::Dictation(control));
                Admitted::Claimed(self.claim())
            }
        }
    }

    /// Admits an enrollment. `None` when something is already recording.
    pub(crate) fn admit_enrollment(self: &Arc<Self>) -> Option<Claim> {
        let mut holder = self.hold();
        if holder.is_some() {
            return None;
        }
        *holder = Some(Holder::Enrollment);
        Some(self.claim())
    }

    /// Admits a relayed recording, which owns the gate until its claim
    /// drops. `None` when something is already recording — the phone is told
    /// the Mac is busy, and the press at the keyboard wins.
    pub(crate) fn admit_relay(self: &Arc<Self>) -> Option<Claim> {
        let mut holder = self.hold();
        if holder.is_some() {
            return None;
        }
        *holder = Some(Holder::Relay);
        Some(self.claim())
    }

    pub(crate) fn admit_history_retry(self: &Arc<Self>) -> Option<Claim> {
        let mut holder = self.hold();
        if holder.is_some() {
            return None;
        }
        *holder = Some(Holder::HistoryRetry);
        Some(self.claim())
    }

    /// Ends the named recording, which goes on to transcribe what it heard.
    /// False when it is not the one running, or is already stopping: a stop
    /// from one client must never land on another's press.
    pub(crate) fn stop_recording(&self, recording: RecordingId) -> bool {
        match self.hold().as_ref() {
            Some(Holder::Dictation(running)) if running.recording_id() == recording => {
                running.request_recording_stop()
            }
            Some(
                Holder::Dictation(_) | Holder::Enrollment | Holder::Relay | Holder::HistoryRetry,
            )
            | None => false,
        }
    }

    fn claim(self: &Arc<Self>) -> Claim {
        Claim {
            gate: Arc::clone(self),
        }
    }

    /// A panic while an operation held the lock leaves the state it was
    /// reading, not a poisoned daemon that can never record again.
    fn hold(&self) -> MutexGuard<'_, Option<Holder>> {
        self.holder.lock().unwrap_or_else(|poisoned| {
            tracing::error!("the microphone lock was poisoned; recovering");
            poisoned.into_inner()
        })
    }
}

/// The microphone, until this drops.
pub(crate) struct Claim {
    gate: Arc<Gate>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        *self.gate.hold() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(stop_mode: StopMode) -> Arc<ExecutionControl> {
        ExecutionControl::for_test(stop_mode)
    }

    #[test]
    fn an_idle_microphone_admits_a_dictation() {
        let gate = Gate::new();
        assert!(matches!(
            gate.admit_dictation(StopMode::OnSilence, control(StopMode::OnSilence)),
            Admitted::Claimed(_)
        ));
    }

    #[test]
    fn history_retry_excludes_recording_and_releases_on_drop() {
        let gate = Gate::new();
        let retry = gate.admit_history_retry().unwrap();
        assert!(gate.admit_history_retry().is_none());
        assert!(gate.admit_relay().is_none());
        assert!(gate.admit_enrollment().is_none());
        assert!(matches!(
            gate.admit_dictation(StopMode::Manual, control(StopMode::Manual)),
            Admitted::Busy
        ));
        assert!(!gate.stop_recording(RecordingId(0)));
        drop(retry);
        assert!(gate.admit_enrollment().is_some());
    }

    #[test]
    fn a_claim_releases_the_microphone_when_it_drops() {
        let gate = Gate::new();
        drop(gate.admit_dictation(StopMode::OnSilence, control(StopMode::OnSilence)));
        assert!(gate.admit_enrollment().is_some());
    }

    #[test]
    fn an_enrollment_keeps_a_press_out() {
        let gate = Gate::new();
        let _enrolling = gate.admit_enrollment().expect("the microphone is idle");
        assert!(matches!(
            gate.admit_dictation(StopMode::OnSilence, control(StopMode::OnSilence)),
            Admitted::Busy
        ));
    }

    #[test]
    fn a_dictation_keeps_an_enrollment_out() {
        let gate = Gate::new();
        let _dictating = gate.admit_dictation(StopMode::OnSilence, control(StopMode::OnSilence));
        assert!(gate.admit_enrollment().is_none());
    }

    #[test]
    fn two_enrollments_do_not_overlap() {
        let gate = Gate::new();
        let _first = gate.admit_enrollment().expect("the microphone is idle");
        assert!(gate.admit_enrollment().is_none());
    }

    #[test]
    fn a_second_manual_press_stops_the_recording_instead_of_starting_one() {
        let gate = Gate::new();
        let running = control(StopMode::Manual);
        let _recording = running
            .start_recording(&DictationEventEmitter::unsubscribed())
            .expect("recording starts");
        let _dictating = gate.admit_dictation(StopMode::Manual, Arc::clone(&running));

        assert!(matches!(
            gate.admit_dictation(StopMode::Manual, control(StopMode::Manual)),
            Admitted::StopAccepted
        ));
    }

    /// A phone that walks out of range mid-upload drops its session, and
    /// nothing runs afterwards to tidy up. Without the drop, every later
    /// press is refused with no way back.
    #[test]
    fn a_relay_holds_the_microphone_until_its_claim_drops() {
        let gate = Gate::new();

        let relaying = gate.admit_relay().expect("the microphone is idle");
        assert!(gate.admit_relay().is_none(), "one relay at a time");
        assert!(matches!(
            gate.admit_dictation(StopMode::Manual, control(StopMode::Manual)),
            Admitted::Busy
        ));

        drop(relaying);

        assert!(gate.admit_relay().is_some(), "a dropped claim releases");
    }

    /// The audio was captured on the wrist and is already in the engine, so
    /// a press has nothing left to cut short.
    #[test]
    fn a_press_cannot_stop_a_relay() {
        let gate = Gate::new();
        let _relaying = gate.admit_relay().expect("the microphone is idle");

        assert!(!gate.stop_recording(RecordingId(0)));
    }

    #[test]
    fn a_stop_does_not_end_an_enrollment() {
        let gate = Gate::new();
        let _enrolling = gate.admit_enrollment().expect("the microphone is idle");

        assert!(!gate.stop_recording(RecordingId(0)));
    }

    #[test]
    fn a_stop_names_the_recording_it_ends() {
        let gate = Gate::new();
        let running = control(StopMode::OnSilence);
        let _recording = running
            .start_recording(&DictationEventEmitter::unsubscribed())
            .expect("recording starts");
        let _dictating = gate.admit_dictation(StopMode::OnSilence, Arc::clone(&running));

        assert!(!gate.stop_recording(RecordingId(7)));
        assert!(gate.stop_recording(running.recording_id()));
    }
}
