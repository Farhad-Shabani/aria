//! The thread that hears a recording ahead of the decoder: the speech
//! detector first, then the tracker that reads its map. The caller only
//! reads the snapshots it publishes, so neither waits on the decoder.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{Scope, ScopedJoinHandle};
use std::time::Duration;

use aria_kit::audio::canonical_samples;
use aria_kit::cancel::CancelToken;
use aria_kit::speaker::{Admission, SpeakerTracker, Timeline, TurnFeed};
use aria_kit::voice_activity::{SpeechMap, VoiceActivityDetector};

/// The longest the walk sleeps without new audio, and the most audio the
/// tracker takes in one step. It is also how soon an abandoned walk notices.
pub(super) const PIECE: Duration = Duration::from_millis(100);
const MAX_NAMING_LAG: u64 = canonical_samples(Duration::from_secs(10));

#[derive(Debug, thiserror::Error)]
pub(crate) enum TrackingError {
    #[error("speaker tracking is unavailable")]
    Unavailable,
    #[error("speaker tracking failed")]
    Failed,
    #[error("speaker tracking panicked")]
    Panicked,
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
}

pub(super) struct TurnThread<'scope> {
    shared: Arc<Shared>,
    cancel: CancelToken,
    worker: Option<ScopedJoinHandle<'scope, ()>>,
}

struct Shared {
    published: Mutex<Published>,
    changed: Condvar,
    /// Set when nobody will read the snapshots again.
    abandoned: AtomicBool,
    /// Set when the caller needs only the rest of the speech, not voices.
    untracked: AtomicBool,
}

struct Published {
    snapshot: Snapshot,
    /// The walk has stopped: the snapshot will not change again.
    done: bool,
}

impl<'scope> TurnThread<'scope> {
    /// Starts hearing the recording `feed` reads: its detector scores every
    /// sample, and its tracker, when there is one, judges the turns.
    /// `progress` sees each snapshot before anyone can read it, and
    /// `readable` runs once it can be read.
    pub(super) fn spawn<'env, V, K>(
        scope: &'scope Scope<'scope, 'env>,
        feed: TurnFeed<'scope, V, K>,
        cancel: &CancelToken,
        mut progress: impl FnMut(&Snapshot) + Send + 'scope,
        mut readable: impl FnMut() + Send + 'scope,
    ) -> std::io::Result<Self>
    where
        V: VoiceActivityDetector + Send,
        K: SpeakerTracker + Send,
    {
        let shared = Arc::new(Shared {
            published: Mutex::new(Published {
                snapshot: Snapshot {
                    speech: Arc::new(feed.speech().clone()),
                    timeline: feed
                        .voices()
                        .map(|voices| Arc::new(voices.timeline().clone())),
                },
                done: false,
            }),
            changed: Condvar::new(),
            abandoned: AtomicBool::new(false),
            untracked: AtomicBool::new(false),
        });
        let worker = std::thread::Builder::new()
            .name("aria-hearing-turns".into())
            .spawn_scoped(scope, {
                let shared = Arc::clone(&shared);
                let cancel = cancel.clone();
                move || {
                    let _done = Done(&shared);
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        walk(feed, &shared, &cancel, &mut progress, &mut readable);
                    }));
                    if let Err(panic) = outcome {
                        let broken = shared.broken();
                        progress(&broken);
                        shared.publish(broken);
                        readable();
                        std::panic::resume_unwind(panic);
                    }
                }
            })?;
        Ok(Self {
            shared,
            cancel: cancel.clone(),
            worker: Some(worker),
        })
    }

    /// What the walk has heard so far, without waiting.
    pub(super) fn snapshot(&self) -> Snapshot {
        self.shared.lock().snapshot.clone()
    }

    /// The speech map once the detector has scored through stream
    /// `position`, or as far as it got before it stopped. The decoder never
    /// reads past what capture has delivered, and the detector takes all of
    /// that first, so the wait is only for scoring audio already captured.
    pub(super) fn speech_through(&self, position: u64) -> Result<Arc<SpeechMap>, TrackingError> {
        self.wait_for(|snapshot| {
            snapshot.speech.is_broken() || snapshot.speech.reached() >= position
        })
        .map(|snapshot| snapshot.speech)
    }

    /// The snapshot once the tracker, too, has heard through `position`.
    pub(super) fn heard_through(&self, position: u64) -> Result<Snapshot, TrackingError> {
        self.wait_for(|snapshot| match &snapshot.timeline {
            Some(timeline) => timeline.is_broken() || timeline.reached() >= position,
            None => snapshot.speech.is_broken() || snapshot.speech.reached() >= position,
        })
    }

    fn wait_for(&self, heard: impl Fn(&Snapshot) -> bool) -> Result<Snapshot, TrackingError> {
        let mut published = self.shared.lock();
        loop {
            if published.done || heard(&published.snapshot) {
                return Ok(published.snapshot.clone());
            }
            if self.cancel.is_cancelled() {
                return Err(TrackingError::Cancelled);
            }
            (published, _) = self
                .shared
                .changed
                .wait_timeout(published, PIECE)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Waits until the whole recording is heard. A walk that panicked heard
    /// nothing more, so nothing after it passes.
    pub(super) fn finish(mut self) -> Result<Snapshot, TrackingError> {
        let worker = self.worker.take().expect("the walk is joined only once");
        if worker.join().is_err() {
            tracing::error!("the turn thread panicked; nothing more passes");
            return Err(TrackingError::Panicked);
        }
        if self.cancel.is_cancelled() {
            return Err(TrackingError::Cancelled);
        }
        Ok(self.snapshot())
    }

    /// Stops judging voices and waits only for the rest of the speech. A
    /// walk that panicked left a broken map, which counts as speech.
    pub(super) fn finish_speech(self) -> Result<Snapshot, TrackingError> {
        self.shared.untracked.store(true, Ordering::Relaxed);
        let shared = Arc::clone(&self.shared);
        match self.finish() {
            Err(TrackingError::Panicked) => Ok(shared.lock().snapshot.clone()),
            finished => finished,
        }
    }
}

/// Every way out of a hearing stops the walk and joins it, so a panic in
/// it stays here rather than tearing down the scope.
impl Drop for TurnThread<'_> {
    fn drop(&mut self) {
        self.shared.abandoned.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("the turn thread panicked");
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Published> {
        self.published
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish(&self, snapshot: Snapshot) {
        self.lock().snapshot = snapshot;
        self.changed.notify_all();
    }

    /// The last snapshot, with nothing after it to be trusted: speech counts
    /// as unjudged, and no voice passes.
    fn broken(&self) -> Snapshot {
        let snapshot = self.lock().snapshot.clone();
        let mut speech = (*snapshot.speech).clone();
        speech.break_off();
        Snapshot {
            speech: Arc::new(speech),
            timeline: snapshot.timeline.map(|_| Arc::new(Timeline::broken())),
        }
    }
}

/// Marks the walk stopped however it ends, a panic included, so nobody waits
/// on a snapshot that will not change.
struct Done<'a>(&'a Shared);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let broken = self.0.broken();
            self.0.lock().snapshot = broken;
        }
        self.0.lock().done = true;
        self.0.changed.notify_all();
    }
}

fn walk<V: VoiceActivityDetector, K: SpeakerTracker>(
    mut feed: TurnFeed<'_, V, K>,
    shared: &Shared,
    cancel: &CancelToken,
    progress: &mut impl FnMut(&Snapshot),
    readable: &mut impl FnMut(),
) {
    let admission = feed.admission();
    loop {
        if shared.abandoned.load(Ordering::Relaxed) || cancel.is_cancelled() {
            return;
        }
        if shared.untracked.load(Ordering::Relaxed) && feed.voices().is_some() {
            feed.drop_voices();
        }
        let tracking_behind = feed
            .voices()
            .is_some_and(|voices| voices.heard_to() < feed.spoken_to());
        if feed.spoken_to() >= feed.written() && !tracking_behind {
            if feed.capture_ended() {
                break;
            }
            feed.wait_for_audio(PIECE);
            continue;
        }
        feed.hear_speech();
        if let Some(heard_to) = feed.voices().map(SpeakerTracker::heard_to) {
            let lag = feed.spoken_to().saturating_sub(heard_to);
            if admission != Admission::OwnerOnly && lag > MAX_NAMING_LAG {
                feed.skip_voices_to(feed.spoken_to());
            }
            let heard_to = feed.voices().map_or(heard_to, SpeakerTracker::heard_to);
            let through = feed.spoken_to().min(heard_to + canonical_samples(PIECE));
            feed.hear_voices_through(through);
        }
        publish(shared, &feed, progress, readable);
        if feed
            .voices()
            .is_some_and(|voices| voices.timeline().is_broken())
        {
            // Nothing more can be judged or named, but the speech map still
            // decides whether anything was said.
            feed.drop_voices();
        }
    }
    feed.hear_rest();
    publish(shared, &feed, progress, readable);
}

/// Publishes what was heard. Once voices are dropped, the last timeline
/// stands.
fn publish<V: VoiceActivityDetector, K: SpeakerTracker>(
    shared: &Shared,
    feed: &TurnFeed<'_, V, K>,
    progress: &mut impl FnMut(&Snapshot),
    readable: &mut impl FnMut(),
) {
    let timeline = match feed.voices() {
        Some(voices) => Some(Arc::new(voices.timeline().clone())),
        None => shared.lock().snapshot.timeline.clone(),
    };
    let snapshot = Snapshot {
        speech: Arc::new(feed.speech().clone()),
        timeline,
    };
    progress(&snapshot);
    shared.publish(snapshot);
    readable();
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use aria_kit::audio::{Recording, RecordingReader};
    use aria_kit::speaker::{Embedder, SpeakerError, VoiceprintTracker};
    use aria_kit::voice_activity::SpeechDetector;

    use super::*;
    use crate::dictation::testing::Loud;
    use crate::speaker::testing::{OWN, Voice, tracker};

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
            let timeline = walk.snapshot().timeline.expect("a tracker runs");
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
    fn ring_announcement_precedes_a_readable_owner_snapshot() {
        use std::sync::mpsc::channel;

        use aria_kit::speaker::Speaker;

        let mut recording = RecordingReader::from_samples(&[0.5; SECOND]);
        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Voice(OWN));
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::OwnerOnly,
        )
        .unwrap();
        let (entered, waiting) = channel();
        let (resume, blocked) = channel();
        let mut announced = false;
        std::thread::scope(|scope| {
            let walk = TurnThread::spawn(
                scope,
                feed,
                &CancelToken::never(),
                move |snapshot: &Snapshot| {
                    let speaking = snapshot
                        .timeline
                        .as_ref()
                        .and_then(|timeline| timeline.speaking());
                    if !announced && matches!(speaking, Some(Speaker::Owner(_))) {
                        announced = true;
                        entered.send(()).unwrap();
                        blocked.recv().unwrap();
                    }
                },
                || {},
            )
            .unwrap();
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            let before_announcement = walk.snapshot().timeline.unwrap().speaking();
            resume.send(()).unwrap();
            let finished = walk.finish().unwrap().timeline.unwrap();
            assert!(!matches!(before_announcement, Some(Speaker::Owner(_))));
            assert!(matches!(finished.speaking(), Some(Speaker::Owner(_))));
        });
    }

    #[test]
    fn a_published_snapshot_is_readable_before_anyone_is_woken() {
        use std::sync::atomic::AtomicU64;
        use std::sync::mpsc::channel;

        let mut recording = RecordingReader::from_samples(&[0.5; SECOND]);
        let mut speech = SpeechDetector::new(Loud);
        let feed = TurnFeed::start(
            &mut speech,
            None::<&mut VoiceprintTracker<Voice>>,
            &mut recording,
            Admission::Everyone,
        )
        .unwrap();
        let (woken, wakes) = channel();
        let (resume, blocked) = channel::<()>();
        let announced = Arc::new(AtomicU64::new(0));
        let heard = Arc::clone(&announced);
        std::thread::scope(|scope| {
            let walk = TurnThread::spawn(
                scope,
                feed,
                &CancelToken::never(),
                move |snapshot: &Snapshot| heard.store(snapshot.speech.reached(), Ordering::SeqCst),
                move || {
                    let _ = woken.send(announced.load(Ordering::SeqCst));
                    let _ = blocked.recv();
                },
            )
            .unwrap();
            let _resume = resume;
            let announced = wakes.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(announced > 0);
            assert_eq!(walk.snapshot().speech.reached(), announced);
        });
    }

    #[test]
    fn the_speech_map_runs_ahead_without_a_tracker() {
        let (mut recording, mut writer) = Recording::open();
        let mut speech = SpeechDetector::new(Loud);
        let feed = TurnFeed::start(
            &mut speech,
            None::<&mut VoiceprintTracker<Voice>>,
            &mut recording,
            Admission::Everyone,
        )
        .unwrap();
        writer.push(&[0.5; SECOND]);

        std::thread::scope(|scope| {
            let walk =
                TurnThread::spawn(scope, feed, &CancelToken::never(), |_: &Snapshot| {}, || {})
                    .unwrap();
            let map = walk.speech_through(SECOND as u64).unwrap();

            assert_eq!(map.speech_before(SECOND as u64), SECOND as u64);
            assert!(walk.snapshot().timeline.is_none());
            writer.finish();
            assert!(walk.finish().is_ok());
        });
    }

    #[test]
    fn the_walk_cuts_a_finished_press_as_a_whole_pass_does() {
        let mut audio = vec![0.0; SECOND / 2];
        audio.extend([0.5; SECOND]);
        audio.extend([0.0; SECOND]);
        audio.extend([0.5; SECOND]);
        let (mut recording, mut writer) = Recording::open();
        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Voice(OWN));
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::OwnerOnly,
        )
        .unwrap();

        let timeline = std::thread::scope(|scope| {
            let walk =
                TurnThread::spawn(scope, feed, &CancelToken::never(), |_: &Snapshot| {}, || {})
                    .unwrap();
            for piece in audio.chunks(1_000) {
                writer.push(piece);
            }
            writer.finish();
            walk.finish().unwrap().timeline.unwrap()
        });

        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Voice(OWN));
        let mut recording = RecordingReader::from_samples(&audio);
        TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::OwnerOnly,
        )
        .unwrap()
        .hear_rest();
        assert!(!voices.timeline().is_broken());
        let whole = voices.timeline().admitted_audio(&audio, 0);
        assert!(!whole.is_empty() && whole.len() < audio.len());
        assert_eq!(timeline.admitted_audio(&audio, 0), whole);
    }

    #[test]
    fn dropping_the_walk_stops_it_while_capture_runs() {
        let (mut recording, mut writer) = Recording::open();
        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Voice(OWN));
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::OwnerOnly,
        )
        .unwrap();
        writer.push(&[0.5; SECOND]);

        let started = Instant::now();
        std::thread::scope(|scope| {
            let walk =
                TurnThread::spawn(scope, feed, &CancelToken::never(), |_: &Snapshot| {}, || {})
                    .unwrap();
            timeline_through(&walk, SECOND as u64);
            drop(walk);
        });

        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer);
    }

    #[test]
    fn a_walk_that_panics_while_naming_leaves_a_broken_map() {
        let mut recording = RecordingReader::from_samples(&[0.5; SECOND]);
        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Panicking);
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::Everyone,
        )
        .unwrap();

        let snapshot = std::thread::scope(|scope| {
            let walk =
                TurnThread::spawn(scope, feed, &CancelToken::never(), |_: &Snapshot| {}, || {})
                    .unwrap();
            assert!(timeline_through(&walk, SECOND as u64).is_broken());
            walk.finish_speech()
        });

        assert!(snapshot.unwrap().speech.is_broken());
    }

    #[test]
    fn a_walk_that_panics_lets_nothing_more_pass() {
        let mut recording = RecordingReader::from_samples(&[0.5; SECOND]);
        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Panicking);
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::OwnerOnly,
        )
        .unwrap();

        std::thread::scope(|scope| {
            let walk =
                TurnThread::spawn(scope, feed, &CancelToken::never(), |_: &Snapshot| {}, || {})
                    .unwrap();
            assert!(timeline_through(&walk, SECOND as u64).is_broken());
            assert!(walk.speech_through(SECOND as u64).unwrap().is_broken());
            assert!(matches!(walk.finish(), Err(TrackingError::Panicked)));
        });
    }

    #[test]
    fn naming_reports_failure_after_a_voice_was_recognized() {
        struct FailingOwner {
            heard: bool,
            panics: bool,
        }

        impl Embedder for FailingOwner {
            fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
                if self.heard {
                    assert!(!self.panics, "the embedder broke");
                    return Err(SpeakerError::Inference {
                        backend: "test",
                        source: aria_kit::backend::BackendError::Unusable(
                            "embedding failed".into(),
                        ),
                    });
                }
                self.heard = true;
                Ok(Some(OWN.to_vec()))
            }

            fn dimension(&self) -> usize {
                2
            }
        }

        for panics in [false, true] {
            let mut voices = tracker(FailingOwner {
                heard: false,
                panics,
            });
            let mut speech = SpeechDetector::new(Loud);
            let mut recording = RecordingReader::from_samples(&[0.5; 2 * SECOND]);
            let feed = TurnFeed::start(
                &mut speech,
                Some(&mut voices),
                &mut recording,
                Admission::Everyone,
            )
            .unwrap();
            let mut snapshots = Vec::new();
            std::thread::scope(|scope| {
                let walk = TurnThread::spawn(
                    scope,
                    feed,
                    &CancelToken::never(),
                    |snapshot: &Snapshot| {
                        let timeline = snapshot.timeline.as_ref().unwrap();
                        snapshots.push((
                            timeline.speaker(&(0..SECOND as u64)).is_some(),
                            timeline.is_broken(),
                        ));
                    },
                    || {},
                )
                .unwrap();
                let finished = walk.finish();
                assert!(
                    finished.is_err()
                        || finished.is_ok_and(|snapshot| snapshot.timeline.unwrap().is_broken())
                );
            });
            assert!(snapshots.iter().any(|(named, _)| *named));
            assert_eq!(snapshots.last(), Some(&(false, true)), "panics={panics}");
        }
    }

    #[test]
    fn a_walk_keeps_hearing_speech_after_naming_breaks() {
        struct Failing;

        impl Embedder for Failing {
            fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
                Err(SpeakerError::Inference {
                    backend: "test",
                    source: aria_kit::backend::BackendError::Unusable("embedding failed".into()),
                })
            }

            fn dimension(&self) -> usize {
                2
            }
        }

        let (mut recording, mut writer) = Recording::open();
        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Failing);
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::Everyone,
        )
        .unwrap();
        writer.push(&[0.5; SECOND]);

        let snapshot = std::thread::scope(|scope| {
            let walk =
                TurnThread::spawn(scope, feed, &CancelToken::never(), |_: &Snapshot| {}, || {})
                    .unwrap();
            assert!(timeline_through(&walk, SECOND as u64).is_broken());
            writer.push(&[0.5; SECOND]);
            writer.finish();
            walk.finish_speech().unwrap()
        });

        assert_eq!(snapshot.speech.reached(), 2 * SECOND as u64);
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
        let mut voices = tracker(BlockedOwner {
            entered,
            resume: blocked,
            calls: Arc::clone(&calls),
        });
        let mut speech = SpeechDetector::new(Loud);
        let mut recording = RecordingReader::from_samples(&vec![0.5; 5 * SECOND]);
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::OwnerOnly,
        )
        .unwrap();
        std::thread::scope(|scope| {
            let walk =
                TurnThread::spawn(scope, feed, &CancelToken::never(), |_: &Snapshot| {}, || {})
                    .unwrap();
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            walk.shared.abandoned.store(true, Ordering::Relaxed);
            resume.send(()).unwrap();
            drop(walk);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelling_a_wait_does_not_need_capture_to_end() {
        let (mut recording, _writer) = Recording::open();
        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Voice(OWN));
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::OwnerOnly,
        )
        .unwrap();
        let cancel = CancelToken::new();
        std::thread::scope(|scope| {
            let walk = TurnThread::spawn(scope, feed, &cancel, |_: &Snapshot| {}, || {}).unwrap();
            cancel.cancel();
            assert!(matches!(
                walk.speech_through(SECOND as u64),
                Err(TrackingError::Cancelled)
            ));
            assert!(matches!(walk.finish(), Err(TrackingError::Cancelled)));
        });
    }

    #[test]
    fn naming_skips_excessive_backlog_but_filtering_drains_it() {
        let audio = vec![0.5; 12 * SECOND];
        for admission in [Admission::Everyone, Admission::OwnerOnly] {
            let mut recording = RecordingReader::from_samples(&audio);
            let mut speech = SpeechDetector::new(Loud);
            let mut voices = tracker(Voice(OWN));
            let feed =
                TurnFeed::start(&mut speech, Some(&mut voices), &mut recording, admission).unwrap();
            let timeline = std::thread::scope(|scope| {
                TurnThread::spawn(scope, feed, &CancelToken::never(), |_: &Snapshot| {}, || {})
                    .unwrap()
                    .finish()
                    .unwrap()
                    .timeline
                    .unwrap()
            });
            assert_eq!(timeline.reached(), audio.len() as u64);
            assert_eq!(
                timeline.admitted_audio(&audio, 0).is_empty(),
                admission == Admission::Everyone
            );
        }
    }

    #[test]
    fn an_empty_finished_recording_ends_the_walk() {
        let mut recording = RecordingReader::from_samples(&[]);
        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Voice(OWN));
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::OwnerOnly,
        )
        .unwrap();
        std::thread::scope(|scope| {
            let snapshot =
                TurnThread::spawn(scope, feed, &CancelToken::never(), |_: &Snapshot| {}, || {})
                    .unwrap()
                    .finish()
                    .unwrap();
            assert_eq!(snapshot.timeline.unwrap().reached(), 0);
            assert_eq!(snapshot.speech.reached(), 0);
        });
    }
}
