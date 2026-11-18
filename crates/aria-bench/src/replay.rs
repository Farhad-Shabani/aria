//! Replaying a WAV to a streaming decoder at the decoder's own pace: one
//! stride of audio per finished decode. Nothing waits on a wall clock, so a
//! replay costs decode time only, and each decode sees the same audio on any
//! machine under any load.

use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use aria_kit::audio::{
    CaptureError, Recording, RecordingReader, ResampleError, WavError, read_wav,
};
use aria_kit::cancel::CancelToken;
use aria_voice::stt::{
    ChunkStream, Emitted, Hypothesis, Language, RecognitionAudio, Recognizer, TranscriptionError,
};

/// Where finished decodes send the permits that release a stepped replay's
/// strides. Clones share one slot.
#[derive(Clone, Default)]
pub(crate) struct Step(Arc<Mutex<Option<mpsc::Sender<()>>>>);

impl Step {
    /// Replays `audio` one stride per finished decode, for as long as the
    /// returned `Armed` lives.
    pub(crate) fn replay(
        &self,
        audio: &Path,
        longest: Duration,
        stride: usize,
    ) -> Result<(RecordingReader, Armed<'_>), ReplayError> {
        let (permits, waiting) = mpsc::channel();
        let recording = stepped(audio, longest, stride, waiting)?;
        Ok((recording, self.arm(permits)))
    }

    /// Sends each finished decode's permit to `permits` until the guard
    /// drops.
    pub(crate) fn arm(&self, permits: mpsc::Sender<()>) -> Armed<'_> {
        *self.slot() = Some(permits);
        Armed(self)
    }

    fn release(&self) {
        // The finishing decode can outlive its recording's feeder; a send
        // into a dropped receiver is the expected end of the run.
        if let Some(permits) = self.slot().as_ref() {
            let _ = permits.send(());
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<mpsc::Sender<()>>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A replay's claim on a `Step`. Dropping it closes the channel, which ends a
/// feeder still waiting for its next stride, on every exit path.
#[must_use = "dropping it ends the replay after one stride"]
pub(crate) struct Armed<'a>(&'a Step);

impl Drop for Armed<'_> {
    fn drop(&mut self) {
        *self.0.slot() = None;
    }
}

/// A recognizer whose every finished decode releases the next stride.
pub(crate) struct Stepped<R> {
    inner: R,
    step: Step,
}

impl<R> Stepped<R> {
    pub(crate) fn new(inner: R, step: Step) -> Self {
        Self { inner, step }
    }
}

impl<R: Recognizer> Recognizer for Stepped<R> {
    fn validate_language(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        self.inner.validate_language(language, cancel)
    }

    fn recognize_with(
        &self,
        audio: RecognitionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        let heard = self.inner.recognize_with(audio, language, cancel);
        self.step.release();
        heard
    }

    fn open_stream(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
        let Some(inner) = self.inner.open_stream(language, cancel)? else {
            return Ok(None);
        };
        Ok(Some(Box::new(SteppedStream {
            inner,
            step: self.step.clone(),
        })))
    }
}

/// A native stream stepped the same way: every chunk fed is one decode.
struct SteppedStream {
    inner: Box<dyn ChunkStream>,
    step: Step,
}

impl ChunkStream for SteppedStream {
    fn chunk_samples(&self) -> usize {
        self.inner.chunk_samples()
    }

    fn feed(&mut self, chunk: &[f32]) -> Result<Emitted, TranscriptionError> {
        let heard = self.inner.feed(chunk);
        self.step.release();
        heard
    }
}

/// A recording that releases one stride of audio per finished decode: the
/// first stride is pushed up front so the opening fill returns, then each
/// permit releases the next.
///
/// The feeder finishes the recording right after the last batch, without
/// waiting for another permit — the decode covering that batch is the one
/// that would send it, and the reader's `fill_through` returns only once the
/// recording has ended. Dropping the sender ends the feeder early.
fn stepped(
    audio: &Path,
    longest: Duration,
    stride: usize,
    permits: mpsc::Receiver<()>,
) -> Result<RecordingReader, ReplayError> {
    let clip = read_wav(audio)?;
    // The WAV replay's limit, checked before resampling an oversized file.
    if clip.duration() > longest {
        return Err(WavError::TooLong {
            path: audio.to_path_buf(),
            maximum_duration: longest,
        }
        .into());
    }
    let samples = clip.into_canonical()?.into_samples();

    let (recording, mut push) = Recording::open();
    std::thread::spawn(move || {
        let mut batches = samples.chunks(stride);
        if let Some(first) = batches.next() {
            push.push(first);
        }
        for batch in batches {
            if permits.recv().is_err() {
                break;
            }
            push.push(batch);
        }
        push.finish();
    });
    Ok(recording)
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Wav(#[from] WavError),
    #[error(transparent)]
    Resample(#[from] ResampleError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_kit::audio::canonical_fixture;

    const FIXTURE_SAMPLES: u64 = 16_000;
    const STRIDE: u64 = 1_600;
    const LONGEST: Duration = Duration::from_secs(120);

    struct Silent;

    impl Recognizer for Silent {
        fn validate_language(
            &self,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn recognize_with(
            &self,
            _audio: RecognitionAudio<'_>,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            Ok(Hypothesis::new(""))
        }
    }

    #[test]
    fn each_finished_decode_releases_one_stride_until_disarmed() {
        let step = Step::default();
        let stepped = Stepped::new(Silent, step.clone());
        let decode = || {
            stepped
                .recognize_with(
                    RecognitionAudio::Samples(&[]),
                    &Language::Automatic,
                    &CancelToken::never(),
                )
                .expect("silence decodes")
        };
        let (permits, waiting) = mpsc::channel();

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _armed = step.arm(permits);
            decode();
            decode();
            panic!("a hearing that unwinds");
        }));
        decode();

        assert!(unwound.is_err());
        assert_eq!(waiting.try_recv(), Ok(()));
        assert_eq!(waiting.try_recv(), Ok(()));
        assert_eq!(waiting.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn a_stepped_replay_releases_one_stride_per_decode() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = canonical_fixture(directory.path());
        let (permits, waiting) = mpsc::channel();
        let mut recording =
            stepped(&fixture, LONGEST, STRIDE as usize, waiting).expect("the fixture replays");

        recording
            .fill_through(STRIDE)
            .expect("the first stride is pushed up front");
        assert_eq!(recording.held_to(), STRIDE);

        for release in 2..=FIXTURE_SAMPLES / STRIDE {
            permits
                .send(())
                .expect("the feeder is waiting for a permit");
            recording
                .fill_through(release * STRIDE)
                .expect("one permit releases exactly one stride");
            assert_eq!(recording.held_to(), release * STRIDE);
        }
        recording.fill().expect("the last batch ends the recording");
        assert!(
            recording.reached_end(),
            "no further permit is needed to finish"
        );
        assert_eq!(recording.held_to(), FIXTURE_SAMPLES);
    }

    #[test]
    fn closing_the_permits_finishes_the_recording() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = canonical_fixture(directory.path());
        let (permits, waiting) = mpsc::channel();
        let mut recording =
            stepped(&fixture, LONGEST, STRIDE as usize, waiting).expect("the fixture replays");

        drop(permits);

        recording
            .fill()
            .expect("the feeder finishes once the permits close");
        assert!(recording.reached_end(), "an abandoned replay cannot hang");
        assert_eq!(recording.held_to(), STRIDE);
    }
}
