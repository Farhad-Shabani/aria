//! Audio sources behind [`AudioCapture`]: the default microphone and WAV replay.

#[cfg(feature = "microphone")]
mod microphone;
mod wav_replay;

use std::time::Duration;

use crate::audio::{CaptureError, RecordingReader, ResampleError, StopSignal};
use crate::voice_activity::VoiceActivityError;

#[cfg(feature = "microphone")]
pub use microphone::{
    Microphone, MicrophoneBuilder, PreparedMicrophone, WakeWordControl, WakeWordEvent,
    WakeWordFailure, WakeWordMonitor, WakeWordSuspension, check_default_input_device,
};
pub use wav_replay::{ReplayPace, WavReplayCapture, WavReplayError};

const MIN_SILENCE_DURATION: Duration = Duration::from_millis(100);
const MAX_SILENCE_DURATION: Duration = Duration::from_secs(10);
const DEFAULT_SILENCE_DURATION: Duration = Duration::from_millis(1_500);

/// A source of canonical 16 kHz mono audio. Each implementation bounds how
/// long a recording runs.
pub trait AudioCapture: Send + Sync + 'static {
    /// Starts capturing and returns a reader for the new recording. Returns
    /// once capture is live or the recording has ended, and fails if it
    /// ended in error.
    ///
    /// A live source writes from its own thread and polls `stop` there, so
    /// the caller can transcribe while audio arrives.
    fn start(&self, mode: RecordingMode, stop: StopSignal)
    -> Result<RecordingReader, CaptureError>;
}

/// One backend serves every operation that records, so the operations share
/// it rather than each opening a microphone of their own.
impl<C: AudioCapture> AudioCapture for std::sync::Arc<C> {
    fn start(
        &self,
        mode: RecordingMode,
        stop: StopSignal,
    ) -> Result<RecordingReader, CaptureError> {
        (**self).start(mode, stop)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingMode {
    StopOnSilence { trailing_silence: SilenceDuration },
    UntilStopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SilenceDuration(Duration);

impl SilenceDuration {
    pub fn new(duration: Duration) -> Result<Self, SilenceDurationError> {
        if !(MIN_SILENCE_DURATION..=MAX_SILENCE_DURATION).contains(&duration) {
            return Err(SilenceDurationError::OutOfRange {
                actual: duration,
                minimum: MIN_SILENCE_DURATION,
                maximum: MAX_SILENCE_DURATION,
            });
        }
        if !duration.subsec_nanos().is_multiple_of(1_000_000) {
            return Err(SilenceDurationError::SubmillisecondPrecision { actual: duration });
        }
        Ok(Self(duration))
    }

    pub fn get(self) -> Duration {
        self.0
    }
}

impl Default for SilenceDuration {
    fn default() -> Self {
        Self(DEFAULT_SILENCE_DURATION)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SilenceDurationError {
    #[error("silence duration must be between {minimum:?} and {maximum:?}; got {actual:?}")]
    OutOfRange {
        actual: Duration,
        minimum: Duration,
        maximum: Duration,
    },

    #[error("silence duration must use whole-millisecond precision; got {actual:?}")]
    SubmillisecondPrecision { actual: Duration },
}

#[derive(Debug, thiserror::Error)]
pub enum MicrophoneError {
    #[error("failed to prepare the recording spool directory")]
    SpoolDirectory(#[source] crate::primitives::private_fs::PrivateDirectoryError),

    #[error("no default microphone is available")]
    NoInputDevice,

    #[error("failed to read the default microphone configuration")]
    DefaultConfiguration(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("failed to read the default microphone description")]
    DeviceDescription(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("microphone input configuration has no channels")]
    NoInputChannels,

    #[error("microphone input configuration has a zero sample rate")]
    ZeroInputSampleRate,

    #[error(
        "microphone input sample rate {actual} Hz exceeds the supported maximum of {maximum} Hz"
    )]
    InputSampleRateTooHigh { actual: u32, maximum: u32 },

    #[error("microphone sample format {0} is not supported")]
    UnsupportedSampleFormat(String),

    #[error("failed to build the microphone input stream")]
    BuildStream(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("failed to start the microphone input stream")]
    StartStream(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("the microphone input stream failed")]
    Stream(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("the microphone input stream ended unexpectedly")]
    StreamEnded,

    #[error("the microphone callback supplied an incomplete audio frame")]
    IncompleteFrame,

    #[error("the microphone worker fell behind and lost audio")]
    Overrun,

    #[error("failed to convert microphone audio")]
    Resample(#[from] ResampleError),

    #[error("microphone sample count exceeds the supported recording bound")]
    SampleCountOverflow,

    #[error("failed to start the microphone worker thread")]
    SpawnWorker(#[source] std::io::Error),

    #[error("the microphone worker thread is not running")]
    WorkerGone,

    #[error("the microphone worker recovered from an internal panic")]
    WorkerPanicked,

    #[error("voice-activity detection failed")]
    VoiceActivity(#[source] VoiceActivityError),
}
