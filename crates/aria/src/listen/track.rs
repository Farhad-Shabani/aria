//! The voice track's own thread.
//!
//! The track walks its own fork of the recording on its own clock. Neither
//! thread waits on the other's reads, and neither pins the other's audio.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::{canonical_duration, canonical_samples};
use aria_kit::segmentation::Segmenter;
use aria_kit::speaker::Embedder;

use crate::audio::RecordingReader;
use crate::speaker::{Merged, PyannoteVoices, VoiceTrack};

/// Maximum audio backlog retained between chunks. Excess audio is skipped
/// to bound memory use and the work remaining after capture ends.
const MAX_LAG_SAMPLES: u64 = canonical_samples(Duration::from_secs(30));

/// Tail-processing budget after capture ends, checked between chunks.
/// One slow inference call can exceed it.
const DRAIN_LIMIT: Duration = Duration::from_secs(3);

/// A voice track running beside the transcriber.
pub(crate) struct VoiceThread {
    track: Arc<Mutex<VoiceTrack>>,
    merges: Receiver<Merged>,
    thread: Option<JoinHandle<()>>,
}

impl VoiceThread {
    /// Starts the track over `audio`, which must be its own fork of the
    /// recording: the walk releases what it has read.
    pub(crate) fn spawn(track: VoiceTrack, audio: RecordingReader) -> std::io::Result<Self> {
        let track = Arc::new(Mutex::new(track));
        let walking = Arc::clone(&track);
        let (sender, merges) = channel();
        let thread = std::thread::Builder::new()
            .name("aria-listen-voices".into())
            .spawn(move || walk(&walking, audio, &sender))?;
        Ok(Self {
            track,
            merges,
            thread: Some(thread),
        })
    }

    /// Voices the track has joined since the last call.
    pub(crate) fn merged(&self) -> Vec<Merged> {
        self.merges.try_iter().collect()
    }

    /// Locks the current track for attribution. The worker releases this lock
    /// between chunks and before waiting for more audio.
    pub(crate) fn attribution(&self) -> MutexGuard<'_, VoiceTrack> {
        lock(&self.track)
    }

    /// Waits for the walk to end, and reports what the track joined on the
    /// way.
    ///
    /// Capture must have ended first: the walk's own read is what waits on
    /// it, so joining a thread whose capture is still live waits forever.
    pub(crate) fn finish(&mut self) -> Vec<Merged> {
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("the voice track thread panicked; the tail stays unnamed");
        }
        self.merged()
    }
}

/// Hears chunk after chunk until the audio runs out or the drain limit cuts
/// the tail.
fn walk<S: Segmenter, E: Embedder>(
    track: &Mutex<PyannoteVoices<S, E>>,
    mut audio: RecordingReader,
    merges: &Sender<Merged>,
) {
    let chunk_samples = lock(track).shape().samples as u64;
    // The fork opened where capture stood, so nothing before that is
    // readable; starting at zero would warn its way up to here.
    lock(track).skip_to(audio.held_from());
    let mut draining: Option<Instant> = None;
    loop {
        // The lock is never held across `fill_through`: a rewrite must wait on
        // a chunk, never on capture.
        let heard = lock(track).hear(&audio);
        let Some(chunk) = heard else {
            if audio.reached_end() {
                return;
            }
            let want = lock(track).retained_from() + chunk_samples;
            // A capture failure is capture ending abnormally, and its own
            // exit classification names the cause. Here it just means the
            // audio stopped coming.
            if audio.fill_through(want).is_err() {
                return;
            }
            continue;
        };

        let mut voices = lock(track);
        for merged in voices.group(&chunk).merges {
            // A closed channel means the transcriber is gone, and the walk
            // is about to run out of audio anyway.
            let _ = merges.send(merged);
        }
        // Capture's total, not this window's edge: audio still queued for the
        // fork is backlog too.
        let written = audio.written();
        let behind = written.saturating_sub(voices.retained_from());
        if behind > MAX_LAG_SAMPLES {
            tracing::warn!(
                behind_seconds = canonical_duration(behind).as_secs(),
                skipped_seconds = canonical_duration(behind - MAX_LAG_SAMPLES).as_secs(),
                "the voice track cannot keep up; this stretch will have no speaker"
            );
            voices.skip_to(written - MAX_LAG_SAMPLES);
        }
        tracing::debug!(
            at_seconds = canonical_duration(chunk.at).as_secs(),
            behind_seconds = canonical_duration(behind).as_secs(),
            "voice track heard a chunk"
        );
        let read_through = voices.retained_from();
        drop(voices);
        audio.release_before(read_through);

        // The recording's end, not this reader's: `reached_end()` only turns true
        // on a read past the audio the reader holds, which a reader with a
        // backlog does not make — so it would start the clock after the wait
        // it is meant to bound.
        if audio.ended_at().is_some()
            && draining.get_or_insert_with(Instant::now).elapsed() > DRAIN_LIMIT
        {
            tracing::warn!(
                behind_seconds = canonical_duration(behind).as_secs(),
                "the voice track ran out of time; the tail stays unnamed"
            );
            return;
        }
    }
}

fn lock<T>(held: &Mutex<T>) -> MutexGuard<'_, T> {
    held.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use aria_kit::segmentation::{Activity, ChunkShape, SegmentationError};
    use aria_kit::speaker::SpeakerError;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::audio::{Recording, RecordingWriter};
    use crate::speaker::testing::{alone, track_of};
    use crate::speaker::{Diarizer, Grouping};

    use super::*;

    /// Nobody reads the primary recording; the track still walks.
    #[test]
    fn the_track_walks_while_nobody_reads_the_recording() {
        let (recording, mut capture) = Recording::open();
        let audio = recording.fork();
        let chunks = 4;
        let voices = Arc::new(Mutex::new(track_of(
            HOP_SAMPLES,
            vec![alone(0); chunks],
            vec![[1.0, 0.0]; chunks],
        )));
        let walking = Arc::clone(&voices);
        let (sender, _merges) = channel();

        let walker = std::thread::spawn(move || walk(&walking, audio, &sender));
        capture.push(&vec![0.1; HOP_SAMPLES * chunks]);
        capture.finish();
        walker.join().expect("the walk finishes");

        assert_eq!(
            lock(&voices).retained_from(),
            (HOP_SAMPLES * chunks) as u64,
            "the track should reach the end of a recording nobody else read"
        );
    }

    #[test]
    fn a_track_that_falls_behind_gives_up_on_the_audio_it_skipped() {
        let (recording, mut capture) = Recording::open();
        let audio = recording.fork();
        // Twice the cap, and a script long enough to hear every hop of it.
        let over_cap = MAX_LAG_SAMPLES as usize * 2;
        let hops = over_cap / HOP_SAMPLES;
        capture.push(&vec![0.1; over_cap]);
        capture.finish();
        let voices = Arc::new(Mutex::new(track_of(
            HOP_SAMPLES,
            vec![alone(0); hops],
            vec![[1.0, 0.0]; hops],
        )));
        let (sender, _merges) = channel();

        walk(&voices, audio, &sender);

        let heard = lock(&voices).heard_so_far();
        let voiceprints = heard.first().map_or(0, |voice| voice.voiceprints);
        assert!(
            voiceprints < hops,
            "a track past the cap should skip the backlog, not hear all {hops} hops of it"
        );
        assert!(
            voiceprints >= hops / 4,
            "and should still hear the audio inside the cap, not give up on the lot"
        );
    }

    /// A net too slow to walk a backlog inside the drain limit, so the tail
    /// has to give up rather than hold the stop.
    struct Crawling;

    impl Segmenter for Crawling {
        fn shape(&self) -> ChunkShape {
            ChunkShape {
                samples: HOP_SAMPLES,
                frame_shift: 320,
                frame_size: 320,
                speakers: 3,
            }
        }

        fn segment(&mut self, _chunk: &[f32]) -> Result<Vec<Activity>, SegmentationError> {
            std::thread::sleep(Duration::from_millis(1_000));
            Ok(Vec::new())
        }
    }

    /// Counts its chunks. On the first, it pushes `late` samples into capture,
    /// so they queue behind the window the walk is busy with.
    struct Interrupting {
        calls: Arc<AtomicUsize>,
        capture: Option<RecordingWriter>,
        late: usize,
    }

    impl Segmenter for Interrupting {
        fn shape(&self) -> ChunkShape {
            Crawling.shape()
        }

        fn segment(&mut self, _chunk: &[f32]) -> Result<Vec<Activity>, SegmentationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(mut capture) = self.capture.take() {
                capture.push(&vec![0.1; self.late]);
                capture.finish();
            }
            Ok(Vec::new())
        }
    }

    /// Hears nobody, so the walk's cost is the segmenter's alone.
    struct Deaf;

    impl Embedder for Deaf {
        fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
            Ok(None)
        }

        fn dimension(&self) -> usize {
            2
        }
    }

    #[test]
    fn a_tail_too_slow_to_walk_gives_up_rather_than_holding_the_stop() {
        let (recording, mut capture) = Recording::open();
        let audio = recording.fork();
        // Pushed while capture is live, so the walk takes the lot into its
        // window in one read and then grinds without reading again. Capture
        // ends after that, which the walk can only learn from the recording —
        // its own `reached_end()` went stale at the read before.
        capture.push(&vec![0.1; HOP_SAMPLES * 400]);
        let writing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            capture.finish();
        });
        let voices = Arc::new(Mutex::new(PyannoteVoices::new(
            Crawling,
            Diarizer::new(Deaf, Vec::new()),
            Grouping::default(),
        )));
        let (sender, _merges) = channel();

        let started = Instant::now();
        walk(&voices, audio, &sender);

        writing.join().expect("capture finishes");
        // The cap leaves 30 s of backlog; at a second a chunk that is fifteen
        // seconds of walking, and the limit is the only thing that cuts it.
        assert!(
            started.elapsed() < DRAIN_LIMIT * 2,
            "the tail should give up near the drain limit, not walk the whole cap"
        );
    }

    #[test]
    fn audio_queued_behind_the_window_counts_as_backlog() {
        let (recording, mut capture) = Recording::open();
        let audio = recording.fork();
        // The walk takes this whole cap into its window in one read.
        capture.push(&vec![0.1; MAX_LAG_SAMPLES as usize]);
        let calls = Arc::new(AtomicUsize::new(0));
        let voices = Arc::new(Mutex::new(PyannoteVoices::new(
            Interrupting {
                calls: Arc::clone(&calls),
                capture: Some(capture),
                late: MAX_LAG_SAMPLES as usize * 2,
            },
            Diarizer::new(Deaf, Vec::new()),
            Grouping::default(),
        )));
        let (sender, _merges) = channel();

        walk(&voices, audio, &sender);

        let window_chunks = MAX_LAG_SAMPLES as usize / HOP_SAMPLES;
        assert!(
            calls.load(Ordering::SeqCst) < window_chunks + window_chunks / 3,
            "the late audio should skip the window the walk was in, not wait behind it"
        );
    }

    const HOP_SAMPLES: usize = 32_000;
}
