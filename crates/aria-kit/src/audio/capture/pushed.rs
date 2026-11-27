//! A recording from 16 kHz mono PCM that arrives from outside, such as a
//! phone on the network, ended by the same modes as a microphone recording.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::capture::modes::{CaptureProgress, CaptureRecording};
use crate::audio::limit::LimitedWriter;
use crate::audio::recording::{self, RecordingWriter};
use crate::audio::{CaptureError, RecordingReader, RecordingSource, StopSignal, canonical_samples};
use crate::voice_activity::VoiceActivityDetector;

use super::{FeedEnd, MicrophoneError, PushedFeed, RecordingMode};

/// How often the recording checks `stop` while no audio arrives.
const STOP_POLL: Duration = Duration::from_millis(10);

/// Records one utterance at a time from samples a caller pushes in. A
/// remote device's feed goes in, and a recording of it comes out, ended
/// as its mode says.
pub struct PushedCapture<V> {
    maximum_duration: Duration,
    /// One recording runs at a time, as for a microphone, so the lock is
    /// never contended.
    vad: Arc<Mutex<V>>,
}

impl<V: VoiceActivityDetector + Send + 'static> PushedCapture<V> {
    pub fn new(maximum_duration: Duration, vad: V) -> Self {
        Self {
            maximum_duration,
            vad: Arc::new(Mutex::new(vad)),
        }
    }

    /// Records the feed's first samples, then what it brings, until `mode`
    /// or `stop` ends the recording, the feed closes, or no samples arrive
    /// for its stall. The caller waits for the first samples itself, so it
    /// handles `stop` for a source that never sends.
    pub fn start(
        &self,
        mode: RecordingMode,
        feed: PushedFeed,
        stop: StopSignal,
    ) -> Result<RecordingReader, CaptureError> {
        let PushedFeed {
            first,
            rest,
            stall,
            finished,
        } = feed;
        let (recording, writer) = recording::ends();
        let limited = LimitedWriter::new(self.maximum_duration, &writer)?;
        let session = Session {
            mode,
            feed: rest,
            stall,
            maximum_duration: self.maximum_duration,
        };
        let vad = Arc::clone(&self.vad);
        writer.begin(RecordingSource::Pushed);
        thread::Builder::new()
            .name("aria-pushed-capture".to_owned())
            .spawn(move || {
                let finished = Finished(Some(finished));
                session.run(limited, &first, &vad, writer, stop, finished);
            })
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
        limited: LimitedWriter,
        first: &[i16],
        vad: &Mutex<V>,
        writer: RecordingWriter,
        stop: StopSignal,
        finished: Finished<impl FnOnce(FeedEnd)>,
    ) {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut vad = vad.lock().unwrap_or_else(|poisoned| {
                tracing::error!(
                    "the pushed capture's speech detector lock was poisoned; recovering"
                );
                poisoned.into_inner()
            });
            let mut recording = CaptureRecording::new(limited, self.mode, &mut *vad);
            let end = self.pump(&mut recording, first, &stop)?;
            recording.finish().map(|()| end)
        }))
        .unwrap_or_else(|_panic| {
            tracing::error!("the pushed capture recovered from a panic");
            Err(MicrophoneError::WorkerPanicked.into())
        });
        // As the microphone worker does: the stop closure can own state that
        // must be released before a reader sees capture end.
        drop(stop);
        finished.run(*result.as_ref().unwrap_or(&FeedEnd::Ended));
        writer.close(result.map(|_end| ()));
    }

    fn pump<V: VoiceActivityDetector>(
        &self,
        recording: &mut CaptureRecording<V>,
        first: &[i16],
        stop: &StopSignal,
    ) -> Result<FeedEnd, CaptureError> {
        let mut room = Room(canonical_samples(self.maximum_duration));
        if room.push(recording, first)? == CaptureProgress::Complete {
            return Ok(FeedEnd::Ended);
        }
        // Audio its device held while it couldn't send arrives late, by up
        // to the gap the feed may leave. `room` still limits how much counts.
        // A feed that may stall without end has no deadline.
        let deadline = self
            .maximum_duration
            .checked_add(self.stall)
            .and_then(|limit| Instant::now().checked_add(limit));
        let mut heard = Instant::now();
        loop {
            match self.feed.recv_timeout(STOP_POLL) {
                Ok(samples) => {
                    heard = Instant::now();
                    if room.push(recording, &samples)? == CaptureProgress::Complete {
                        return Ok(FeedEnd::Ended);
                    }
                }
                Err(RecvTimeoutError::Timeout) if heard.elapsed() >= self.stall => {
                    tracing::info!(
                        stage = "recording",
                        "the pushed audio stalled; keeping what arrived"
                    );
                    return Ok(FeedEnd::Stalled);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(FeedEnd::Ended),
            }
            if stop.should_stop() {
                return self
                    .drain_tail(recording, &mut room)
                    .map(|()| FeedEnd::Ended);
            }
            // The sender's clock started first and its packets lag, so its
            // own limit can land just past this one.
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                tracing::info!(
                    stage = "recording",
                    "the pushed audio reached the recording limit; keeping what arrived"
                );
                return Ok(FeedEnd::Ended);
            }
        }
    }

    /// Keeps what already arrived when a manual stop lands, as a microphone
    /// recording keeps its buffered tail.
    fn drain_tail<V: VoiceActivityDetector>(
        &self,
        recording: &mut CaptureRecording<V>,
        room: &mut Room,
    ) -> Result<(), CaptureError> {
        let Some(limited) = recording.tail() else {
            return Ok(());
        };
        while let Ok(samples) = self.feed.try_recv() {
            let fits = room.take(samples.len());
            limited.push(&samples[..fits])?;
            if fits < samples.len() {
                break;
            }
        }
        Ok(())
    }
}

/// The samples a pushed recording has left before its limit. The sender
/// can't hear the Mac's limit in time, so audio past it ends the recording
/// with what fits instead of failing it.
struct Room(u64);

impl Room {
    /// How many of `wanted` samples fit, now spent.
    fn take(&mut self, wanted: usize) -> usize {
        let fits = wanted.min(usize::try_from(self.0).unwrap_or(usize::MAX));
        self.0 -= fits as u64;
        fits
    }

    fn push<V: VoiceActivityDetector>(
        &mut self,
        recording: &mut CaptureRecording<V>,
        samples: &[i16],
    ) -> Result<CaptureProgress, CaptureError> {
        let fits = self.take(samples.len());
        let progress = recording.push(&samples[..fits])?;
        if self.0 == 0 {
            tracing::info!(
                stage = "recording",
                "the pushed audio reached the recording limit; keeping what fits"
            );
            return Ok(CaptureProgress::Complete);
        }
        Ok(progress)
    }
}

/// Runs the caller's closure once, on every way out of a capture that
/// started, panics included.
struct Finished<F: FnOnce(FeedEnd)>(Option<F>);

impl<F: FnOnce(FeedEnd)> Finished<F> {
    fn run(mut self, end: FeedEnd) {
        if let Some(finished) = self.0.take() {
            finished(end);
        }
    }
}

impl<F: FnOnce(FeedEnd)> Drop for Finished<F> {
    fn drop(&mut self) {
        if let Some(finished) = self.0.take() {
            finished(FeedEnd::Ended);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{SyncSender, sync_channel};

    use super::*;
    use crate::audio::Stopper;
    use crate::audio::capture::{AutoStop, SilenceDuration};
    use crate::audio::testing::{Loudness, silence, speech};

    const STALL: Duration = Duration::from_millis(300);

    struct Harness {
        capture: PushedCapture<Loudness>,
        stall: Duration,
        finished: Arc<Mutex<Option<FeedEnd>>>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                capture: PushedCapture::new(Duration::from_secs(60), Loudness),
                stall: STALL,
                finished: Arc::new(Mutex::new(None)),
            }
        }

        fn with_limit(mut self, limit: Duration) -> Self {
            self.capture = PushedCapture::new(limit, Loudness);
            self
        }

        fn with_stall(mut self, stall: Duration) -> Self {
            self.stall = stall;
            self
        }

        fn start(
            &self,
            first: Vec<i16>,
            mode: RecordingMode,
            stop: StopSignal,
        ) -> (SyncSender<Vec<i16>>, Result<RecordingReader, CaptureError>) {
            let (feed, samples) = sync_channel(64);
            let finished = Arc::clone(&self.finished);
            let outcome = self.capture.start(
                mode,
                PushedFeed::new(first, samples, self.stall, move |end| {
                    *finished.lock().expect("unpoisoned") = Some(end);
                }),
                stop,
            );
            (feed, outcome)
        }
    }

    fn recorded_duration(mut recording: RecordingReader) -> Result<Duration, CaptureError> {
        recording.fill()?;
        Ok(recording.duration())
    }

    #[test]
    fn a_recording_ends_at_trailing_silence() {
        let harness = Harness::new();
        let trailing_silence = SilenceDuration::new(Duration::from_millis(500)).expect("in range");
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(500)),
            RecordingMode::StopOnSilence { trailing_silence },
            StopSignal::never(),
        );
        feed.send(silence(Duration::from_millis(500)))
            .expect("sent");

        let audio = recorded_duration(recording.expect("audio arrived")).expect("speech was heard");

        assert_eq!(audio, Duration::from_secs(1));
    }

    #[test]
    fn finished_runs_once_capture_ends() {
        let harness = Harness::new();
        let trailing_silence = SilenceDuration::new(Duration::from_millis(500)).expect("in range");
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(500)),
            RecordingMode::StopOnSilence { trailing_silence },
            StopSignal::never(),
        );
        feed.send(silence(Duration::from_millis(500)))
            .expect("sent");

        recorded_duration(recording.expect("audio arrived")).expect("speech was heard");

        assert_eq!(
            *harness.finished.lock().expect("unpoisoned"),
            Some(FeedEnd::Ended)
        );
    }

    #[test]
    fn a_stop_ends_the_recording_and_keeps_what_arrived() {
        let harness = Harness::new();
        let stopper = Stopper::new();
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(500)),
            RecordingMode::UntilStopped,
            StopSignal::from(stopper.clone()),
        );
        feed.send(speech(Duration::from_millis(500))).expect("sent");
        let recording = recording.expect("audio arrived");

        stopper.stop();
        let audio = recorded_duration(recording).expect("the recording finishes");

        assert_eq!(audio, Duration::from_secs(1));
    }

    #[test]
    fn a_pushed_recording_names_its_source() {
        let harness = Harness::new();
        let (_feed, recording) = harness.start(
            speech(Duration::from_millis(500)),
            RecordingMode::UntilStopped,
            StopSignal::never(),
        );

        assert_eq!(
            recording.expect("audio arrived").source(),
            Some(RecordingSource::Pushed)
        );
    }

    #[test]
    fn a_closed_feed_finishes_with_what_arrived() {
        let harness = Harness::new();
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(500)),
            RecordingMode::UntilStopped,
            StopSignal::never(),
        );

        drop(feed);
        let audio =
            recorded_duration(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_millis(500));
    }

    #[test]
    fn audio_that_fills_the_limit_ends_the_recording_at_once() {
        let harness = Harness::new()
            .with_limit(Duration::from_secs(1))
            .with_stall(Duration::from_secs(30));
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(500)),
            RecordingMode::UntilStopped,
            StopSignal::never(),
        );
        feed.send(speech(Duration::from_millis(500))).expect("sent");
        let filled = Instant::now();

        let audio =
            recorded_duration(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_secs(1));
        assert!(
            filled.elapsed() < Duration::from_millis(500),
            "no wait for the deadline"
        );
        drop(feed);
    }

    #[test]
    fn audio_past_the_limit_ends_the_recording_with_what_fits() {
        let harness = Harness::new().with_limit(Duration::from_secs(1));
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(700)),
            RecordingMode::UntilStopped,
            StopSignal::never(),
        );
        feed.send(speech(Duration::from_millis(700))).expect("sent");

        let audio =
            recorded_duration(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_secs(1));
    }

    #[test]
    fn a_feed_still_sending_at_the_limit_ends_with_what_arrived() {
        let harness = Harness::new().with_limit(Duration::from_millis(200));
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(10)),
            RecordingMode::UntilStopped,
            StopSignal::never(),
        );
        let sending = std::thread::spawn(move || {
            while feed.send(speech(Duration::from_millis(1))).is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        let audio =
            recorded_duration(recording.expect("audio arrived")).expect("the recording finishes");

        assert!(audio < Duration::from_millis(200), "{audio:?}");
        sending.join().expect("the sender stops once capture ends");
    }

    #[test]
    fn audio_its_device_held_past_the_limit_still_counts() {
        // A stall far past the sender's delay, so a slow machine can't end
        // the capture first.
        let harness = Harness::new()
            .with_limit(Duration::from_millis(200))
            .with_stall(Duration::from_secs(10));
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(10)),
            RecordingMode::UntilStopped,
            StopSignal::never(),
        );
        let sending = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            feed.send(speech(Duration::from_millis(50)))
        });

        let audio =
            recorded_duration(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_millis(60));
        sending
            .join()
            .expect("the sender finishes")
            .expect("the capture still reads");
    }

    #[test]
    fn a_feed_that_may_stall_forever_still_records() {
        let harness = Harness::new().with_stall(Duration::MAX);
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(500)),
            RecordingMode::UntilStopped,
            StopSignal::never(),
        );
        drop(feed);

        let audio =
            recorded_duration(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_millis(500));
    }

    #[test]
    fn a_stalled_feed_finishes_with_what_arrived() {
        let harness = Harness::new();
        let (_feed, recording) = harness.start(
            speech(Duration::from_millis(500)),
            RecordingMode::UntilStopped,
            StopSignal::never(),
        );

        let audio =
            recorded_duration(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_millis(500));
        assert_eq!(
            *harness.finished.lock().expect("unpoisoned"),
            Some(FeedEnd::Stalled)
        );
    }

    #[test]
    fn auto_stop_ends_a_quiet_recording() {
        let harness = Harness::new();
        let auto_stop = AutoStop::new(Duration::from_secs(3)).expect("in range");
        let (feed, recording) = harness.start(
            speech(Duration::from_millis(500)),
            RecordingMode::UntilQuiet { auto_stop },
            StopSignal::never(),
        );
        feed.send(silence(Duration::from_secs(3))).expect("sent");

        let audio =
            recorded_duration(recording.expect("audio arrived")).expect("the recording finishes");

        assert_eq!(audio, Duration::from_millis(3_500));
    }
}
