//! The capture source the daemon's settings select.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::daemon::config::WakeConfig;
use aria_kit::audio::{
    AudioCapture, CaptureError, InputDevice, InputDeviceId, Microphone, PreparedMicrophone,
    RecordingMode, RecordingReader, StopSignal, WakeWordMonitor, WavReplayCapture,
};
use aria_primitives::error_chain;
use aria_settings::config::ConfigError;
use aria_settings::rpc::RpcErrorCode;
use aria_voice::config::CaptureConfig;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::DictationInitializationError;
use super::tasks::Supervised;
use super::wake::WakeDetector;

pub(in crate::daemon) enum CaptureBackend {
    Microphone(Microphone),
    WavReplay(WavReplayCapture),
}

impl CaptureBackend {
    /// The next recording uses `input`. A replay has no device to choose.
    pub(in crate::daemon) fn use_input(
        &self,
        input: Option<InputDeviceId>,
    ) -> Result<(), CaptureError> {
        match self {
            Self::Microphone(capture) => capture.use_input(input),
            Self::WavReplay(_) => Ok(()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("couldn't list the microphones")]
pub(in crate::daemon) struct ListMicrophonesError(#[source] CaptureError);

impl From<&ListMicrophonesError> for RpcErrorCode {
    fn from(_error: &ListMicrophonesError) -> Self {
        Self::CaptureFailed
    }
}

/// Every connected input device. Opens none of them.
pub(in crate::daemon) fn list_microphones() -> Result<Vec<InputDevice>, ListMicrophonesError> {
    aria_kit::audio::input_devices().map_err(ListMicrophonesError)
}

/// The device `dictation.microphone` names, or `None` for the system default.
pub(in crate::daemon) fn input(microphone: &aria_settings::Microphone) -> Option<InputDeviceId> {
    match microphone {
        aria_settings::Microphone::Automatic => None,
        aria_settings::Microphone::Device(id) => Some(InputDeviceId::new(id.as_str())),
    }
}

/// Hands each change of `dictation.microphone` to the capture, which applies
/// it before the next recording.
pub(in crate::daemon) struct MicrophoneFollower {
    capture: Arc<CaptureBackend>,
    /// Marked seen where the capture took its first device.
    microphone: watch::Receiver<aria_settings::Microphone>,
}

impl MicrophoneFollower {
    pub(in crate::daemon) fn new(
        capture: Arc<CaptureBackend>,
        microphone: watch::Receiver<aria_settings::Microphone>,
    ) -> Self {
        Self {
            capture,
            microphone,
        }
    }
}

impl Supervised for MicrophoneFollower {
    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = self.microphone.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let input = input(&self.microphone.borrow_and_update());
                    if let Err(error) = self.capture.use_input(input) {
                        tracing::error!(
                            error = %error_chain(&error),
                            "the capture stopped; microphone changes no longer apply"
                        );
                        return;
                    }
                }
            }
        }
    }
}

impl AudioCapture for CaptureBackend {
    fn start(
        &self,
        mode: RecordingMode,
        stop: StopSignal,
    ) -> Result<RecordingReader, CaptureError> {
        match self {
            Self::Microphone(capture) => capture.start(mode, stop),
            Self::WavReplay(capture) => capture.start(mode, stop),
        }
    }
}

/// Capture with every model it needs loaded and the input device still
/// closed.
pub(in crate::daemon) enum PreparedCapture {
    Replay(WavReplayCapture),
    Microphone {
        microphone: Box<PreparedMicrophone>,
        wake: Option<WakeDetector>,
    },
}

impl PreparedCapture {
    pub(in crate::daemon) fn load(
        capture: CaptureConfig,
        wake: WakeConfig,
        input: Option<InputDeviceId>,
        spool_dir: &Path,
        recording_timeout: Duration,
    ) -> Result<Self, DictationInitializationError> {
        match capture {
            CaptureConfig::WavFile { path, pace } => {
                if matches!(wake, WakeConfig::Enabled { .. }) {
                    return Err(DictationInitializationError::Config(
                        ConfigError::WakeRequiresMicrophone,
                    ));
                }
                WavReplayCapture::new(spool_dir, recording_timeout, &path, pace)
                    .map(Self::Replay)
                    .map_err(DictationInitializationError::Capture)
            }
            CaptureConfig::Microphone => {
                let wake = match wake {
                    WakeConfig::Disabled => None,
                    WakeConfig::Enabled {
                        backend,
                        classifier,
                        threshold,
                    } => Some(WakeDetector::load(backend, classifier, threshold)?),
                };
                let started = Instant::now();
                let microphone = Microphone::builder()
                    .spool_dir(spool_dir)
                    .maximum_duration(recording_timeout)
                    .input(input)
                    .load()
                    .map_err(DictationInitializationError::Capture)?;
                tracing::info!(
                    load_ms = started.elapsed().as_millis(),
                    "silero speech detector loaded"
                );
                Ok(Self::Microphone {
                    microphone: Box::new(microphone),
                    wake,
                })
            }
        }
    }

    pub(in crate::daemon) fn open(
        self,
    ) -> Result<(CaptureBackend, Option<WakeWordMonitor>), CaptureError> {
        match self {
            Self::Replay(replay) => Ok((CaptureBackend::WavReplay(replay), None)),
            Self::Microphone {
                microphone,
                wake: None,
            } => Ok((CaptureBackend::Microphone((*microphone).open()?), None)),
            Self::Microphone {
                microphone,
                wake: Some(detector),
            } => {
                let (microphone, monitor) = detector.open_microphone(*microphone)?;
                Ok((CaptureBackend::Microphone(microphone), Some(monitor)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aria_kit::audio::ReplayPace;
    use aria_settings::WakeEngine;

    #[test]
    fn a_wake_word_on_a_replayed_capture_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let wake = WakeConfig::Enabled {
            backend: WakeEngine::OpenWakeWord,
            classifier: home.path().join("wake.onnx"),
            threshold: 0.5,
        };
        let replay = CaptureConfig::WavFile {
            path: aria_kit::audio::canonical_fixture(home.path()),
            pace: ReplayPace::Immediate,
        };

        let outcome =
            PreparedCapture::load(replay, wake, None, home.path(), Duration::from_secs(60));

        assert!(matches!(
            outcome,
            Err(DictationInitializationError::Config(
                ConfigError::WakeRequiresMicrophone
            ))
        ));
    }
}
