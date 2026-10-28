//! What one hearing's threads tell each other: the decoder's updates, the
//! walk's snapshots, and when either ends or nobody will listen. Every
//! change rings one condvar, so no wait needs a timeout to notice it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use aria_kit::audio::RecordingWaker;
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::HeardUpdate;

use super::turns::{Snapshot, TrackingError};

/// Updates in flight before the decoder waits for the caller's thread.
const UPDATES_KEPT: usize = 64;

pub(crate) struct Board {
    state: Mutex<State>,
    changed: Condvar,
    /// Set once nobody will read another update or snapshot. Kept outside
    /// the lock so the engines' cancel checks stay cheap; it is only ever
    /// set under the lock, so a waiter cannot miss it.
    abandoned: AtomicBool,
}

#[derive(Default)]
struct State {
    decoded: VecDeque<Decoded>,
    decoder_done: bool,
    /// `None` until a walk starts, and in a hearing without one.
    walk: Option<Walk>,
}

struct Walk {
    /// Rings the walk awake while it waits for audio.
    bell: RecordingWaker,
    snapshot: Snapshot,
    /// Published since the caller last woke for it.
    heard: bool,
    ended: Option<Ending>,
    /// The caller needs only the rest of the speech, not voices.
    untracked: bool,
}

pub(super) struct Decoded {
    pub(super) update: HeardUpdate,
    pub(super) read_to: u64,
}

/// What the caller's thread woke for.
pub(super) enum Wake {
    Decoded(Decoded),
    /// The walk published a snapshot, or ended.
    Heard,
    DecoderDone,
}

/// How the walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ending {
    /// It heard the whole recording.
    Heard,
    /// It stopped early: cancelled, or abandoned by its reader.
    Stopped,
    /// Nothing after its broken snapshot can be trusted.
    Panicked,
}

/// Nobody will read another update.
#[derive(Debug)]
pub(super) struct Abandoned;

impl Board {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            abandoned: AtomicBool::new(false),
        })
    }

    /// The token a hearing's engines and walk check: it stops once the
    /// hearing is abandoned, or whenever `stop` says so.
    pub(crate) fn token(
        self: &Arc<Self>,
        stop: impl Fn() -> bool + Send + Sync + 'static,
    ) -> CancelToken {
        let board = Arc::clone(self);
        CancelToken::from_fn(move || stop() || board.abandoned())
    }

    pub(super) fn abandon(&self) {
        let state = self.lock();
        self.abandoned.store(true, Ordering::Release);
        let bell = state.walk.as_ref().map(|walk| walk.bell.clone());
        drop(state);
        self.changed.notify_all();
        if let Some(bell) = bell {
            bell.wake();
        }
    }

    pub(super) fn abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }

    /// Waits for room. Once the hearing is abandoned, the update is dropped.
    pub(super) fn push_decoded(&self, decoded: Decoded) -> Result<(), Abandoned> {
        let mut state = self.wait_while(self.lock(), |state| state.decoded.len() >= UPDATES_KEPT);
        if self.abandoned() {
            return Err(Abandoned);
        }
        state.decoded.push_back(decoded);
        self.notify(state);
        Ok(())
    }

    pub(super) fn decoder_done(&self) {
        let mut state = self.lock();
        state.decoder_done = true;
        self.notify(state);
    }

    /// The next thing the caller's thread acts on, in the order the threads
    /// produced it: updates first, then a fresh snapshot, then the decoder's
    /// end once no update is left.
    pub(super) fn next_wake(&self) -> Wake {
        let mut state = self.wait_while(self.lock(), |state| {
            state.decoded.is_empty()
                && !state.walk.as_ref().is_some_and(|walk| walk.heard)
                && !state.decoder_done
        });
        if let Some(decoded) = state.decoded.pop_front() {
            drop(state);
            // A decoder waiting for room can go on.
            self.changed.notify_all();
            return Wake::Decoded(decoded);
        }
        if let Some(walk) = state.walk.as_mut().filter(|walk| walk.heard) {
            walk.heard = false;
            return Wake::Heard;
        }
        Wake::DecoderDone
    }

    pub(super) fn start_walk(&self, bell: RecordingWaker, snapshot: Snapshot) {
        self.lock().walk = Some(Walk {
            bell,
            snapshot,
            heard: false,
            ended: None,
            untracked: false,
        });
    }

    pub(super) fn publish(&self, snapshot: Snapshot) {
        let mut state = self.lock();
        let walk = walk_mut(&mut state);
        walk.snapshot = snapshot;
        walk.heard = true;
        self.notify(state);
    }

    /// A walk that panicked leaves a broken snapshot: speech counts as
    /// unjudged, and no voice passes.
    pub(super) fn end_walk(&self, ending: Ending) {
        let mut state = self.lock();
        let walk = walk_mut(&mut state);
        if ending == Ending::Panicked {
            walk.snapshot = walk.snapshot.broken();
        }
        walk.ended = Some(ending);
        walk.heard = true;
        self.notify(state);
    }

    /// The walk's next fresh snapshot, waiting for it. Its end counts as one.
    pub(super) fn next_snapshot(&self) -> Snapshot {
        let mut state = self.wait_while(self.lock(), |state| !walk(state).heard);
        let walk = walk_mut(&mut state);
        walk.heard = false;
        walk.snapshot.clone()
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        walk(&self.lock()).snapshot.clone()
    }

    pub(super) fn walk_ending(&self) -> Option<Ending> {
        walk(&self.lock()).ended
    }

    pub(super) fn untrack(&self) {
        walk_mut(&mut self.lock()).untracked = true;
    }

    pub(super) fn untracked(&self) -> bool {
        walk(&self.lock()).untracked
    }

    /// The snapshot once `heard` holds for it, or as far as the walk got
    /// before it ended.
    pub(super) fn wait_walk(
        &self,
        heard: impl Fn(&Snapshot) -> bool,
    ) -> Result<Snapshot, TrackingError> {
        let state = self.wait_while(self.lock(), |state| {
            let walk = walk(state);
            walk.ended.is_none() && !heard(&walk.snapshot)
        });
        let walk = walk(&state);
        if heard(&walk.snapshot) {
            return Ok(walk.snapshot.clone());
        }
        match walk.ended {
            Some(Ending::Heard | Ending::Panicked) => Ok(walk.snapshot.clone()),
            Some(Ending::Stopped) | None => Err(TrackingError::Cancelled),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Waits while `pending` holds and nobody abandoned the hearing.
    fn wait_while<'a>(
        &self,
        state: MutexGuard<'a, State>,
        mut pending: impl FnMut(&mut State) -> bool,
    ) -> MutexGuard<'a, State> {
        self.changed
            .wait_while(state, |state| !self.abandoned() && pending(state))
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn notify(&self, state: MutexGuard<'_, State>) {
        drop(state);
        self.changed.notify_all();
    }
}

fn walk(state: &State) -> &Walk {
    state
        .walk
        .as_ref()
        .expect("a turn thread starts its walk on the board")
}

fn walk_mut(state: &mut State) -> &mut Walk {
    state
        .walk
        .as_mut()
        .expect("a turn thread starts its walk on the board")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aria_kit::audio::RecordingReader;
    use aria_kit::voice_activity::SpeechMap;

    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            speech: Arc::new(SpeechMap::default()),
            timeline: None,
        }
    }

    fn update() -> Decoded {
        Decoded {
            update: HeardUpdate::Provisional {
                text: String::new(),
                source_window: None,
            },
            read_to: 0,
        }
    }

    #[test]
    fn two_publishes_wake_the_caller_once_and_a_later_one_again() {
        let board = Board::new();
        let recording = RecordingReader::from_samples(&[]);
        board.start_walk(recording.waker(), snapshot());
        board.publish(snapshot());
        board.publish(snapshot());
        board.decoder_done();

        assert!(matches!(board.next_wake(), Wake::Heard));
        assert!(matches!(board.next_wake(), Wake::DecoderDone));
        board.publish(snapshot());
        assert!(matches!(board.next_wake(), Wake::Heard));
    }

    #[test]
    fn an_abandoned_hearing_frees_a_decoder_waiting_for_room() {
        let board = Board::new();
        for _ in 0..UPDATES_KEPT {
            board.push_decoded(update()).expect("there is room");
        }
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| board.push_decoded(update()));
            std::thread::sleep(Duration::from_millis(20));
            assert!(
                !waiting.is_finished(),
                "a full board makes the decoder wait"
            );

            board.abandon();

            assert!(waiting.join().unwrap().is_err());
        });
    }
}
