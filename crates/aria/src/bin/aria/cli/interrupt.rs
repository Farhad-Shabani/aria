//! What Esc and Ctrl-C do while a command is working.

use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use anyhow::Context;
use signal_hook::consts::SIGINT;

use aria::daemon::{DaemonSession, DeliveryMode, DictationStage, RecordingId, Ticket};

use super::activity::Activity;
use super::keys::{Keys, restore_terminal};
use super::lock;

/// 128 + SIGINT: the status a shell reports for a command Ctrl-C ended.
pub(crate) const INTERRUPTED: i32 = 130;

/// What the next press does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interruption {
    StopRecording(RecordingId),
    Cancel(Ticket),
}

/// What the caller does once a press has been served.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Handled,
    Quit,
}

/// What a press acts on. The daemon session is the real one; a test brings
/// its own.
trait Target: Send + Sync {
    fn stop_recording(&self, recording: RecordingId) -> anyhow::Result<bool>;
    fn cancel_ask(&self, ticket: Ticket) -> anyhow::Result<bool>;
}

impl Target for DaemonSession {
    fn stop_recording(&self, recording: RecordingId) -> anyhow::Result<bool> {
        Ok(self.stop_dictation(recording)?)
    }

    fn cancel_ask(&self, ticket: Ticket) -> anyhow::Result<bool> {
        Ok(self.cancel(ticket)?)
    }
}

/// Serves a press, whether it arrived as a signal or a key.
struct Router {
    /// Weak, so the command's own handle is the last one: the session
    /// stops the daemon it started, and that belongs on the main thread.
    target: Weak<dyn Target>,
    /// The one source of truth for what a press does. `None` whenever
    /// there is nothing to act on: between phases, and before the daemon
    /// has named a recording.
    pending: Mutex<Option<Interruption>>,
    /// Read by the conditional-default action on every SIGINT: true means
    /// take the default action, which ends the process. Set by a cancel,
    /// so that the press after it ends the wait.
    ends_process: Arc<AtomicBool>,
    /// Set once a cancel went out, so the failure that follows is read as
    /// this one rather than the agent's.
    cancelled: AtomicBool,
    failure: Mutex<Option<anyhow::Error>>,
}

impl Router {
    fn press(&self) -> Flow {
        let Some(target) = self.target.upgrade() else {
            return Flow::Quit;
        };
        let action = *lock(&self.pending);
        match action {
            // The utterance ends and the work goes on — even when the
            // recording was already over. Capture can end on its own a
            // moment before the client hears so; quitting there would hang
            // up on a press past the point of stopping.
            Some(Interruption::StopRecording(recording)) => {
                match target.stop_recording(recording) {
                    Ok(_) => Flow::Handled,
                    Err(error) => {
                        *lock(&self.failure) = Some(error);
                        Flow::Quit
                    }
                }
            }
            // A cancel is the last graceful act. Nothing is left for a
            // second press, so the phase disarms itself here.
            Some(Interruption::Cancel(ticket)) => {
                *lock(&self.pending) = None;
                self.cancelled.store(true, Ordering::Relaxed);
                self.ends_process.store(true, Ordering::Relaxed);
                if let Err(error) = target.cancel_ask(ticket) {
                    *lock(&self.failure) = Some(error);
                }
                Flow::Handled
            }
            None => Flow::Quit,
        }
    }
}

/// Esc requests a stop; Ctrl-C also exits when the command owns the keyboard.
/// Without keyboard input, Ctrl-C requests the stop first and exits on a
/// later press. A [`Phase`] owns the current action and clears it on drop.
pub(crate) struct Interrupt {
    router: Arc<Router>,
    pipe: signal_hook::SigId,
}

impl Interrupt {
    /// Watches for Ctrl-C with nothing yet to act on. Until a phase is
    /// named, a press ends the process.
    pub(crate) fn install(session: &Arc<DaemonSession>) -> anyhow::Result<Self> {
        let ends_process = Arc::new(AtomicBool::new(false));
        let shared: Arc<dyn Target> = session.clone();
        let target = Arc::downgrade(&shared);
        let router = Arc::new(Router {
            target,
            pending: Mutex::new(None),
            ends_process: Arc::clone(&ends_process),
            cancelled: AtomicBool::new(false),
            failure: Mutex::new(None),
        });
        // Registered first, so it runs first: once the flag is set, a press
        // ends the process before the pipe could act on it again.
        signal_hook::flag::register_conditional_default(SIGINT, ends_process)
            .context("failed to watch for Ctrl-C")?;
        let (reader, writer) = UnixStream::pair().context("failed to open the interrupt pipe")?;
        let pipe = signal_hook::low_level::pipe::register(SIGINT, writer)
            .context("failed to watch for Ctrl-C")?;
        thread::spawn({
            let router = Arc::clone(&router);
            move || {
                let mut reader = reader;
                let mut byte = [0u8; 1];
                loop {
                    match reader.read(&mut byte) {
                        Ok(1) => {}
                        // `read` does not retry itself, and a signal
                        // without SA_RESTART would end the watch for good.
                        Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                        _ => break,
                    }
                    if let Flow::Quit = router.press() {
                        quit();
                    }
                }
            }
        });
        Ok(Self { router, pipe })
    }

    /// Esc ends the utterance being recorded, once the daemon has named
    /// it through [`Phase::follow`]. Until then Esc does nothing, and Ctrl-C
    /// hangs up, which cancels the dictation anyway. The line shows at
    /// once.
    ///
    /// A streamed dictation types each segment into whatever is in front while
    /// it is still recording, and that may be this terminal. Holding the
    /// keyboard would swallow that text, so Ctrl-C is the only key there.
    pub(crate) fn recording(&mut self, delivery: DeliveryMode) -> Phase<'_> {
        let keyboard = match delivery {
            DeliveryMode::Complete => Keyboard::Ours,
            DeliveryMode::Streaming => Keyboard::TypedInto,
        };
        let mut phase = self.arm(None, "stop", keyboard);
        phase.show("listening…");
        phase
    }

    /// Esc cancels this ask. The line stays empty until the agent reports
    /// a step.
    pub(crate) fn asking(&mut self, ticket: Ticket) -> Phase<'_> {
        self.arm(Some(Interruption::Cancel(ticket)), "cancel", Keyboard::Ours)
    }

    /// `&mut self` so two phases cannot overlap: they share one armed
    /// slot and one terminal, and the second to end would put back what
    /// the first had already restored.
    fn arm(
        &mut self,
        interruption: Option<Interruption>,
        verb: &str,
        keyboard: Keyboard,
    ) -> Phase<'_> {
        *lock(&self.router.pending) = interruption;
        let keys = match keyboard {
            Keyboard::TypedInto => None,
            Keyboard::Ours => self.watch_keys(),
        };
        let key = match keys {
            Some(_) => format!("esc to {verb}"),
            None => format!("ctrl-c to {verb}"),
        };
        Phase {
            interrupt: self,
            activity: Activity::start(key),
            keys,
        }
    }

    fn watch_keys(&self) -> Option<Keys> {
        Keys::watch(
            {
                let router = Arc::clone(&self.router);
                // Esc never ends the process. A press with nothing to
                // stop does nothing, which is what Esc means elsewhere.
                move || {
                    router.press();
                }
            },
            {
                let router = Arc::clone(&self.router);
                move || {
                    router.press();
                    quit();
                }
            },
        )
    }
}

/// Who may hold the keyboard while a phase runs.
enum Keyboard {
    Ours,
    /// The daemon types into whatever is in front while the phase runs.
    TypedInto,
}

impl Drop for Interrupt {
    /// Closing the pipe ends the reader thread; after this Ctrl-C ends
    /// the process, since the flag stays true from here on.
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.pipe);
        self.router.ends_process.store(true, Ordering::Relaxed);
    }
}

/// Hands the terminal back before going, on whichever thread got there.
fn quit() -> ! {
    restore_terminal();
    std::process::exit(INTERRUPTED)
}

/// One stretch of work a press can act on, and the line describing it.
pub(crate) struct Phase<'a> {
    interrupt: &'a mut Interrupt,
    activity: Activity,
    keys: Option<Keys>,
}

impl Phase<'_> {
    pub(crate) fn show(&mut self, text: impl Into<String>) {
        self.activity.show(text);
    }

    pub(crate) fn seconds(&self) -> u64 {
        self.activity.seconds()
    }

    /// The two boundaries a press acts on. Once the daemon names the
    /// recording a press can end it. Once capture is over the keyboard
    /// goes back — before the daemon types into whatever is in front, which
    /// may be this terminal — and the press is disarmed, since there is
    /// nothing left to stop. The line stays, minus its key.
    pub(crate) fn follow(&mut self, stage: &DictationStage) {
        match stage {
            DictationStage::Recording(recording) => {
                *lock(&self.interrupt.router.pending) =
                    Some(Interruption::StopRecording(*recording));
            }
            DictationStage::Transcribing => {
                self.disarm();
                self.activity.drop_key();
            }
            _ => {}
        }
    }

    fn disarm(&mut self) {
        self.keys.take();
        *lock(&self.interrupt.router.pending) = None;
    }

    /// Whether the work ended because a press cancelled it.
    pub(crate) fn cancelled(&self) -> bool {
        self.interrupt.router.cancelled.load(Ordering::Relaxed)
    }

    /// Why an interruption did not reach the daemon, once the line is
    /// free. Taken, so a second call does not report it twice.
    pub(crate) fn take_failure(&self) -> Option<anyhow::Error> {
        lock(&self.interrupt.router.failure).take()
    }

    /// The work never started, so the line describing it is wiped.
    pub(crate) fn erase(&mut self) {
        self.disarm();
        self.activity.erase();
    }

    /// The work is over. Hands the terminal back, disarms the press and
    /// ends the line, so whatever is printed next starts on its own.
    ///
    /// Left armed, a finished phase spends the press on work that is over.
    pub(crate) fn finish(&mut self) {
        self.disarm();
        self.activity.finish();
    }
}

impl Drop for Phase<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTarget {
        stopped: Mutex<Vec<()>>,
        cancelled: Mutex<Vec<Ticket>>,
        was_recording: bool,
        broken: bool,
    }

    impl Target for FakeTarget {
        fn stop_recording(&self, _recording: RecordingId) -> anyhow::Result<bool> {
            if self.broken {
                anyhow::bail!("the daemon is gone");
            }
            lock(&self.stopped).push(());
            Ok(self.was_recording)
        }

        fn cancel_ask(&self, ticket: Ticket) -> anyhow::Result<bool> {
            lock(&self.cancelled).push(ticket);
            Ok(true)
        }
    }

    fn router(target: &Arc<FakeTarget>, pending: Option<Interruption>) -> Router {
        let shared: Arc<dyn Target> = target.clone();
        Router {
            target: Arc::downgrade(&shared),
            pending: Mutex::new(pending),
            ends_process: Arc::new(AtomicBool::new(false)),
            cancelled: AtomicBool::new(false),
            failure: Mutex::new(None),
        }
    }

    fn ticket() -> Ticket {
        Ticket(1)
    }

    fn stopping_recording_one() -> Option<Interruption> {
        Some(Interruption::StopRecording(RecordingId(1)))
    }

    #[test]
    fn a_press_while_recording_stops_the_utterance() {
        let target = Arc::new(FakeTarget {
            was_recording: true,
            ..FakeTarget::default()
        });
        let router = router(&target, stopping_recording_one());

        assert_eq!(router.press(), Flow::Handled);
        assert_eq!(lock(&target.stopped).len(), 1);
        // Still armed: the recording may end again on its own.
        assert_eq!(*lock(&router.pending), stopping_recording_one());
    }

    #[test]
    fn a_stop_that_finds_the_recording_over_carries_on() {
        let target = Arc::new(FakeTarget::default());
        let router = router(&target, stopping_recording_one());

        assert_eq!(router.press(), Flow::Handled);
        assert!(lock(&router.failure).is_none());
    }

    #[test]
    fn a_stop_that_fails_is_kept_for_the_caller() {
        let target = Arc::new(FakeTarget {
            broken: true,
            ..FakeTarget::default()
        });
        let router = router(&target, stopping_recording_one());

        assert_eq!(router.press(), Flow::Quit);
        assert!(lock(&router.failure).is_some());
    }

    #[test]
    fn a_press_while_asking_cancels_once() {
        let target = Arc::new(FakeTarget::default());
        let router = router(&target, Some(Interruption::Cancel(ticket())));

        assert_eq!(router.press(), Flow::Handled);
        assert_eq!(router.press(), Flow::Quit);
        assert_eq!(*lock(&target.cancelled), vec![ticket()]);
        assert!(router.cancelled.load(Ordering::Relaxed));
    }

    /// Also the state before the daemon has named a recording: nothing to
    /// stop, and hanging up cancels the dictation.
    #[test]
    fn a_press_with_nothing_armed_quits() {
        let target = Arc::new(FakeTarget::default());
        let router = router(&target, None);

        assert_eq!(router.press(), Flow::Quit);
        assert!(lock(&target.stopped).is_empty());
    }

    #[test]
    fn a_press_after_the_command_let_go_quits() {
        let target = Arc::new(FakeTarget {
            was_recording: true,
            ..FakeTarget::default()
        });
        let router = router(&target, stopping_recording_one());
        drop(target);

        assert_eq!(router.press(), Flow::Quit);
    }
}
