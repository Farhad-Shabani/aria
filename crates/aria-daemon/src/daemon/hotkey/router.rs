//! What each key means right now, and doing it. The meaning is read from
//! the active dictation alone, by two pure functions; the router then acts
//! on that dictation, so a stale read fails instead of landing on the next
//! one. A chat run is asked by its own actor, which owns that state.

use super::super::chat::{ChatCallError, ChatHandle};
use super::super::dictation::DictationRuntimeError;
use super::super::dictation::{
    AutoStopRule, DictationCallError, DictationHandle, DictationRuntime,
};
use super::super::operation::CallError;
use super::super::subscriptions::Subscriptions;
use aria_platform::delivery::frontmost_process;
use aria_platform::hotkey::{Combo, Press};
use aria_voice::dictation::{
    ActiveDictation, DictationError, DictationOutcome, DictationRequest, DictationStage, StopMode,
};
use std::sync::Arc;
use tokio::sync::{oneshot, watch};

/// What an Aria key press means.
enum PressAction {
    /// Ends the active dictation's recording; it goes on to transcribe.
    StopRecording(ActiveDictation),
    /// Ends the chat run a press started, if one is in flight; otherwise
    /// dictates until the next tap.
    CancelRunOrDictate,
    /// Dictates until the hold ends.
    DictateHeld,
    EndHold(HoldEnd),
    /// The app runs what the combo is set to.
    Broadcast(Combo),
}

/// What a bare Return the person pressed means.
enum ReturnAction {
    /// The dictation types into the app in front: the Return ends its
    /// recording and is pressed after the rest of the text.
    Take(ActiveDictation),
    /// The key belongs to the app.
    Pass,
}

fn press_action(press: Press, dictation: Option<ActiveDictation>) -> PressAction {
    match press {
        Press::Tap => match dictation {
            Some(dictation)
                if dictation.stop_mode() == StopMode::Manual
                    && dictation.is_recording()
                    && !dictation.is_cancelled() =>
            {
                PressAction::StopRecording(dictation)
            }
            Some(_) | None => PressAction::CancelRunOrDictate,
        },
        // A hold during a manual recording stops it: admission's toggle.
        Press::HoldStarted => PressAction::DictateHeld,
        Press::HoldEnded => PressAction::EndHold(HoldEnd::Released),
        Press::HoldCancelled => PressAction::EndHold(HoldEnd::Cancelled),
        Press::Combo(combo) => PressAction::Broadcast(combo),
    }
}

/// `front` is asked only once the dictation types somewhere: it costs a
/// call to the system, and every Return waits on this.
fn return_action(
    dictation: Option<ActiveDictation>,
    front: impl FnOnce() -> Option<u32>,
) -> ReturnAction {
    let Some(dictation) = dictation.filter(|dictation| !dictation.is_cancelled()) else {
        return ReturnAction::Pass;
    };
    match dictation.types_into() {
        Some(process) if front() == Some(process) => ReturnAction::Take(dictation),
        Some(_) | None => ReturnAction::Pass,
    }
}

/// How a hold ended, sent to the task that owns its dictation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoldEnd {
    Released,
    Cancelled,
}

/// The hold in progress. Its task cancels the recording when this drops
/// unsent, as when the listener goes away mid-hold.
pub(super) struct Hold(oneshot::Sender<HoldEnd>);

pub(super) struct KeyRouter {
    dictation: DictationHandle,
    /// Cancels a run a client's press started, so a tap does not dictate
    /// over it.
    chat: ChatHandle,
    /// Reads the active dictation, and stops a hold's recording by id.
    runtime: Arc<DictationRuntime>,
    /// Where combos go; the Mac app runs their actions.
    subscriptions: Arc<Subscriptions>,
}

impl KeyRouter {
    pub(super) fn new(
        dictation: DictationHandle,
        chat: ChatHandle,
        runtime: Arc<DictationRuntime>,
        subscriptions: Arc<Subscriptions>,
    ) -> Self {
        Self {
            dictation,
            chat,
            runtime,
            subscriptions,
        }
    }

    /// Does what `press` means. `hold` is the hold in progress, owned by
    /// the listener it came from.
    pub(super) fn press(&self, press: Press, hold: &mut Option<Hold>) {
        if press == Press::Tap {
            tracing::info!(trigger = "hotkey", "tap");
        }
        match press_action(press, self.runtime.active_dictation()) {
            PressAction::StopRecording(dictation) => self.stop_recording(&dictation),
            PressAction::CancelRunOrDictate => self.cancel_run_or_dictate(),
            PressAction::DictateHeld => *hold = Some(self.dictate_held()),
            PressAction::EndHold(end) => {
                // A hold task is only gone when the actor is; its end then
                // has nothing left to steer.
                if let Some(Hold(hold)) = hold.take() {
                    let _ = hold.send(end);
                }
            }
            PressAction::Broadcast(combo) => {
                tracing::info!(trigger = "hotkey", ?combo, "combo");
                // With no subscriber, the press goes nowhere.
                self.subscriptions.broadcast(combo);
            }
        }
    }

    /// True when the Return is taken and must not reach the app. Runs
    /// inside the hotkey tap, which every keystroke waits on.
    pub(super) fn take_return(&self) -> bool {
        match return_action(self.runtime.active_dictation(), frontmost_process) {
            ReturnAction::Take(dictation) => dictation.take_return(),
            ReturnAction::Pass => false,
        }
    }

    fn stop_recording(&self, dictation: &ActiveDictation) {
        if dictation.stop_recording() {
            tracing::info!(
                trigger = "hotkey",
                status = "stop_accepted",
                "hotkey tap stopped the running recording"
            );
            return;
        }
        // It stopped on its own meanwhile, so the tap finds nothing to stop.
        self.cancel_run_or_dictate();
    }

    /// One detached task per tap: a starting tap holds its call open for
    /// the whole dictation, and the next tap must not wait behind it.
    /// Between a press's recording stopping and its ticket, transcribing
    /// plus one round trip, a tap still dictates.
    fn cancel_run_or_dictate(&self) {
        let chat = self.chat.clone();
        let dictation = self.dictation.clone();
        tokio::spawn(async move {
            match chat.cancel_tapped().await {
                Ok(true) => tracing::info!(
                    trigger = "hotkey",
                    status = "cancelled",
                    "hotkey tap cancelled the run a press started"
                ),
                Ok(false) => log_outcome(
                    dictation
                        .start(DictationRequest::new(StopMode::Manual))
                        .await,
                ),
                Err(error) => log_chat_unreached(error),
            }
        });
    }

    /// One detached task per hold. It owns the hold's call, so the listener
    /// loop never waits on the actor; the hold's end reaches it through the
    /// returned `Hold`. A release stops the recording by id, so it can never
    /// be mistaken for a fresh start, and a cancel drops the call, which
    /// discards whatever was recorded.
    fn dictate_held(&self) -> Hold {
        tracing::info!(trigger = "hotkey", "hold started");
        let (end, ended) = oneshot::channel();
        let dictation = self.dictation.clone();
        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            // The id arrives with the Recording stage.
            let (recording, recorded) = watch::channel(None);
            let events = runtime.emitter_for_call(move |stage| {
                if let DictationStage::Recording { id, .. } = stage {
                    recording.send_replace(Some(id));
                }
            });
            // The release is the stop, so a pause mid-hold must not end it.
            let pending = match dictation
                .submit(
                    DictationRequest::new(StopMode::Manual),
                    AutoStopRule::Off,
                    Some(events),
                )
                .await
            {
                Ok(pending) => pending,
                Err(error) => return log_outcome(Err(error)),
            };
            // A listener that went away mid-hold cannot report its end;
            // dropping the call cancels the recording.
            let Ok(end) = ended.await else { return };
            tracing::info!(trigger = "hotkey", ?end, "hold ended");
            match end {
                // Released before capture opened: nothing was heard, and
                // stopping a recording the instant it opens would only
                // report no speech. Dropping the call cancels it quietly.
                HoldEnd::Released if recorded.borrow().is_none() => {
                    drop(pending);
                    tracing::info!(
                        trigger = "hotkey",
                        status = "cancelled",
                        "hold released before recording began"
                    );
                }
                HoldEnd::Released => {
                    let id = recorded.borrow().expect("checked above");
                    runtime.stop_recording(id);
                    log_outcome(pending.wait().await);
                }
                HoldEnd::Cancelled => {
                    drop(pending);
                    tracing::info!(
                        trigger = "hotkey",
                        status = "cancelled",
                        "hold dictation discarded"
                    );
                }
            }
        });
        Hold(end)
    }
}

fn log_outcome(outcome: Result<DictationOutcome, DictationCallError>) {
    match outcome {
        Ok(DictationOutcome::Completed(_)) => tracing::info!(
            trigger = "hotkey",
            status = "completed",
            "hotkey dictation finished"
        ),
        Ok(DictationOutcome::StopAccepted) => tracing::info!(
            trigger = "hotkey",
            status = "stop_accepted",
            "hotkey press stopped the running dictation"
        ),
        Err(DictationCallError::Runtime(DictationRuntimeError::Execution(
            DictationError::Cancelled,
        ))) => tracing::info!(
            trigger = "hotkey",
            status = "cancelled",
            "hotkey dictation finished"
        ),
        Err(DictationCallError::Unavailable) => tracing::info!(
            trigger = "hotkey",
            status = "unavailable",
            "hotkey dictation did not run; the daemon is stopping"
        ),
        Err(error @ (DictationCallError::Runtime(_) | DictationCallError::JobFailed)) => {
            tracing::warn!(
                trigger = "hotkey",
                status = "failed",
                %error,
                "hotkey dictation finished"
            );
        }
    }
}

fn log_chat_unreached(error: ChatCallError) {
    match error {
        CallError::Unavailable => tracing::info!(
            trigger = "hotkey",
            status = "unavailable",
            "hotkey tap did nothing; the daemon is stopping"
        ),
        error @ (CallError::Runtime(_) | CallError::JobFailed) => tracing::warn!(
            trigger = "hotkey",
            status = "failed",
            %error,
            "hotkey tap could not ask chat"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use aria_voice::dictation::testing::ActiveDictationBuilder;

    use super::*;

    const TYPED: u32 = 7;

    fn manual() -> ActiveDictationBuilder {
        ActiveDictationBuilder::new(StopMode::Manual)
    }

    #[test]
    fn a_tap_stops_only_a_manual_recording_it_can_still_stop() {
        let stops = |built: Option<ActiveDictationBuilder>| {
            let built = built.map(ActiveDictationBuilder::build);
            let dictation = built.as_ref().map(|built| built.dictation.clone());
            matches!(
                press_action(Press::Tap, dictation),
                PressAction::StopRecording(_)
            )
        };
        assert!(stops(Some(manual().with_recording())));
        assert!(!stops(None), "nothing records");
        assert!(!stops(Some(manual())), "it stopped recording already");
        assert!(
            !stops(Some(manual().with_recording().with_cancellation())),
            "it is ending"
        );
        assert!(
            !stops(Some(
                ActiveDictationBuilder::new(StopMode::OnSilence).with_recording()
            )),
            "silence ends it, not a tap"
        );
    }

    #[test]
    fn a_tap_with_nothing_to_stop_goes_to_a_run_or_dictates() {
        let stopped = manual().build();
        for dictation in [None, Some(stopped.dictation)] {
            assert!(matches!(
                press_action(Press::Tap, dictation),
                PressAction::CancelRunOrDictate
            ));
        }
    }

    #[test]
    fn a_hold_dictates_until_it_ends() {
        let recording = manual().with_recording().build();
        let dictation = || Some(recording.dictation.clone());
        assert!(matches!(
            press_action(Press::HoldStarted, dictation()),
            PressAction::DictateHeld
        ));
        assert!(matches!(
            press_action(Press::HoldEnded, dictation()),
            PressAction::EndHold(HoldEnd::Released)
        ));
        assert!(matches!(
            press_action(Press::HoldCancelled, dictation()),
            PressAction::EndHold(HoldEnd::Cancelled)
        ));
        assert!(matches!(
            press_action(Press::Combo(Combo::Shift), dictation()),
            PressAction::Broadcast(Combo::Shift)
        ));
    }

    #[test]
    fn only_a_return_where_the_text_goes_is_taken() {
        let typing = manual().with_recording().with_typing_into(TYPED).build();
        let takes = |front: u32| {
            matches!(
                return_action(Some(typing.dictation.clone()), || Some(front)),
                ReturnAction::Take(_)
            )
        };
        assert!(takes(TYPED));
        assert!(!takes(TYPED + 1), "another app's Return is its own");
    }

    #[test]
    fn a_return_asks_for_the_front_app_only_while_text_is_coming() {
        let asked = Cell::new(false);
        let front = || {
            asked.set(true);
            Some(TYPED)
        };
        let not_typing = manual().with_recording().build();
        let cancelled = manual()
            .with_recording()
            .with_typing_into(TYPED)
            .with_cancellation()
            .build();
        for dictation in [None, Some(not_typing.dictation), Some(cancelled.dictation)] {
            assert!(matches!(
                return_action(dictation, front),
                ReturnAction::Pass
            ));
        }
        assert!(!asked.get(), "no system call on an ordinary Return");
    }
}
