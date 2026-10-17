//! The capture source the daemon's configuration selects.

use std::path::Path;
use std::time::Duration;

use crate::audio::{
    AudioCapture, CaptureError, Microphone, PreparedMicrophone, RecordingMode, RecordingReader,
    StopSignal, WakeWordMonitor, WavReplayCapture,
};
use crate::config::{CaptureConfig, ConfigError, WakeConfig};

use super::wake::WakeDetector;

#[derive(Debug, thiserror::Error)]
pub(in crate::daemon) enum CaptureLoadError {
    #[error("failed to load dictation configuration")]
    Config(#[source] ConfigError),

    #[error("failed to initialize audio capture")]
    Capture(#[source] CaptureError),

    #[error("failed to configure wake-word detection")]
    WakeConfiguration(#[source] aria_wake::ConfigError),

    #[error("failed to load the wake-word model")]
    WakeModel(#[source] aria_wake::Error),
}

pub(in crate::daemon) enum CaptureBackend {
    Microphone(Microphone),
    WavReplay(WavReplayCapture),
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
        spool_dir: &Path,
        recording_timeout: Duration,
    ) -> Result<Self, CaptureLoadError> {
        match capture {
            CaptureConfig::WavFile { path, pace } => {
                if matches!(wake, WakeConfig::Enabled { .. }) {
                    return Err(CaptureLoadError::Config(
                        ConfigError::WakeRequiresMicrophone,
                    ));
                }
                WavReplayCapture::new(spool_dir, recording_timeout, &path, pace)
                    .map(Self::Replay)
                    .map_err(CaptureLoadError::Capture)
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
                let microphone = Microphone::builder()
                    .spool_dir(spool_dir)
                    .maximum_duration(recording_timeout)
                    .load()
                    .map_err(CaptureLoadError::Capture)?;
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

    use crate::audio::ReplayPace;
    use crate::config::WakeBackend;

    #[test]
    fn a_wake_word_on_a_replayed_capture_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let wake = WakeConfig::Enabled {
            backend: WakeBackend::OpenWakeWord,
            classifier: home.path().join("wake.onnx"),
            threshold: 0.5,
        };
        let replay = CaptureConfig::WavFile {
            path: crate::audio::canonical_fixture(home.path()),
            pace: ReplayPace::Immediate,
        };

        let outcome = PreparedCapture::load(replay, wake, home.path(), Duration::from_secs(60));

        assert!(matches!(
            outcome,
            Err(CaptureLoadError::Config(
                ConfigError::WakeRequiresMicrophone
            ))
        ));
    }
}
