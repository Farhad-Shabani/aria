//! Listening for the wake word while the microphone is idle.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::time::Duration;

use crate::voice_activity::{SPEECH_THRESHOLD, VoiceActivityDetector};
use crate::wake_word::{WakeWordDetector, WakeWordMatch};

use super::worker::{Command, IdleMonitor, IdleObservation};
use super::{MicrophoneError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeWordEvent {
    Detected,
    Failed,
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

pub(super) struct WakeMonitor<W> {
    detector: W,
    events: SyncSender<WakeWordEvent>,
    state: WakeMonitorState,
    vad_frame: Vec<i16>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WakeMonitorState {
    Armed,
    Suspended,
    Triggered,
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
        }
    }

    fn publish(&self, event: WakeWordEvent) {
        match self.events.try_send(event) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                tracing::info!("wake-word monitor stopped; event receiver closed");
            }
        }
    }

    fn fail(&mut self, error: &dyn std::fmt::Display, operation: &'static str) {
        self.state = WakeMonitorState::Failed;
        tracing::warn!(%error, operation, "wake-word monitoring failed and is suspended");
        self.publish(WakeWordEvent::Failed);
    }

    fn observe_frame<V: VoiceActivityDetector>(&mut self, vad: &mut V) -> IdleObservation {
        let probability = match vad.probability(&self.vad_frame) {
            Ok(probability) => probability,
            Err(error) => {
                self.fail(&error, "voice_activity_detection");
                return IdleObservation::Continue;
            }
        };
        let detection = match self.detector.detect(&self.vad_frame) {
            Ok(detection) => detection,
            Err(error) => {
                self.fail(&error, "wake_word_detection");
                return IdleObservation::Continue;
            }
        };
        if detection == WakeWordMatch::Detected && probability >= SPEECH_THRESHOLD {
            self.state = WakeMonitorState::Triggered;
            self.publish(WakeWordEvent::Detected);
            return IdleObservation::Detected;
        }
        IdleObservation::Continue
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

        for &sample in samples {
            self.vad_frame.push(sample);
            if self.vad_frame.len() == V::FRAME_SAMPLES {
                let observation = self.observe_frame(vad);
                self.vad_frame.clear();
                if observation == IdleObservation::Detected
                    || self.state == WakeMonitorState::Failed
                {
                    return observation;
                }
            }
        }
        IdleObservation::Continue
    }

    fn reset(&mut self) {
        self.detector.reset();
        self.vad_frame.clear();
    }

    fn suspend(&mut self) {
        self.detector.reset();
        self.vad_frame.clear();
        self.state = match self.state {
            WakeMonitorState::Armed | WakeMonitorState::Suspended | WakeMonitorState::Triggered => {
                WakeMonitorState::Suspended
            }
            WakeMonitorState::Failed => WakeMonitorState::Failed,
        };
    }

    fn resume(&mut self) {
        self.detector.reset();
        self.vad_frame.clear();
        self.state = match self.state {
            WakeMonitorState::Armed | WakeMonitorState::Suspended | WakeMonitorState::Triggered => {
                WakeMonitorState::Armed
            }
            WakeMonitorState::Failed => WakeMonitorState::Failed,
        };
    }

    fn recording_finished(&mut self) {
        self.detector.reset();
        self.vad_frame.clear();
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

    struct FixedVad(f32);

    impl VoiceActivityDetector for FixedVad {
        const FRAME_SAMPLES: usize = 4;

        fn probability(
            &mut self,
            samples: &[i16],
        ) -> Result<f32, crate::voice_activity::VoiceActivityError> {
            assert_eq!(samples.len(), Self::FRAME_SAMPLES);
            Ok(self.0)
        }
    }

    struct FailingVad;

    impl VoiceActivityDetector for FailingVad {
        const FRAME_SAMPLES: usize = 4;

        fn probability(
            &mut self,
            _samples: &[i16],
        ) -> Result<f32, crate::voice_activity::VoiceActivityError> {
            Err(crate::voice_activity::VoiceActivityError::InvalidProbability(f32::NAN))
        }
    }

    struct DetectsWakeWord {
        calls: usize,
    }

    impl WakeWordDetector for DetectsWakeWord {
        type Error = Infallible;

        fn detect(&mut self, _canonical_samples: &[i16]) -> Result<WakeWordMatch, Self::Error> {
            self.calls += 1;
            Ok(WakeWordMatch::Detected)
        }

        fn reset(&mut self) {}
    }

    #[test]
    fn recording_completion_does_not_rearm_a_triggered_monitor() {
        let (events, _detections) = sync_channel(1);
        let mut monitor = WakeMonitor::new(
            DetectsWakeWord { calls: 0 },
            events,
            FixedVad::FRAME_SAMPLES,
        );
        let mut vad = FixedVad(1.0);

        monitor.observe(&mut vad, &[0; FixedVad::FRAME_SAMPLES]);
        monitor.recording_finished();

        assert!(matches!(monitor.state, WakeMonitorState::Triggered));
    }

    #[test]
    fn idle_vad_rejects_a_wake_detection_without_speech() {
        let (events, detections) = sync_channel(1);
        let mut monitor = WakeMonitor::new(
            DetectsWakeWord { calls: 0 },
            events,
            FixedVad::FRAME_SAMPLES,
        );
        let mut vad = FixedVad(SPEECH_THRESHOLD - 0.1);

        let observation = monitor.observe(&mut vad, &[0; FixedVad::FRAME_SAMPLES]);

        assert!(matches!(observation, IdleObservation::Continue));
        assert!(matches!(monitor.state, WakeMonitorState::Armed));
        assert_eq!(monitor.detector.calls, 1);
        assert!(detections.try_recv().is_err());
    }

    #[test]
    fn idle_vad_buffers_partial_frames() {
        let (events, detections) = sync_channel(1);
        let mut monitor = WakeMonitor::new(
            DetectsWakeWord { calls: 0 },
            events,
            FixedVad::FRAME_SAMPLES,
        );
        let mut vad = FixedVad(SPEECH_THRESHOLD);

        let partial = monitor.observe(&mut vad, &[0; FixedVad::FRAME_SAMPLES - 1]);
        let complete = monitor.observe(&mut vad, &[0]);

        assert!(matches!(partial, IdleObservation::Continue));
        assert!(matches!(complete, IdleObservation::Detected));
        assert_eq!(monitor.detector.calls, 1);
        assert!(matches!(detections.try_recv(), Ok(WakeWordEvent::Detected)));
    }

    #[test]
    fn idle_vad_failure_suspends_wake_monitoring() {
        let (events, detections) = sync_channel(1);
        let mut monitor = WakeMonitor::new(
            DetectsWakeWord { calls: 0 },
            events,
            FailingVad::FRAME_SAMPLES,
        );
        let mut vad = FailingVad;

        let observation = monitor.observe(&mut vad, &[0; FailingVad::FRAME_SAMPLES]);

        assert!(matches!(observation, IdleObservation::Continue));
        assert!(matches!(monitor.state, WakeMonitorState::Failed));
        assert!(!monitor.requires_live_stream());
        assert_eq!(monitor.detector.calls, 0);
        assert!(matches!(detections.try_recv(), Ok(WakeWordEvent::Failed)));
    }
}
