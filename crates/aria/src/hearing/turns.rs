//! The thread that hears a recording ahead of the decoder: the speech
//! detector first, then the tracker that reads its map. The caller only
//! reads the snapshots it publishes, so neither waits on the decoder.

use std::sync::Arc;
use std::thread::{Scope, ScopedJoinHandle};
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use aria_kit::speaker::{Ahead, SpeakerTracker, Timeline, TurnFeed};
use aria_kit::voice_activity::{SpeechMap, VoiceActivityDetector};

use super::board::{Board, Ending};

/// The most audio the tracker takes in one step, and how often an idle walk
/// checks the cancel token, which nothing rings.
pub(super) const PIECE: Duration = Duration::from_millis(100);
/// How far a tracker that only labels voices may fall behind the speech
/// detector before it skips ahead.
pub(super) const MAX_LABELING_LAG: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub(crate) enum TrackingError {
    #[error("speaker tracking is unavailable")]
    Unavailable,
    #[error("speaker tracking failed")]
    Failed,
    #[error("speaker tracking was cancelled")]
    Cancelled,
    #[error("could not start speaker tracking")]
    Spawn(#[source] std::io::Error),
}

/// What the walk has heard so far.
#[derive(Clone)]
pub(crate) struct Snapshot {
    pub(crate) speech: Arc<SpeechMap>,
    /// `None` when no tracker runs.
    pub(crate) timeline: Option<Arc<Timeline>>,
}

impl Snapshot {
    /// The tracker failed, so nothing more can be judged.
    pub(crate) fn tracking_failed(&self) -> bool {
        self.timeline
            .as_ref()
            .is_some_and(|timeline| timeline.is_broken())
    }

    /// This snapshot with nothing after it to be trusted: speech counts as
    /// unjudged, and no voice passes.
    pub(super) fn broken(&self) -> Self {
        let mut speech = (*self.speech).clone();
        speech.break_off();
        Self {
            speech: Arc::new(speech),
            timeline: self.timeline.as_ref().map(|_| Arc::new(Timeline::broken())),
        }
    }
}

pub(super) struct TurnThread<'scope> {
    board: Arc<Board>,
    worker: Option<ScopedJoinHandle<'scope, ()>>,
}

impl<'scope> TurnThread<'scope> {
    /// Starts hearing the recording `feed` reads: its detector scores every
    /// sample, and its tracker, when there is one, judges the turns. A
    /// tracker more than `max_lag` behind skips ahead. Each snapshot goes on
    /// `board`.
    pub(super) fn spawn<'env, V, K>(
        scope: &'scope Scope<'scope, 'env>,
        feed: TurnFeed<V, K>,
        max_lag: Option<Duration>,
        cancel: &CancelToken,
        board: &Arc<Board>,
    ) -> std::io::Result<Self>
    where
        V: VoiceActivityDetector + Send + 'scope,
        K: SpeakerTracker + Send + 'scope,
    {
        board.start_walk(
            feed.waker(),
            Snapshot {
                speech: Arc::new(feed.speech().clone()),
                timeline: feed.timeline().cloned().map(Arc::new),
            },
        );
        let spawned = std::thread::Builder::new()
            .name("aria-hearing-turns".into())
            .spawn_scoped(scope, {
                let board = Arc::clone(board);
                let cancel = cancel.clone();
                move || {
                    let mut done = Done {
                        board: &board,
                        ending: Ending::Panicked,
                    };
                    done.ending = walk(feed, max_lag, &board, &cancel);
                }
            });
        let worker = match spawned {
            Ok(worker) => worker,
            Err(error) => {
                board.end_walk(Ending::Stopped);
                return Err(error);
            }
        };
        Ok(Self {
            board: Arc::clone(board),
            worker: Some(worker),
        })
    }

    /// The speech map once the detector has scored through stream
    /// `position`, or as far as it got before it stopped. The decoder never
    /// reads past what capture has delivered, and the detector takes all of
    /// that first, so the wait is only for scoring audio already captured.
    pub(super) fn speech_through(&self, position: u64) -> Result<Arc<SpeechMap>, TrackingError> {
        self.board
            .wait_walk(|snapshot| {
                snapshot.speech.is_broken() || snapshot.speech.reached() >= position
            })
            .map(|snapshot| snapshot.speech)
    }

    /// Waits until the tracker, too, has heard through `position`.
    pub(super) fn heard_through(&self, position: u64) -> Result<(), TrackingError> {
        self.board
            .wait_walk(|snapshot| match &snapshot.timeline {
                Some(timeline) => timeline.is_broken() || timeline.reached() >= position,
                None => snapshot.speech.is_broken() || snapshot.speech.reached() >= position,
            })
            .map(drop)
    }

    /// Waits until the whole recording is heard. A walk that panicked
    /// fails, since its voices can no longer be judged.
    pub(super) fn finish(mut self) -> Result<Snapshot, TrackingError> {
        let worker = self.worker.take().expect("the walk is joined only once");
        if worker.join().is_err() {
            tracing::error!("the turn thread panicked");
            return Err(TrackingError::Failed);
        }
        match self.board.walk_ending() {
            Some(Ending::Heard) => Ok(self.board.snapshot()),
            Some(Ending::Panicked) => Err(TrackingError::Failed),
            Some(Ending::Stopped) | None => Err(TrackingError::Cancelled),
        }
    }

    /// Stops judging voices and waits only for the rest of the speech. A
    /// walk that panicked left a broken map, which counts as speech.
    pub(super) fn finish_speech(self) -> Result<Snapshot, TrackingError> {
        self.board.untrack();
        let board = Arc::clone(&self.board);
        let finished = self.finish();
        if board.walk_ending() == Some(Ending::Panicked) {
            return Ok(board.snapshot());
        }
        finished
    }
}

/// A walk dropped unfinished is stopped, then joined, so a panic in it
/// stays here rather than tearing down the scope.
impl Drop for TurnThread<'_> {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.board.abandon();
            if worker.join().is_err() {
                tracing::error!("the turn thread panicked");
            }
        }
    }
}

/// Records how the walk ended, however it ends: a panic leaves `ending` at
/// [`Ending::Panicked`].
struct Done<'a> {
    board: &'a Board,
    ending: Ending,
}

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.board.end_walk(self.ending);
    }
}

fn walk<V: VoiceActivityDetector, K: SpeakerTracker>(
    mut feed: TurnFeed<V, K>,
    max_lag: Option<Duration>,
    board: &Board,
    cancel: &CancelToken,
) -> Ending {
    loop {
        if board.abandoned() || cancel.is_cancelled() {
            return Ending::Stopped;
        }
        if board.untracked() {
            feed.drop_tracker();
        }
        match feed.hear_ahead(PIECE, max_lag) {
            Ahead::Heard => {}
            Ahead::Idle => {
                feed.wait_for_audio(PIECE, || board.abandoned() || cancel.is_cancelled());
                continue;
            }
            Ahead::Ended => break,
        }
        feed.forget_before(board.decided_to());
        publish(board, &feed);
        if feed.timeline().is_some_and(Timeline::is_broken) {
            // Nothing more can be judged, but the speech map still decides
            // whether anything was said.
            feed.drop_tracker();
        }
    }
    feed.hear_rest();
    publish(board, &feed);
    Ending::Heard
}

/// Publishes what was heard. Once the tracker is dropped, the last
/// timeline stands.
fn publish<V: VoiceActivityDetector, K: SpeakerTracker>(board: &Board, feed: &TurnFeed<V, K>) {
    let timeline = match feed.timeline() {
        Some(timeline) => Some(Arc::new(timeline.clone())),
        None => board.snapshot().timeline,
    };
    let snapshot = Snapshot {
        speech: Arc::new(feed.speech().clone()),
        timeline,
    };
    board.publish(snapshot);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use aria_kit::audio::{Recording, RecordingReader};
    use aria_kit::speaker::{Embedder, SpeakerError, VoiceprintTracker};
    use aria_kit::voice_activity::SpeechDetector;

    use super::*;
    use crate::dictation::testing::Loud;
    use crate::speaker::testing::{OWN, Voice, diarizer, tracker};
    use std::sync::atomic::Ordering;

    const SECOND: usize = 16_000;
    /// Panics at the first window it embeds.
    struct Panicking;

    impl Embedder for Panicking {
        fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
            panic!("the embedder broke")
        }

        fn dimension(&self) -> usize {
            2
        }
    }

    /// The timeline once the walk has heard through `position`, waiting at
    /// most two seconds.
    fn timeline_through(walk: &TurnThread<'_>, position: u64) -> Arc<Timeline> {
        let started = Instant::now();
        loop {
            let timeline = walk.board.snapshot().timeline.expect("a tracker runs");
            if timeline.reached() >= position || timeline.is_broken() {
                return timeline;
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the walk stalled"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_walk_cuts_a_finished_press_as_a_whole_pass_does() {
        let mut press = vec![0.0; SECOND / 2];
        press.extend([0.5; SECOND]);
        press.extend([0.0; SECOND]);
        press.extend([0.5; SECOND]);
        let mut backlog = vec![0.0; SECOND / 2];
        backlog.resize(12 * SECOND, 0.5);
        for (audio, written_before_the_walk) in [(press, false), (backlog, true)] {
            let (mut recording, writer) = if written_before_the_walk {
                (RecordingReader::from_samples(&audio), None)
            } else {
                let (recording, writer) = Recording::open();
                (recording, Some(writer))
            };
            let speech = SpeechDetector::new(Loud, 0);
            let mut diarizer = diarizer(Voice(OWN));
            let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));

            let timeline = std::thread::scope(|scope| {
                let walk =
                    TurnThread::spawn(scope, feed, None, &CancelToken::never(), &Board::new())
                        .unwrap();
                if let Some(mut writer) = writer {
                    for piece in audio.chunks(1_000) {
                        writer.push(piece);
                    }
                    writer.finish();
                }
                walk.finish().unwrap().timeline.unwrap()
            });
            assert_eq!(timeline.reached(), audio.len() as u64);

            let mut recording = RecordingReader::from_samples(&audio);
            let mut feed = TurnFeed::start(
                &mut recording,
                SpeechDetector::new(Loud, 0),
                Some(tracker(&mut diarizer, 0)),
            );
            feed.hear_rest();
            let whole_pass = feed.timeline().expect("a tracker runs");
            assert!(!whole_pass.is_broken());
            let whole = whole_pass.admitted_audio(&audio, 0);
            assert!(!whole.is_empty() && whole.len() < audio.len());
            assert_eq!(timeline.admitted_audio(&audio, 0), whole);
        }
    }

    #[test]
    fn a_listen_walk_forgets_what_was_decided() {
        let (mut recording, mut writer) = Recording::open();
        let speech = SpeechDetector::new(Loud, 0);
        let feed = TurnFeed::start(&mut recording, speech, None::<VoiceprintTracker<'_, Voice>>);

        let board = Board::new();

        let snapshot = std::thread::scope(|scope| {
            let walk = TurnThread::spawn(scope, feed, None, &CancelToken::never(), &board).unwrap();
            board.decided_before(2 * SECOND as u64);
            for _ in 0..2 {
                writer.push(&[0.5; SECOND]);
                writer.push(&[0.0; SECOND]);
            }
            writer.finish();
            walk.finish().unwrap()
        });

        assert_eq!(snapshot.speech.speech(&(0..SECOND as u64)), 0, "forgotten");
        assert_eq!(
            snapshot.speech.speech_before(snapshot.speech.reached()),
            2 * SECOND as u64,
            "the count of speech stays whole"
        );
        assert!(snapshot.speech.is_finished());
    }

    #[test]
    fn dropping_the_walk_stops_it_while_capture_runs() {
        let (mut recording, mut writer) = Recording::open();
        let speech = SpeechDetector::new(Loud, 0);
        let mut diarizer = diarizer(Voice(OWN));
        let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));
        writer.push(&[0.5; SECOND]);

        let started = Instant::now();
        std::thread::scope(|scope| {
            let walk =
                TurnThread::spawn(scope, feed, None, &CancelToken::never(), &Board::new()).unwrap();
            timeline_through(&walk, SECOND as u64);
            drop(walk);
        });

        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer);
    }

    #[test]
    fn a_walk_that_panics_lets_nothing_more_pass() {
        for speech_only in [false, true] {
            let mut recording = RecordingReader::from_samples(&[0.5; SECOND]);
            let speech = SpeechDetector::new(Loud, 0);
            let mut diarizer = diarizer(Panicking);
            let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));

            std::thread::scope(|scope| {
                let walk =
                    TurnThread::spawn(scope, feed, None, &CancelToken::never(), &Board::new())
                        .unwrap();
                assert!(timeline_through(&walk, SECOND as u64).is_broken());
                assert!(walk.speech_through(SECOND as u64).unwrap().is_broken());
                if speech_only {
                    let snapshot = walk.finish_speech();
                    assert!(snapshot.unwrap().speech.is_broken());
                } else {
                    assert!(matches!(walk.finish(), Err(TrackingError::Failed)));
                }
            });
        }
    }

    #[test]
    fn a_walk_that_panics_wakes_its_reader() {
        use std::sync::mpsc::channel;

        use super::super::board::Wake;

        let mut recording = RecordingReader::from_samples(&[0.5; SECOND]);
        let speech = SpeechDetector::new(Loud, 0);
        let mut diarizer = diarizer(Panicking);
        let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));
        let board = Board::new();
        let (woken, wakes) = channel();

        std::thread::scope(|scope| {
            let walk = TurnThread::spawn(scope, feed, None, &CancelToken::never(), &board).unwrap();
            scope.spawn(|| {
                while matches!(board.next_wake(), Wake::Heard) {
                    if board.snapshot().tracking_failed() {
                        let _ = woken.send(());
                        return;
                    }
                }
            });
            assert!(
                wakes.recv_timeout(Duration::from_secs(2)).is_ok(),
                "the reader never woke for the broken snapshot"
            );
            assert!(matches!(walk.finish(), Err(TrackingError::Failed)));
        });
    }

    #[test]
    fn abandonment_stops_before_another_check() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc;

        struct BlockedOwner {
            entered: mpsc::Sender<()>,
            resume: mpsc::Receiver<()>,
            calls: Arc<AtomicUsize>,
        }

        impl Embedder for BlockedOwner {
            fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.entered.send(()).unwrap();
                    self.resume.recv().unwrap();
                }
                Ok(Some(OWN.to_vec()))
            }

            fn dimension(&self) -> usize {
                2
            }
        }

        let (entered, waiting) = mpsc::channel();
        let (resume, blocked) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut diarizer = diarizer(BlockedOwner {
            entered,
            resume: blocked,
            calls: Arc::clone(&calls),
        });
        let speech = SpeechDetector::new(Loud, 0);
        let mut recording = RecordingReader::from_samples(&vec![0.5; 5 * SECOND]);
        let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));
        std::thread::scope(|scope| {
            let walk =
                TurnThread::spawn(scope, feed, None, &CancelToken::never(), &Board::new()).unwrap();
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            walk.board.abandon();
            resume.send(()).unwrap();
            drop(walk);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelling_a_wait_does_not_need_capture_to_end() {
        let (mut recording, _writer) = Recording::open();
        let speech = SpeechDetector::new(Loud, 0);
        let mut diarizer = diarizer(Voice(OWN));
        let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));
        let cancel = CancelToken::new();
        std::thread::scope(|scope| {
            let walk = TurnThread::spawn(scope, feed, None, &cancel, &Board::new()).unwrap();
            cancel.cancel();
            assert!(matches!(
                walk.speech_through(SECOND as u64),
                Err(TrackingError::Cancelled)
            ));
            assert!(matches!(walk.finish(), Err(TrackingError::Cancelled)));
        });
    }

    #[test]
    fn an_empty_finished_recording_ends_the_walk() {
        let mut recording = RecordingReader::from_samples(&[]);
        let speech = SpeechDetector::new(Loud, 0);
        let mut diarizer = diarizer(Voice(OWN));
        let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));
        std::thread::scope(|scope| {
            let snapshot =
                TurnThread::spawn(scope, feed, None, &CancelToken::never(), &Board::new())
                    .unwrap()
                    .finish()
                    .unwrap();
            assert_eq!(snapshot.timeline.unwrap().reached(), 0);
            assert_eq!(snapshot.speech.reached(), 0);
        });
    }
}
