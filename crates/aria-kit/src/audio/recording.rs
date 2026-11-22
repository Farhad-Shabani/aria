//! Reading a recording while capture is still writing it.
//!
//! Capture appends samples from its own thread; whoever transcribes takes them
//! as they arrive. A backend that needs the whole recording asks for it and
//! waits. Streaming transcription asks for one chunk at a time and starts
//! before capture ends.

use std::ops::Range;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;

use super::RecordingSource;
use super::canonical_duration;
#[cfg(not(target_family = "wasm"))]
use super::f32_from_pcm16;
#[cfg(not(target_family = "wasm"))]
use super::{LengthError, MicrophoneError, WavError, WavReplayError};

// The source-specific error types stay with their backends.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[cfg(not(target_family = "wasm"))]
    #[error(transparent)]
    Microphone(#[from] MicrophoneError),

    #[cfg(not(target_family = "wasm"))]
    #[error(transparent)]
    WavReplay(#[from] WavReplayError),

    #[cfg(not(target_family = "wasm"))]
    #[error(transparent)]
    Wav(#[from] WavError),

    #[cfg(not(target_family = "wasm"))]
    #[error(transparent)]
    Length(#[from] LengthError),

    #[error("no speech was detected within {0:?}")]
    NoSpeech(Duration),

    #[error("recording exceeded its safety deadline of {0:?}")]
    DeadlineExceeded(Duration),

    #[error("the capture source stopped before the recording ended")]
    SourceGone,

    #[error("the recording already failed and its cause was reported")]
    AlreadyFailed,

    #[cfg(target_family = "wasm")]
    #[error("the recording is still capturing and this target cannot wait for it")]
    StillCapturing,
}

/// The two ends of one recording, before capture begins.
pub(super) fn ends() -> (RecordingReader, RecordingWriter) {
    let recording = Arc::new(Recording::new());
    let (queue, _from) = recording.enrol();
    let reader = RecordingReader {
        recording: Arc::clone(&recording),
        samples: Vec::new(),
        offset: 0,
        queue,
        owns_outcome: true,
        outcome: None,
    };
    (reader, RecordingWriter { recording })
}

/// What one capture's writer and readers share: each reader's queue of
/// unread samples, the count written, and how capture ended. It keeps no
/// history: a sample lives only in the queues of readers yet to take it.
pub struct Recording {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// Capture is live: audio is flowing, whether or not any is kept yet.
    capturing: bool,
    /// What capture records from, once it's live.
    source: Option<RecordingSource>,
    /// One queue per live reader. Capture writes into every one of them, so a
    /// reader falling behind holds its own memory and nobody else's.
    queues: Vec<Queue>,
    /// Counts up, so an id stays unique after other queues retire.
    next_queue: u64,
    /// Samples written so far. Stream positions count from the first.
    appended: usize,
    ended: Option<Ended>,
}

/// What capture has written that one reader has not taken yet.
struct Queue {
    id: u64,
    pending: Vec<f32>,
}

impl State {
    fn enrol(&mut self) -> u64 {
        let id = self.next_queue;
        self.next_queue += 1;
        self.queues.push(Queue {
            id,
            pending: Vec::new(),
        });
        id
    }

    /// One reader's queue. A live `RecordingReader` always has one — its
    /// `Drop` is what retires it — so a missing queue is a bug rather than an
    /// ending.
    fn queue(&mut self, id: u64) -> &mut Queue {
        self.queues
            .iter_mut()
            .find(|queue| queue.id == id)
            .expect("a live reader holds its own queue")
    }
}

struct Ended {
    /// Whether capture failed. Forks read this: the owning reader moves
    /// `outcome` out, leaving `AlreadyFailed` in its place either way.
    failed: bool,
    outcome: Result<(), CaptureError>,
    #[cfg(not(target_family = "wasm"))]
    at: Instant,
}

impl Recording {
    /// A new recording's two ends, for samples pushed in as they arrive.
    /// Capture is live at once.
    pub fn open() -> (RecordingReader, RecordingWriter) {
        let (reader, writer) = ends();
        writer.begin(RecordingSource::Pushed);
        (reader, writer)
    }

    /// A new recording's two ends, for audio recorded before.
    pub(super) fn prerecorded() -> (RecordingReader, RecordingWriter) {
        let (reader, writer) = ends();
        writer.begin(RecordingSource::Prerecorded);
        (reader, writer)
    }
}
/// The write end of a [`Recording`].
///
/// Dropping this without [`RecordingWriter::finish`] ends the recording the
/// way a vanished capture thread does.
pub struct RecordingWriter {
    recording: Arc<Recording>,
}

impl RecordingWriter {
    pub fn push(&mut self, samples: &[f32]) {
        self.recording.extend(samples.iter().copied());
    }

    /// Ends the recording. A reader sees every pushed sample.
    pub fn finish(self) {
        self.close(Ok(()));
    }

    #[cfg(not(target_family = "wasm"))]
    pub(super) fn recording(&self) -> Arc<Recording> {
        Arc::clone(&self.recording)
    }

    #[cfg(not(target_family = "wasm"))]
    pub(super) fn push_pcm16(&self, samples: &[i16]) {
        self.recording.append(samples);
    }

    /// Capture is live from here on. The first sample can come later — a
    /// microphone holds back leading silence — so this mark is when the
    /// recording started.
    pub(super) fn begin(&self, source: RecordingSource) {
        self.recording.begin(source);
    }

    pub(super) fn close(&self, outcome: Result<(), CaptureError>) {
        self.recording.close(outcome);
    }
}

impl Drop for RecordingWriter {
    fn drop(&mut self) {
        self.recording.close(Err(CaptureError::SourceGone));
    }
}

impl Recording {
    fn new() -> Self {
        Self {
            state: Mutex::default(),
            changed: Condvar::new(),
        }
    }

    fn begin(&self, source: RecordingSource) {
        let mut state = self.lock();
        state.capturing = true;
        state.source = Some(source);
        drop(state);
        self.changed.notify_all();
    }

    /// Hands the readers 16-bit PCM samples as floats.
    #[cfg(not(target_family = "wasm"))]
    pub(super) fn append(&self, samples: &[i16]) {
        self.extend(samples.iter().copied().map(f32_from_pcm16));
    }

    fn extend(&self, samples: impl ExactSizeIterator<Item = f32>) {
        let mut state = self.lock();
        if state.ended.is_some() {
            return;
        }
        let count = samples.len();
        state.appended += count;
        // Written once into the first queue, then copied across from there,
        // so the iterator is walked once however many readers there are.
        if let Some((first, rest)) = state.queues.split_first_mut() {
            first.pending.extend(samples);
            let written = first.pending.len().saturating_sub(count);
            for queue in rest {
                queue.pending.extend_from_slice(&first.pending[written..]);
            }
        }
        drop(state);
        self.changed.notify_all();
    }

    /// Opens a queue for one reader, and says where it opened: the recording
    /// keeps no history, so a reader enrolled late begins where capture stands.
    ///
    /// Both under one lock. Read separately, a write landing between them
    /// would go into the new queue and be counted in the origin too, which
    /// shifts every position that reader ever reports.
    fn enrol(&self) -> (u64, u64) {
        let mut state = self.lock();
        let id = state.enrol();
        (id, state.appended as u64)
    }

    /// Closes a reader's queue, so capture stops copying into it.
    fn retire(&self, queue: u64) {
        self.lock().queues.retain(|held| held.id != queue);
    }

    /// The first outcome stands, so a reader learns why capture stopped rather
    /// than what tidied up after it.
    fn close(&self, outcome: Result<(), CaptureError>) {
        let mut state = self.lock();
        if state.ended.is_none() {
            state.ended = Some(Ended {
                failed: outcome.is_err(),
                outcome,
                #[cfg(not(target_family = "wasm"))]
                at: Instant::now(),
            });
        }
        drop(state);
        self.changed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A window onto a recording, readable while capture is still writing it.
/// One reader; [`RecordingReader::fork`] adds another.
///
/// The window holds what this reader has collected and not yet released.
/// Audio is addressed by stream position — counted from the first sample
/// capture ever appended — so a position keeps meaning what it meant after a
/// release moves the window forward.
/// Wakes a recording's waiting readers, so a reader waiting for audio
/// notices whatever else it waits for.
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
#[derive(Clone)]
pub struct RecordingWaker(Arc<Recording>);

#[cfg(all(feature = "silero", not(target_family = "wasm")))]
impl RecordingWaker {
    pub fn wake(&self) {
        // Taken so a reader between checking and sleeping cannot miss it.
        drop(self.0.lock());
        self.0.changed.notify_all();
    }
}

pub struct RecordingReader {
    recording: Arc<Recording>,
    samples: Vec<f32>,
    /// Stream position of `samples[0]`.
    offset: u64,
    /// Which queue in the recording this reader drains.
    queue: u64,
    /// Whether the failure cause is this reader's to take. Exactly one
    /// reader per recording owns it.
    owns_outcome: bool,
    outcome: Option<Ending>,
}

/// How the recording ended, for one reader. A fork learns the end and
/// whether capture failed, but not the cause.
enum Ending {
    Recorded,
    Failed,
}

impl RecordingReader {
    /// A finished recording of canonical 16 kHz mono `samples`, held in
    /// memory.
    pub fn from_samples(samples: &[f32]) -> Self {
        let (reader, mut writer) = Recording::prerecorded();
        writer.push(samples);
        writer.finish();
        reader
    }

    /// A finished recording of canonical samples in 16-bit PCM, held in
    /// memory.
    #[cfg(not(target_family = "wasm"))]
    pub fn from_pcm16(samples: &[i16]) -> Self {
        let (reader, writer) = Recording::prerecorded();
        writer.push_pcm16(samples);
        writer.finish();
        reader
    }

    /// What capture records from. `None` until capture is live.
    pub fn source(&self) -> Option<RecordingSource> {
        self.recording.lock().source.clone()
    }

    /// Waits until capture is live or the recording has ended, and reports
    /// how a recording that ended before capturing failed.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn wait_for_capture(&mut self) -> Result<(), CaptureError> {
        let mut state = self.recording.lock();
        while !state.capturing && state.ended.is_none() {
            state = self.wait_for_more(state)?;
        }
        let capturing = state.capturing;
        drop(state);
        if capturing { Ok(()) } else { self.fill() }
    }

    /// Waits until the window reaches `position`, or until capture ends short
    /// of it. Returns whether it reached `position`.
    pub fn fill_through(&mut self, position: u64) -> Result<bool, CaptureError> {
        while self.outcome.is_none() && self.held_to() < position {
            self.take()?;
        }
        self.failure()?;
        Ok(self.held_to() >= position)
    }

    #[cfg(not(target_family = "wasm"))]
    /// Takes whatever audio has arrived, without waiting for more.
    pub fn fill_available(&mut self) -> Result<(), CaptureError> {
        if self.outcome.is_some() {
            return self.failure();
        }
        let mut state = self.recording.lock();
        let ready = !state.queue(self.queue).pending.is_empty() || state.ended.is_some();
        drop(state);
        if ready {
            self.take()?;
        }
        Ok(())
    }

    /// Waits for capture to end.
    pub fn fill(&mut self) -> Result<(), CaptureError> {
        while self.outcome.is_none() {
            self.take()?;
        }
        self.failure()
    }

    /// A second reader of the same recording, with its own window and its own
    /// release floor: what one reader frees, the other still holds.
    ///
    /// The recording keeps no history, so a fork opens where capture stands
    /// rather than at the beginning — ask [`RecordingReader::held_from`] where
    /// that was.
    ///
    /// Every sample written from here on is copied into its queue until it
    /// drops, so a fork nobody reads grows without bound.
    pub fn fork(&self) -> Self {
        let (queue, from) = self.recording.enrol();
        Self {
            recording: Arc::clone(&self.recording),
            samples: Vec::new(),
            offset: from,
            queue,
            owns_outcome: false,
            outcome: None,
        }
    }

    /// A fork that starts from this reader's window, not where capture
    /// stands. It copies what this reader holds once it takes its queue.
    pub fn fork_window(&mut self) -> Self {
        // One lock for both steps, so no write lands between them.
        let mut state = self.recording.lock();
        let pending = std::mem::take(&mut state.queue(self.queue).pending);
        let queue = state.enrol();
        drop(state);
        self.samples.extend(pending);
        Self {
            recording: Arc::clone(&self.recording),
            samples: self.samples.clone(),
            offset: self.offset,
            queue,
            owns_outcome: false,
            outcome: None,
        }
    }

    /// Stream position of the first sample this reader still holds.
    pub fn held_from(&self) -> u64 {
        self.offset
    }

    /// Samples this reader has collected and not yet released.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Stream position just past the last sample held.
    pub fn held_to(&self) -> u64 {
        self.offset + self.samples.len() as u64
    }

    /// The audio in `span`, clamped to the window. A span that ran off either
    /// edge yields what survives of it, which may be nothing.
    pub fn span(&self, span: Range<u64>) -> &[f32] {
        let start = span.start.clamp(self.offset, self.held_to()) - self.offset;
        let end = span.end.clamp(self.offset, self.held_to()) - self.offset;
        &self.samples[start as usize..end.max(start) as usize]
    }

    /// Frees this reader's audio before `position`; other readers keep their
    /// own copies.
    pub fn release_before(&mut self, position: u64) {
        let freed = position.clamp(self.offset, self.held_to()) - self.offset;
        self.samples.drain(..freed as usize);
        self.offset += freed;
    }

    /// Whether this reader has seen capture end. It learns that only when it
    /// reads past the audio it holds, so it can lag.
    /// [`RecordingReader::ended_at`] does not.
    pub fn reached_end(&self) -> bool {
        self.outcome.is_some()
    }

    /// Waits up to `timeout` for capture to end, reading nothing. Returns
    /// whether it ended.
    #[cfg(not(target_family = "wasm"))]
    #[must_use]
    pub fn wait_for_end(&self, timeout: Duration) -> bool {
        let state = self.recording.lock();
        let (state, _timed_out) = self
            .recording
            .changed
            .wait_timeout_while(state, timeout, |state| state.ended.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.ended.is_some()
    }

    /// Waits up to `timeout` for audio this reader has not taken, for
    /// capture to end, or for `stopped`, reading nothing. A stop that rings
    /// the [`RecordingWaker`] ends the wait at once; one that is only
    /// polled is seen at the timeout.
    #[cfg(all(feature = "silero", not(target_family = "wasm")))]
    pub(crate) fn wait_for_audio(&self, timeout: Duration, stopped: impl Fn() -> bool) {
        let queue = self.queue;
        let state = self.recording.lock();
        let _ = self
            .recording
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.queue(queue).pending.is_empty() && state.ended.is_none() && !stopped()
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }

    /// Wakes this recording's waiting readers without writing to it.
    #[cfg(all(feature = "silero", not(target_family = "wasm")))]
    pub fn waker(&self) -> RecordingWaker {
        RecordingWaker(Arc::clone(&self.recording))
    }

    /// When capture ended. Unlike [`RecordingReader::reached_end`], it does not wait
    /// for this reader to catch up.
    #[cfg(not(target_family = "wasm"))]
    pub fn ended_at(&self) -> Option<Instant> {
        self.recording.lock().ended.as_ref().map(|ended| ended.at)
    }

    #[cfg(not(target_family = "wasm"))]
    fn wait_for_more<'state>(
        &self,
        state: MutexGuard<'state, State>,
    ) -> Result<MutexGuard<'state, State>, CaptureError> {
        Ok(self
            .recording
            .changed
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    /// Whoever feeds this recording runs on the reader's own thread, so
    /// waiting would wait forever.
    #[cfg(target_family = "wasm")]
    fn wait_for_more<'state>(
        &self,
        _state: MutexGuard<'state, State>,
    ) -> Result<MutexGuard<'state, State>, CaptureError> {
        Err(CaptureError::StillCapturing)
    }

    /// Stream position just past the last sample capture has written, whether
    /// or not this reader has taken it.
    pub fn written(&self) -> u64 {
        self.recording.lock().appended as u64
    }

    /// How much audio capture has written, whether or not it has been read.
    pub fn duration(&self) -> Duration {
        canonical_duration(self.written())
    }

    /// Waits for capture to append or end.
    fn take(&mut self) -> Result<(), CaptureError> {
        let mut state = self.recording.lock();
        // Another reader's queue filling wakes this one too, so the wait
        // re-checks its own rather than trusting the notification.
        while state.queue(self.queue).pending.is_empty() && state.ended.is_none() {
            state = self.wait_for_more(state)?;
        }
        self.samples.append(&mut state.queue(self.queue).pending);

        let Some(ended) = &mut state.ended else {
            return Ok(());
        };
        if !self.owns_outcome {
            // The cause belongs to whoever opened the recording. A fork
            // learns only that there is no more audio coming.
            let failed = ended.failed;
            drop(state);
            self.outcome = Some(match failed {
                true => Ending::Failed,
                false => Ending::Recorded,
            });
            return match failed {
                true => Err(CaptureError::AlreadyFailed),
                false => Ok(()),
            };
        }
        // The outcome moves to this reader, its only owner. Nothing reads the
        // placeholder left behind.
        let outcome = std::mem::replace(&mut ended.outcome, Err(CaptureError::AlreadyFailed));
        drop(state);
        match outcome {
            Ok(()) => {
                self.outcome = Some(Ending::Recorded);
                Ok(())
            }
            Err(error) => {
                self.outcome = Some(Ending::Failed);
                Err(error)
            }
        }
    }

    fn failure(&self) -> Result<(), CaptureError> {
        match &self.outcome {
            Some(Ending::Failed) => Err(CaptureError::AlreadyFailed),
            _live_or_recorded => Ok(()),
        }
    }
}

/// Retires this reader's queue, so capture stops copying audio nobody will
/// read into it.
impl Drop for RecordingReader {
    fn drop(&mut self) {
        self.recording.retire(self.queue);
    }
}

#[cfg(test)]
mod tests {
    use super::super::capture::MicrophoneError;
    use super::super::testing::{limited, paced, recorded};

    use super::*;

    const SAMPLES: [i16; 4] = [0, 16_384, -16_384, 32_767];
    const EXPECTED: [f32; 4] = [0.0, 0.5, -0.5, 32_767.0 / 32_768.0];

    #[test]
    fn available_reads_return_at_once_then_observe_audio_and_completion() {
        let (mut recording, mut writer) = Recording::open();
        recording.fill_available().unwrap();
        assert_eq!(recording.held_to(), 0);
        assert!(!recording.reached_end());

        writer.push(&[0.25, 0.5]);
        recording.fill_available().unwrap();
        assert_eq!(recording.samples(), [0.25, 0.5]);
        assert!(!recording.reached_end());

        writer.finish();
        recording.fill_available().unwrap();
        assert!(recording.reached_end());
    }

    #[test]
    fn available_reads_report_a_dropped_writer() {
        let (mut recording, writer) = Recording::open();
        drop(writer);
        assert!(matches!(
            recording.fill_available(),
            Err(CaptureError::SourceGone)
        ));
        assert!(matches!(
            recording.fill_available(),
            Err(CaptureError::AlreadyFailed)
        ));
    }

    #[test]
    fn a_fork_reads_the_audio_the_first_reader_already_took() {
        let (mut recording, mut capture) = Recording::open();
        let mut forked = recording.fork();
        capture.push(&[0.25, 0.5]);
        capture.finish();

        recording.fill().expect("the recording ends");
        forked.fill().expect("the fork ends too");

        assert_eq!(recording.samples(), [0.25, 0.5]);
        assert_eq!(
            forked.samples(),
            [0.25, 0.5],
            "one reader taking the audio must not empty it for the other"
        );
    }

    #[test]
    fn a_fork_opens_where_capture_stands_rather_than_at_the_beginning() {
        let (mut recording, mut capture) = Recording::open();
        capture.push(&[0.1, 0.2]);
        recording.take().expect("the first batch arrives");
        let mut forked = recording.fork();
        capture.push(&[0.3]);
        capture.finish();

        forked.fill().expect("the fork ends");

        assert_eq!(forked.held_from(), 2, "the fork opens past the spent audio");
        assert_eq!(forked.samples(), [0.3]);
        assert_eq!(
            forked.held_to(),
            3,
            "and still counts from the first sample ever"
        );
    }

    #[test]
    fn a_window_fork_keeps_audio_written_before_it() {
        let (mut recording, mut capture) = Recording::open();
        capture.push(&[0.1, 0.2]);
        let mut forked = recording.fork_window();
        capture.push(&[0.3]);
        capture.finish();

        forked.fill().expect("the fork ends");
        recording.fill().expect("the recording ends");

        assert_eq!(forked.held_from(), 0);
        assert_eq!(forked.samples(), [0.1, 0.2, 0.3]);
        assert_eq!(recording.samples(), [0.1, 0.2, 0.3]);
    }

    #[test]
    fn a_window_fork_opens_where_the_reader_still_holds() {
        let (mut recording, mut capture) = Recording::open();
        capture.push(&[0.1, 0.2]);
        recording.take().expect("the first batch arrives");
        recording.release_before(1);
        let mut forked = recording.fork_window();
        capture.push(&[0.3]);
        capture.finish();

        forked.fill().expect("the fork ends");

        assert_eq!(forked.held_from(), 1);
        assert_eq!(forked.samples(), [0.2, 0.3]);
    }

    #[test]
    fn a_fork_counts_from_where_its_own_audio_starts() {
        // Enrolling the queue and reading the origin have to happen under one
        // lock. Split them and a write landing between lands in the queue and
        // in the origin both, shifting every position the fork reports — so
        // this forks against a writer that is already running.
        let (recording, mut capture) = Recording::open();
        let writing = std::thread::spawn(move || {
            for sample in 0..4_000 {
                capture.push(&[sample as f32]);
            }
            capture.finish();
        });

        for _ in 0..400 {
            let mut forked = recording.fork();
            if forked.take().is_err() {
                break;
            }
            // Each sample carries its own stream position as its value.
            if let Some(&first) = forked.samples().first() {
                assert_eq!(
                    first as u64,
                    forked.held_from(),
                    "a fork's first sample must sit where the fork says it does"
                );
            }
        }
        writing.join().expect("capture finishes");
    }

    #[test]
    fn releasing_on_one_reader_leaves_the_other_window_alone() {
        let (mut recording, mut capture) = Recording::open();
        let mut forked = recording.fork();
        capture.push(&[0.1, 0.2, 0.3]);
        capture.finish();
        recording.fill().expect("the recording ends");
        forked.fill().expect("the fork ends");

        recording.release_before(2);

        assert_eq!(
            recording.samples(),
            [0.3],
            "the reader freed its own prefix"
        );
        assert_eq!(
            forked.samples(),
            [0.1, 0.2, 0.3],
            "a track walking behind must still be able to read it"
        );
    }

    #[test]
    fn a_fork_leaves_the_failure_cause_to_the_reader_that_opened_the_recording() {
        let (mut recording, writer) = ends();
        let mut forked = recording.fork();
        writer.close(Err(CaptureError::SourceGone));

        forked.fill().expect_err("the fork learns capture failed");

        assert!(matches!(recording.fill(), Err(CaptureError::SourceGone)));
    }

    #[test]
    fn a_failed_capture_ends_a_fork_with_the_same_verdict() {
        let (recording, writer) = ends();
        let mut forked = recording.fork();
        writer.close(Err(CaptureError::SourceGone));

        let failure = forked.fill().expect_err("a failed capture fails its fork");

        assert!(matches!(failure, CaptureError::AlreadyFailed));
    }

    #[test]
    fn a_dropped_fork_stops_costing_capture_anything() {
        let (reader, mut capture) = Recording::open();
        let forked = reader.fork();
        assert_eq!(reader.recording.lock().queues.len(), 2);

        drop(forked);
        capture.push(&[0.1]);

        let state = reader.recording.lock();
        assert_eq!(
            state.queues.len(),
            1,
            "capture still copies into a dead queue"
        );
        assert_eq!(state.queues[0].pending.len(), 1);
    }

    #[test]
    fn two_readers_waiting_are_both_woken_by_one_write() {
        let (mut recording, mut capture) = Recording::open();
        let mut forked = recording.fork();
        let waiting = std::thread::spawn(move || {
            forked.fill_through(1).expect("the fork is woken");
            forked.held_to()
        });

        capture.push(&[0.5]);
        recording.fill_through(1).expect("the reader is woken");

        assert_eq!(recording.held_to(), 1);
        assert_eq!(waiting.join().expect("the fork thread finishes"), 1);
    }

    #[test]
    fn a_reader_waits_for_samples_capture_has_not_written_yet() {
        let (writer, mut limited, mut recording) = limited();
        std::thread::spawn(move || {
            for sample in SAMPLES {
                limited.push(&[sample]).expect("samples are writable");
            }
            writer.close(limited.finish().map_err(CaptureError::from));
        });

        recording.fill().expect("the recording ends");

        assert_eq!(recording.samples(), EXPECTED);
    }

    #[test]
    fn a_reader_can_take_a_prefix_before_the_recording_ends() {
        let (mut recording, pace) = paced(vec![SAMPLES[..2].to_vec(), SAMPLES[2..].to_vec()]);

        recording
            .fill_through(2)
            .expect("two samples are available");

        assert!(!recording.reached_end());
        assert_eq!(recording.samples(), &EXPECTED[..2]);

        pace.advance();
        recording.fill().expect("the rest is available");

        assert_eq!(recording.samples(), EXPECTED);
    }

    #[test]
    fn asking_past_the_end_returns_what_was_recorded() {
        let mut recording = recorded(&SAMPLES);

        assert!(recording.fill_through(4).expect("the recording succeeded"));
        assert!(
            !recording
                .fill_through(5)
                .expect("a short recording is not a failure")
        );
        assert!(
            !recording
                .fill_through(SAMPLES.len() as u64 * 4)
                .expect("a short recording is not a failure")
        );

        assert!(recording.reached_end());
        assert_eq!(recording.samples(), EXPECTED);
    }

    #[test]
    fn waiting_for_the_end_times_out_while_capture_runs() {
        let (recording, writer) = Recording::open();

        assert!(!recording.wait_for_end(Duration::from_millis(10)));
        writer.finish();
        assert!(recording.wait_for_end(Duration::from_millis(10)));
    }

    #[test]
    fn a_recording_from_samples_holds_them_and_has_ended() {
        let mut recording = RecordingReader::from_samples(&[0.25, -0.5]);

        recording.fill().expect("the recording already ended");

        assert_eq!(recording.samples(), [0.25, -0.5]);
        assert!(recording.reached_end());
    }

    #[test]
    fn a_pcm16_recording_keeps_every_sample_exactly() {
        let mut recording = RecordingReader::from_pcm16(&SAMPLES);

        recording.fill().expect("the recording already ended");

        assert_eq!(recording.samples(), EXPECTED);
    }

    /// Reading on past a failure must not look like an empty recording that
    /// simply had nothing in it.
    #[test]
    fn a_failed_recording_stays_failed_for_every_later_read() {
        let (mut recording, writer) = ends();
        writer.close(Err(CaptureError::Microphone(
            MicrophoneError::NoInputDevice,
        )));
        recording
            .fill()
            .expect_err("the first read reports the cause");

        let again = recording.fill_through(1).expect_err("so does the next one");

        assert!(matches!(again, CaptureError::AlreadyFailed));
    }

    /// Silence appends no samples, so capture being live must not depend on
    /// any sample arriving.
    #[test]
    fn a_silent_capture_still_counts_as_started() {
        let (mut recording, writer) = ends();
        writer.begin(RecordingSource::Pushed);

        recording
            .wait_for_capture()
            .expect("a live capture has started");

        assert!(recording.samples().is_empty());
    }

    #[test]
    fn a_capture_that_dies_before_starting_reports_its_cause() {
        let (mut recording, writer) = ends();
        writer.close(Err(CaptureError::Microphone(
            MicrophoneError::NoInputDevice,
        )));

        let error = recording
            .wait_for_capture()
            .expect_err("the recording never started");

        assert!(matches!(
            error,
            CaptureError::Microphone(MicrophoneError::NoInputDevice)
        ));
    }

    #[test]
    fn dropping_the_capture_end_ends_the_recording() {
        let (mut recording, writer) = ends();
        drop(writer);

        let error = recording
            .fill()
            .expect_err("a reader must not wait on a producer that is gone");

        assert!(matches!(error, CaptureError::SourceGone));
    }

    #[test]
    fn the_first_outcome_is_the_one_the_reader_learns() {
        let (mut recording, writer) = ends();
        writer.close(Err(CaptureError::Microphone(
            MicrophoneError::NoInputDevice,
        )));
        drop(writer);

        let error = recording.fill().expect_err("the recording failed to start");

        assert!(matches!(
            error,
            CaptureError::Microphone(MicrophoneError::NoInputDevice)
        ));
    }
}
