//! Listening for the wake word while the microphone is idle.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::time::Duration;

use crate::audio::canonical_samples;
use crate::voice_activity::{SPEECH_THRESHOLD, VoiceActivityDetector, VoiceActivityError};
use crate::wake_word::{WakeWordDetector, WakeWordMatch};

use super::worker::{Command, IdleMonitor, IdleObservation};
use super::{MicrophoneError, Result};

#[derive(Debug)]
pub enum WakeWordEvent {
    Detected,
    /// Monitoring stopped and stays stopped until the microphone reopens.
    Failed(WakeWordFailure),
}

#[derive(Debug, thiserror::Error)]
pub enum WakeWordFailure {
    #[error("voice-activity detection failed")]
    VoiceActivity(#[source] VoiceActivityError),

    #[error("wake-word detection failed")]
    Detector(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub struct WakeWordMonitor {
    control: WakeWordControl,
    events: Receiver<WakeWordEvent>,
}

impl WakeWordMonitor {
    pub(super) fn new(commands: Sender<Command>, events: Receiver<WakeWordEvent>) -> Self {
        Self {
            control: WakeWordControl { commands },
            events,
        }
    }

    pub fn control(&self) -> WakeWordControl {
        self.control.clone()
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Result<Option<WakeWordEvent>> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(MicrophoneError::WorkerGone.into()),
        }
    }

    pub fn resume(&self) -> Result<()> {
        self.control.resume()
    }
}

#[derive(Clone)]
pub struct WakeWordControl {
    commands: Sender<Command>,
}

impl WakeWordControl {
    pub fn suspend(&self) -> Result<WakeWordSuspension> {
        self.commands
            .send(Command::SuspendWakeWord)
            .map_err(|_command| MicrophoneError::WorkerGone)?;
        Ok(WakeWordSuspension {
            control: self.clone(),
        })
    }

    pub fn resume(&self) -> Result<()> {
        self.commands
            .send(Command::ResumeWakeWord)
            .map_err(|_command| MicrophoneError::WorkerGone.into())
    }
}

pub struct WakeWordSuspension {
    control: WakeWordControl,
}

impl Drop for WakeWordSuspension {
    fn drop(&mut self) {
        if let Err(error) = self.control.resume() {
            tracing::error!(%error, "wake-word monitoring could not resume");
        }
    }
}

/// How recently speech must have been heard for a detection to count. Wake
/// classifiers score a window of a second or more and fire up to a few hundred
/// milliseconds after the word ends, often on a silent frame.
const SPEECH_LOOKBACK_SAMPLES: usize = canonical_samples(Duration::from_secs(1)) as usize;

pub(super) struct WakeMonitor<W> {
    detector: W,
    events: SyncSender<WakeWordEvent>,
    state: WakeMonitorState,
    vad_frame: Vec<i16>,
    samples_since_speech: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WakeMonitorState {
    Armed,
    Suspended,
    Triggered,
    Failed,
}

enum Frame {
    Quiet,
    Detected,
    Failed,
}

impl<W: WakeWordDetector> WakeMonitor<W> {
    pub(super) fn new(
        detector: W,
        events: SyncSender<WakeWordEvent>,
        frame_samples: usize,
    ) -> Self {
        Self {
            detector,
            events,
            state: WakeMonitorState::Armed,
            vad_frame: Vec::with_capacity(frame_samples),
            samples_since_speech: None,
        }
    }

    fn forget_audio(&mut self) {
        self.detector.reset();
        self.vad_frame.clear();
        self.samples_since_speech = None;
    }

    fn track_speech(&mut self, probability: f32) {
        self.samples_since_speech = if probability >= SPEECH_THRESHOLD {
            Some(0)
        } else {
            self.samples_since_speech
                .map(|samples| samples + self.vad_frame.len())
        };
    }

    fn heard_speech_recently(&self) -> bool {
        self.samples_since_speech
            .is_some_and(|samples| samples <= SPEECH_LOOKBACK_SAMPLES)
    }

    fn publish(&self, event: WakeWordEvent) {
        match self.events.try_send(event) {
            Ok(()) | Err(TrySendError::Full(WakeWordEvent::Detected)) => {}
            Err(TrySendError::Full(WakeWordEvent::Failed(failure))) => {
                tracing::warn!(error = %failure, "wake-word monitoring failed; the event queue was full");
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::info!("wake-word monitor stopped; event receiver closed");
            }
        }
    }

    fn fail(&mut self, failure: WakeWordFailure) {
        self.state = WakeMonitorState::Failed;
        self.publish(WakeWordEvent::Failed(failure));
    }

    fn observe_frame<V: VoiceActivityDetector>(&mut self, vad: &mut V) -> Frame {
        let probability = match vad.probability(&self.vad_frame) {
            Ok(probability) => probability,
            Err(error) => {
                self.fail(WakeWordFailure::VoiceActivity(error));
                return Frame::Failed;
            }
        };
        let detection = match self.detector.detect(&self.vad_frame) {
            Ok(detection) => detection,
            Err(error) => {
                self.fail(WakeWordFailure::Detector(Box::new(error)));
                return Frame::Failed;
            }
        };
        self.track_speech(probability);
        if detection == WakeWordMatch::Detected && self.heard_speech_recently() {
            self.state = WakeMonitorState::Triggered;
            self.publish(WakeWordEvent::Detected);
            return Frame::Detected;
        }
        Frame::Quiet
    }
}

impl<W: WakeWordDetector> IdleMonitor for WakeMonitor<W> {
    fn observe<V: VoiceActivityDetector>(
        &mut self,
        vad: &mut V,
        samples: &[i16],
    ) -> IdleObservation {
        if self.state != WakeMonitorState::Armed {
            return IdleObservation::Continue;
        }

        for (index, &sample) in samples.iter().enumerate() {
            self.vad_frame.push(sample);
            if self.vad_frame.len() == V::FRAME_SAMPLES {
                let frame = self.observe_frame(vad);
                self.vad_frame.clear();
                match frame {
                    Frame::Quiet => {}
                    Frame::Detected => {
                        return IdleObservation::Detected {
                            command_start: index + 1,
                        };
                    }
                    Frame::Failed => return IdleObservation::Continue,
                }
            }
        }
        IdleObservation::Continue
    }

    fn reset(&mut self) {
        self.forget_audio();
    }

    fn suspend(&mut self) {
        self.forget_audio();
        self.state = match self.state {
            WakeMonitorState::Armed | WakeMonitorState::Suspended | WakeMonitorState::Triggered => {
                WakeMonitorState::Suspended
            }
            WakeMonitorState::Failed => WakeMonitorState::Failed,
        };
    }

    fn resume(&mut self) {
        self.forget_audio();
        self.state = match self.state {
            WakeMonitorState::Armed | WakeMonitorState::Suspended | WakeMonitorState::Triggered => {
                WakeMonitorState::Armed
            }
            WakeMonitorState::Failed => WakeMonitorState::Failed,
        };
    }

    fn recording_finished(&mut self) {
        self.forget_audio();
    }

    fn requires_live_stream(&self) -> bool {
        self.state != WakeMonitorState::Failed
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::result::Result;
    use std::sync::mpsc::sync_channel;

    use super::*;

    const FRAME: usize = 4;

    struct FixedVad(f32);

    impl VoiceActivityDetector for FixedVad {
        const FRAME_SAMPLES: usize = FRAME;

        fn probability(
            &mut self,
            samples: &[i16],
        ) -> Result<f32, crate::voice_activity::VoiceActivityError> {
            assert_eq!(samples.len(), Self::FRAME_SAMPLES);
            Ok(self.0)
        }
    }

    struct SpeechThenSilence {
        frames: usize,
    }

    impl VoiceActivityDetector for SpeechThenSilence {
        const FRAME_SAMPLES: usize = FRAME;

        fn probability(
            &mut self,
            _samples: &[i16],
        ) -> Result<f32, crate::voice_activity::VoiceActivityError> {
            self.frames += 1;
            Ok(if self.frames == 1 { 1.0 } else { 0.0 })
        }
    }

    struct DetectsOnFrame {
        frame: usize,
        calls: usize,
    }

    impl WakeWordDetector for DetectsOnFrame {
        type Error = Infallible;

        fn detect(&mut self, _canonical_samples: &[i16]) -> Result<WakeWordMatch, Self::Error> {
            self.calls += 1;
            Ok(if self.calls == self.frame {
                WakeWordMatch::Detected
            } else {
                WakeWordMatch::NoMatch
            })
        }

        fn reset(&mut self) {}
    }

    struct FailingVad;

    impl VoiceActivityDetector for FailingVad {
        const FRAME_SAMPLES: usize = FRAME;

        fn probability(
            &mut self,
            _samples: &[i16],
        ) -> Result<f32, crate::voice_activity::VoiceActivityError> {
            Err(crate::voice_activity::VoiceActivityError::InvalidProbability(f32::NAN))
        }
    }

    fn monitor(frame: usize) -> (WakeMonitor<DetectsOnFrame>, Receiver<WakeWordEvent>) {
        let (events, detections) = sync_channel(1);
        let monitor = WakeMonitor::new(DetectsOnFrame { frame, calls: 0 }, events, FRAME);
        (monitor, detections)
    }

    #[test]
    fn recording_completion_does_not_rearm_a_triggered_monitor() {
        let (mut monitor, _detections) = monitor(1);
        let mut vad = FixedVad(1.0);

        monitor.observe(&mut vad, &[0; FRAME]);
        monitor.recording_finished();

        assert!(matches!(monitor.state, WakeMonitorState::Triggered));
    }

    #[test]
    fn idle_vad_rejects_a_wake_detection_without_speech() {
        let (mut monitor, detections) = monitor(1);
        let mut vad = FixedVad(SPEECH_THRESHOLD - 0.1);

        let observation = monitor.observe(&mut vad, &[0; FRAME]);

        assert!(matches!(observation, IdleObservation::Continue));
        assert!(matches!(monitor.state, WakeMonitorState::Armed));
        assert_eq!(monitor.detector.calls, 1);
        assert!(detections.try_recv().is_err());
    }

    #[test]
    fn idle_vad_buffers_partial_frames() {
        let (mut monitor, detections) = monitor(1);
        let mut vad = FixedVad(SPEECH_THRESHOLD);

        let partial = monitor.observe(&mut vad, &[0; FRAME - 1]);
        let complete = monitor.observe(&mut vad, &[0]);

        assert!(matches!(partial, IdleObservation::Continue));
        assert!(matches!(
            complete,
            IdleObservation::Detected { command_start: 1 }
        ));
        assert_eq!(monitor.detector.calls, 1);
        assert!(matches!(detections.try_recv(), Ok(WakeWordEvent::Detected)));
    }

    #[test]
    fn idle_vad_failure_suspends_wake_monitoring() {
        let (mut monitor, detections) = monitor(1);
        let mut vad = FailingVad;

        let observation = monitor.observe(&mut vad, &[0; FRAME]);

        assert!(matches!(observation, IdleObservation::Continue));
        assert!(matches!(monitor.state, WakeMonitorState::Failed));
        assert!(!monitor.requires_live_stream());
        assert_eq!(monitor.detector.calls, 0);
        assert!(matches!(
            detections.try_recv(),
            Ok(WakeWordEvent::Failed(WakeWordFailure::VoiceActivity(_)))
        ));
    }

    #[test]
    fn a_detection_on_silence_counts_when_speech_was_just_heard() {
        let (mut monitor, detections) = monitor(3);
        let mut vad = SpeechThenSilence { frames: 0 };

        let observation = monitor.observe(&mut vad, &[0; 5 * FRAME]);

        assert!(matches!(
            observation,
            IdleObservation::Detected { command_start: 12 }
        ));
        assert!(matches!(detections.try_recv(), Ok(WakeWordEvent::Detected)));
    }

    #[test]
    fn a_detection_long_after_the_last_speech_is_rejected() {
        let frames_past_lookback = SPEECH_LOOKBACK_SAMPLES / FRAME + 2;
        let (mut monitor, detections) = monitor(frames_past_lookback);
        let mut vad = SpeechThenSilence { frames: 0 };

        let observation = monitor.observe(&mut vad, &vec![0; frames_past_lookback * FRAME]);

        assert!(matches!(observation, IdleObservation::Continue));
        assert!(detections.try_recv().is_err());
    }
}
