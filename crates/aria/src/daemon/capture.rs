//! The capture source the daemon's configuration selects.

use std::path::Path;
use std::time::Duration;

use aria_kit::wake_word::WakeWordDetector;

use crate::audio::{
    AudioCapture, CaptureError, Microphone, MicrophoneBuilder, RecordingMode, RecordingReader,
    StopSignal, WakeWordMonitor, WavReplayCapture,
};
use crate::config::CaptureConfig;

pub(in crate::daemon) enum CaptureBackend {
    Microphone(Microphone),
    WavReplay(WavReplayCapture),
}

impl CaptureBackend {
    pub(in crate::daemon) fn new(
        config: CaptureConfig,
        spool_dir: &Path,
        recording_timeout: Duration,
    ) -> Result<Self, CaptureError> {
        match config {
            CaptureConfig::Microphone => Ok(Self::Microphone(
                microphone(spool_dir, recording_timeout).open()?,
            )),
            CaptureConfig::WavFile { path, pace } => Ok(Self::WavReplay(WavReplayCapture::new(
                spool_dir,
                recording_timeout,
                &path,
                pace,
            )?)),
        }
    }

    pub(in crate::daemon) fn with_wake_word<W>(
        detector: W,
        spool_dir: &Path,
        recording_timeout: Duration,
    ) -> Result<(Self, WakeWordMonitor), CaptureError>
    where
        W: WakeWordDetector + Send + 'static,
    {
        let (microphone, monitor) =
            microphone(spool_dir, recording_timeout).open_with_wake_word(detector)?;
        Ok((Self::Microphone(microphone), monitor))
    }
}

fn microphone(spool_dir: &Path, recording_timeout: Duration) -> MicrophoneBuilder {
    Microphone::builder()
        .spool_dir(spool_dir)
        .maximum_duration(recording_timeout)
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
