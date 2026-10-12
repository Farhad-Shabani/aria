//! Playing speech while synthesis is still producing it.
//!
//! A synthesizer appends PCM from a worker thread; playback drains it as
//! it arrives, so audio starts before the utterance is finished. The buffer
//! is bounded: a writer that gets far ahead blocks until playback catches up,
//! and a writer whose reader is gone learns it on the next write.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::SynthesisError;

/// How much audio the buffer holds before the writer blocks. Synthesis
/// outruns playback, so blocking is the backpressure.
const BUFFER_SECONDS: u32 = 10;

/// The two ends of one utterance: the writer synthesis appends to, and the
/// stream playback reads from.
pub fn stream(sample_rate: u32) -> (SpeechWriter, SpeechStream) {
    let shared = Arc::new(Shared {
        state: Mutex::new(StreamState::default()),
        changed: Condvar::new(),
        sample_rate,
        capacity: sample_rate as usize * BUFFER_SECONDS as usize,
    });
    (
        SpeechWriter {
            shared: Arc::clone(&shared),
        },
        SpeechStream {
            shared,
            failed: false,
        },
    )
}

/// The writer's reader is gone or the utterance already ended; there is
/// nobody left to hear more samples.
#[derive(Debug, thiserror::Error)]
#[error("the speech stream is closed")]
pub struct StreamClosed;

/// Result of a buffered read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRead {
    /// This many samples were copied into the front of the buffer.
    Samples(usize),
    /// Nothing buffered yet, but the writer is still producing.
    Starved,
    /// No samples are left and none are coming.
    Ended,
}

struct Shared {
    state: Mutex<StreamState>,
    changed: Condvar,
    sample_rate: u32,
    capacity: usize,
}

#[derive(Default)]
struct StreamState {
    pending: VecDeque<f32>,
    reader_gone: bool,
    ended: Option<Result<(), SynthesisError>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, StreamState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The first outcome stands, so the reader learns why synthesis stopped
    /// rather than what tidied up after it.
    fn close(&self, outcome: Result<(), SynthesisError>) {
        let mut state = self.lock();
        if state.ended.is_none() {
            state.ended = Some(outcome);
        }
        drop(state);
        self.changed.notify_all();
    }
}

/// The synthesis end of an utterance. Dropping it without [`finish`] ends the
/// speech the way a vanished synthesizer does.
///
/// [`finish`]: SpeechWriter::finish
pub struct SpeechWriter {
    shared: Arc<Shared>,
}

impl SpeechWriter {
    /// Appends samples, waiting while the buffer is full. Returns an error
    /// when the reader is gone or synthesis has ended.
    pub fn write(&mut self, samples: &[f32]) -> Result<(), StreamClosed> {
        let mut remaining = samples;
        while !remaining.is_empty() {
            let mut state = self.shared.lock();
            loop {
                if state.reader_gone || state.ended.is_some() {
                    return Err(StreamClosed);
                }
                if state.pending.len() < self.shared.capacity {
                    break;
                }
                state = self
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            let available = self.shared.capacity - state.pending.len();
            let appended = available.min(remaining.len());
            state.pending.extend(&remaining[..appended]);
            remaining = &remaining[appended..];
            drop(state);
            self.shared.changed.notify_all();
        }
        Ok(())
    }

    /// Ends the utterance. The reader drains what is buffered, then sees the
    /// end.
    pub fn finish(self) {
        self.shared.close(Ok(()));
    }

    /// Ends the utterance with its cause. The reader learns `error` instead
    /// of a bare end.
    pub fn fail(self, error: SynthesisError) {
        self.shared.close(Err(error));
    }
}

impl Drop for SpeechWriter {
    fn drop(&mut self) {
        self.shared.close(Err(SynthesisError::SourceGone));
    }
}

/// An utterance, readable while synthesis is still writing it.
pub struct SpeechStream {
    shared: Arc<Shared>,
    failed: bool,
}

impl SpeechStream {
    pub fn sample_rate(&self) -> u32 {
        self.shared.sample_rate
    }

    /// Copies buffered samples into `out`, waiting until some exist or the
    /// utterance ends. `Ok(0)` is the end; a failed utterance reports its
    /// cause once and [`SynthesisError::AlreadyFailed`] after.
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize, SynthesisError> {
        if out.is_empty() {
            return Err(SynthesisError::EmptyReadBuffer);
        }
        let mut state = self.shared.lock();
        while state.pending.is_empty() && state.ended.is_none() {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let copied = Self::drain(&mut state, out);
        if copied > 0 {
            drop(state);
            self.shared.changed.notify_all();
            return Ok(copied);
        }
        let outcome = state
            .ended
            .as_mut()
            .expect("the wait only ends with samples or an outcome");
        match std::mem::replace(outcome, Ok(())) {
            Ok(()) if self.failed => Err(SynthesisError::AlreadyFailed),
            Ok(()) => Ok(0),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    /// Reads buffered samples without waiting for more audio.
    /// May wait for the shared buffer lock.
    pub fn try_read(&mut self, out: &mut [f32]) -> TryRead {
        let mut state = self.shared.lock();
        let copied = Self::drain(&mut state, out);
        if copied > 0 {
            drop(state);
            self.shared.changed.notify_all();
            return TryRead::Samples(copied);
        }
        if state.ended.is_some() {
            TryRead::Ended
        } else {
            TryRead::Starved
        }
    }

    fn drain(state: &mut StreamState, out: &mut [f32]) -> usize {
        let wanted = out.len().min(state.pending.len());
        for slot in out.iter_mut().take(wanted) {
            *slot = state
                .pending
                .pop_front()
                .expect("the drain count is bounded by the buffer length");
        }
        wanted
    }
}

impl Drop for SpeechStream {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.reader_gone = true;
        state.pending.clear();
        drop(state);
        self.shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 24_000;

    #[test]
    fn a_reader_hears_what_the_writer_wrote() {
        let (mut writer, mut speech) = stream(RATE);
        writer.write(&[0.1, 0.2]).expect("the reader is listening");
        writer.write(&[0.3]).expect("the reader is listening");
        writer.finish();

        let mut heard = Vec::new();
        let mut buffer = [0.0; 2];
        loop {
            match speech.read(&mut buffer).expect("the utterance succeeded") {
                0 => break,
                copied => heard.extend_from_slice(&buffer[..copied]),
            }
        }

        assert_eq!(heard, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn a_reader_waits_for_samples_synthesis_has_not_written_yet() {
        let (mut writer, mut speech) = stream(RATE);
        std::thread::spawn(move || {
            writer.write(&[0.5]).expect("the reader is listening");
            writer.finish();
        });

        let mut buffer = [0.0; 4];
        let copied = speech.read(&mut buffer).expect("a sample arrives");

        assert_eq!(&buffer[..copied], [0.5]);
    }

    #[test]
    fn a_full_buffer_blocks_the_writer_until_playback_drains_it() {
        let (mut writer, mut speech) = stream(1);
        let chunk = vec![0.0; BUFFER_SECONDS as usize];
        writer.write(&chunk).expect("the buffer has room");

        let blocked = std::thread::spawn(move || {
            writer.write(&[1.0]).expect("the reader drains the buffer");
            writer.finish();
        });
        let mut buffer = vec![0.0; BUFFER_SECONDS as usize];
        while !matches!(speech.read(&mut buffer), Ok(0) | Err(_)) {}

        blocked.join().expect("the writer unblocks");
    }

    #[test]
    fn one_large_write_never_overfills_the_buffer() {
        let (mut writer, mut speech) = stream(1);
        let samples = vec![0.5; BUFFER_SECONDS as usize * 2];
        let finished = std::thread::spawn(move || {
            writer
                .write(&samples)
                .expect("the reader drains the buffer");
            writer.finish();
        });

        let mut received = Vec::new();
        let mut buffer = [0.0; BUFFER_SECONDS as usize];
        loop {
            let copied = speech.read(&mut buffer).expect("the utterance succeeds");
            if copied == 0 {
                break;
            }
            assert!(copied <= BUFFER_SECONDS as usize);
            received.extend_from_slice(&buffer[..copied]);
        }

        finished.join().expect("the writer finishes");
        assert_eq!(received.len(), BUFFER_SECONDS as usize * 2);
    }

    #[test]
    fn dropping_the_reader_stops_the_writer() {
        let (mut writer, speech) = stream(RATE);
        drop(speech);

        writer
            .write(&[0.1])
            .expect_err("nobody is left to hear the samples");
    }

    #[test]
    fn dropping_the_writer_unfinished_fails_the_utterance() {
        let (writer, mut speech) = stream(RATE);
        drop(writer);

        let error = speech
            .read(&mut [0.0; 4])
            .expect_err("a vanished synthesizer is a failure");

        assert!(matches!(error, SynthesisError::SourceGone));
    }

    #[test]
    fn a_failed_utterance_stays_failed_for_every_later_read() {
        let (writer, mut speech) = stream(RATE);
        writer.fail(SynthesisError::Cancelled);
        let first = speech
            .read(&mut [0.0; 4])
            .expect_err("the first read reports the cause");

        let again = speech
            .read(&mut [0.0; 4])
            .expect_err("so does the next one");

        assert!(matches!(first, SynthesisError::Cancelled));
        assert!(matches!(again, SynthesisError::AlreadyFailed));
    }

    #[test]
    fn the_first_outcome_is_the_one_the_reader_learns() {
        let (writer, mut speech) = stream(RATE);
        let shared = Arc::clone(&writer.shared);
        writer.fail(SynthesisError::Cancelled);
        shared.close(Err(SynthesisError::WorkerPanicked { backend: "test" }));

        let error = speech
            .read(&mut [0.0; 4])
            .expect_err("the utterance failed");

        assert!(matches!(error, SynthesisError::Cancelled));
    }

    #[test]
    fn an_empty_read_buffer_is_rejected_while_samples_wait() {
        let (mut writer, mut speech) = stream(RATE);
        writer.write(&[0.1]).expect("the reader is listening");

        assert!(matches!(
            speech.read(&mut []),
            Err(SynthesisError::EmptyReadBuffer)
        ));
    }

    #[test]
    fn try_read_never_waits() {
        let (mut writer, mut speech) = stream(RATE);

        assert_eq!(speech.try_read(&mut [0.0; 4]), TryRead::Starved);

        writer.write(&[0.7]).expect("the reader is listening");
        let mut buffer = [0.0; 4];
        assert_eq!(speech.try_read(&mut buffer), TryRead::Samples(1));
        assert_eq!(buffer[0], 0.7);

        writer.finish();
        assert_eq!(speech.try_read(&mut [0.0; 4]), TryRead::Ended);
    }
}
