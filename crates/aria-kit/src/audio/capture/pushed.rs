//! A recording from 16 kHz mono PCM that arrives from outside, such as a
//! phone on the network, ended by the same modes as a microphone recording.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::capture::modes::{CaptureProgress, CaptureRecording};
use crate::audio::recording::{self, RecordingWriter};
use crate::audio::spool::WavSpool;
use crate::audio::{CaptureError, RecordingReader, StopSignal};
use crate::voice_activity::VoiceActivityDetector;

use super::{MicrophoneError, RecordingMode};

/// How often the recording checks `stop` while no audio arrives.
const STOP_POLL: Duration = Duration::from_millis(10);

/// Records one utterance at a time from samples a caller pushes in.
pub struct PushedCapture<V> {
    spool_dir: PathBuf,
    maximum_duration: Duration,
    /// One recording runs at a time, as for a microphone, so the lock is
    /// never contended.
    vad: Arc<Mutex<V>>,
}

impl<V: VoiceActivityDetector + Send + 'static> PushedCapture<V> {
    pub fn new(spool_dir: impl Into<PathBuf>, maximum_duration: Duration, vad: V) -> Self {
        Self {
            spool_dir: spool_dir.into(),
            maximum_duration,
            vad: Arc::new(Mutex::new(vad)),
        }
    }

    /// Records `first`, then the samples `feed` brings, until `mode` or
    /// `stop` ends the recording, the feed closes, or no samples arrive for
    /// `stall`. The caller waits for `first`, so a source that never sends
    /// leaves it holding `stop`. `finished` runs once, when capture ends,
    /// whether or not it started.
    pub fn start(
        &self,
        mode: RecordingMode,
        first: Vec<i16>,
        feed: Receiver<Vec<i16>>,
        stall: Duration,
        stop: StopSignal,
        finished: impl FnOnce() + Send + 'static,
    ) -> Result<RecordingReader, CaptureError> {
        let finished = Finished(Some(finished));
        let (recording, writer) = recording::ends();
        let spool = WavSpool::create(&self.spool_dir, self.maximum_duration, &writer)?;
        let session = Session {
            mode,
            feed,
            stall,
            maximum_duration: self.maximum_duration,
        };
        let vad = Arc::clone(&self.vad);
        writer.begin();
        thread::Builder::new()
            .name("aria-pushed-capture".to_owned())
            .spawn(move || session.run(spool, &first, &vad, writer, stop, finished))
            .map_err(MicrophoneError::SpawnWorker)?;
        Ok(recording)
    }
}

struct Session {
    mode: RecordingMode,
    feed: Receiver<Vec<i16>>,
    stall: Duration,
    maximum_duration: Duration,
}

impl Session {
    fn run<V: VoiceActivityDetector>(
        self,
        spool: WavSpool,
        first: &[i16],
        vad: &Mutex<V>,
        writer: RecordingWriter,
        stop: StopSignal,
        finished: Finished<impl FnOnce()>,
    ) {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut vad = vad.lock().unwrap_or_else(|poisoned| {
                tracing::error!(
                    "the pushed capture's speech detector lock was poisoned; recovering"
                );
                poisoned.into_inner()
            });
            let mut recording = CaptureRecording::new(spool, self.mode, &mut *vad);
            self.pump(&mut recording, first, &stop)?;
            recording.finish()
        }))
        .unwrap_or_else(|_panic| {
            tracing::error!("the pushed capture recovered from a panic");
            Err(MicrophoneError::WorkerPanicked.into())
        });
        // As the microphone worker does: the stop closure can own state that
        // must be released before a reader sees capture end.
        drop(stop);
        drop(finished);
        writer.close(result);
    }

    fn pump<V: VoiceActivityDetector>(
        &self,
        recording: &mut CaptureRecording<V>,
        first: &[i16],
        stop: &StopSignal,
    ) -> Result<(), CaptureError> {
        if recording.push(first)? == CaptureProgress::Complete {
            return Ok(());
        }
        let deadline = Instant::now() + self.maximum_duration;
        let mut heard = Instant::now();
        loop {
            match self.feed.recv_timeout(STOP_POLL) {
                Ok(samples) => {
                    heard = Instant::now();
                    if recording.push(&samples)? == CaptureProgress::Complete {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) if heard.elapsed() >= self.stall => {
                    tracing::info!(
                        stage = "recording",
                        "the pushed audio stalled; keeping what arrived"
                    );
                    return Ok(());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if stop.should_stop() {
                return self.drain_tail(recording);
            }
            if Instant::now() >= deadline {
                return Err(CaptureError::DeadlineExceeded(self.maximum_duration));
            }
        }
    }

    /// Keeps what already arrived when a manual stop lands, as a microphone
    /// recording keeps its buffered tail.
    fn drain_tail<V: VoiceActivityDetector>(
        &self,
        recording: &mut CaptureRecording<V>,
    ) -> Result<(), CaptureError> {
        let Some(spool) = recording.tail() else {
            return Ok(());
        };
        while let Ok(samples) = self.feed.try_recv() {
            spool.write_samples(&samples)?;
        }
        Ok(())
    }
}

/// Runs the caller's closure once, on every way out, panics included.
struct Finished<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for Finished<F> {
    fn drop(&mut self) {
        if let Some(finished) = self.0.take() {
            finished();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{SyncSender, sync_channel};

    use super::*;
    use crate::audio::capture::{AutoStop, SilenceDuration};
    use crate::audio::{Stopper, canonical_samples};
    use crate::voice_activity::VoiceActivityError;

    struct Loudness;

    impl VoiceActivityDetector for Loudness {
        const FRAME_SAMPLES: usize = 160;

        fn probability(&mut self, frame: &[i16]) -> Result<f32, VoiceActivityError> {
            Ok(if frame.iter().any(|&sample| sample != 0) {
                1.0
            } else {
                0.0
            })
        }
    }

    const STALL: Duration = Duration::from_millis(300);

    struct Harness {
        _directory: tempfile::TempDir,
        capture: PushedCapture<Loudness>,
        finished: Arc<AtomicBool>,
    }

    impl Harness {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("a temp dir");
            let capture = PushedCapture::new(directory.path(), Duration::from_secs(60), Loudness);
            Self {
                _directory: directory,
                capture,
                finished: Arc::new(AtomicBool::new(false)),
            }
        }

        /// Starts on half a second of speech, as a source's first samples.
        fn start(
            &self,
            mode: RecordingMode,
            stop: StopSignal,
        ) -> (SyncSender<Vec<i16>>, Result<RecordingReader, CaptureError>) {
            let (feed, samples) = sync_channel(64);
            let finished = Arc::clone(&self.finished);
            let first = speech(Duration::from_millis(500));
            let outcome = self
                .capture
                .start(mode, first, samples, STALL, stop, move || {
                    finished.store(true, Ordering::SeqCst);
                });
            (feed, outcome)
        }
    }

    fn speech(duration: Duration) -> Vec<i16> {
        vec![1_000; canonical_samples(duration) as usize]
    }

    fn silence(duration: Duration) -> Vec<i16> {
        vec![0; canonical_samples(duration) as usize]
    }

    fn finished_audio(mut recording: RecordingReader) -> Result<Duration, CaptureError> {
        recording.fill()?;
        Ok(recording.finished_audio()?.duration())
    }

    #[test]
    fn a_recording_ends_at_trailing_silence() {
        let harness = Harness::new();
        let trailing_silence = SilenceDuration::new(Duration::from_millis(500)).expect("in range");
        let (feed, recording) = harness.start(
            RecordingMode::StopOnSilence { trailing_silence },
            StopSignal::never(),
        );
        feed.send(silence(Duration::from_millis(500)))
            .expect("sent");

        let audio = finished_audio(recording.expect("audio arrived")).expect("speech was heard");

        assert_eq!(audio, Duration::from_secs(1));
        assert!(harness.finished.load(Ordering::SeqCst));
    }

    #[test]
    fn a_stop_ends_the_recording_and_keeps_what_arrived() {
        let harness = Harness::new();
        let stopper = Stopper::new();
        let (feed, recording) = harness.start(
            RecordingMode::UntilStopped,
            StopSignal::from(stopper.clone()),
        );
        feed.send(speech(Duration::from_millis(500))).expect("sent");
        let recording = recording.expect("audio arrived");

        stopper.stop();
        let audio = finished_audio(recording).expect("the recording finishes");

        assert_eq!(audio, Duration::from_secs(1));
    }

    #[test]
    fn a_closed_feed_finishes_with_what_arrived() {
        let harness = Harness::new();
        let (feed, recording) = harness.start(RecordingMode::UntilStopped, StopSignal::never());

        drop(feed);
        let audio =
            finished_audio(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_millis(500));
    }

    #[test]
    fn a_stalled_feed_finishes_with_what_arrived() {
        let harness = Harness::new();
        let (_feed, recording) = harness.start(RecordingMode::UntilStopped, StopSignal::never());

        let audio =
            finished_audio(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_millis(500));
    }

    #[test]
    fn auto_stop_ends_a_quiet_recording() {
        let harness = Harness::new();
        let auto_stop = AutoStop::new(Duration::from_secs(3)).expect("in range");
        let (feed, recording) =
            harness.start(RecordingMode::UntilQuiet { auto_stop }, StopSignal::never());
        feed.send(silence(Duration::from_secs(3))).expect("sent");

        let audio =
            finished_audio(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_millis(3_500));
    }
}
