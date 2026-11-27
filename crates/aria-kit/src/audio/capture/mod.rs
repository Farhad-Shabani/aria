//! Audio sources behind [`AudioCapture`]: the default microphone and WAV replay.

#[cfg(feature = "microphone")]
mod microphone;
#[cfg(feature = "microphone")]
mod modes;
#[cfg(feature = "microphone")]
mod pushed;
mod wav_replay;

use aria_primitives::Cause;
use std::fmt;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::audio::{CaptureError, RecordingReader, ResampleError, StopSignal};
use crate::voice_activity::VoiceActivityError;

#[cfg(feature = "microphone")]
pub use microphone::{
    IdleStream, InputDevice, Microphone, MicrophoneBuilder, PreparedMicrophone, WakeWordControl,
    WakeWordEvent, WakeWordFailure, WakeWordMonitor, WakeWordSuspension,
    check_fallback_input_device, input_devices, is_available, resolve_input,
};
#[cfg(feature = "microphone")]
pub use pushed::PushedCapture;
pub use wav_replay::{ReplayPace, WavReplayCapture, WavReplayError};

const MIN_SILENCE_DURATION: Duration = Duration::from_millis(100);
const MAX_SILENCE_DURATION: Duration = Duration::from_secs(10);
const DEFAULT_SILENCE_DURATION: Duration = Duration::from_millis(1_500);
const MIN_AUTO_STOP: Duration = Duration::from_secs(3);
const MAX_AUTO_STOP: Duration = Duration::from_secs(60);
const DEFAULT_AUTO_STOP: Duration = Duration::from_secs(10);

/// A source of canonical 16 kHz mono audio. Each implementation bounds how
/// long a recording runs.
pub trait AudioCapture: Send + Sync + 'static {
    /// Starts capturing and returns a reader for the new recording. Returns
    /// once capture is live or the recording has ended, and fails if it
    /// ended in error.
    ///
    /// A live source writes from its own thread and polls `stop` there, so
    /// the caller can transcribe while audio arrives. A source with no
    /// remote input ignores the inputs `source` allows, and one that takes
    /// no pushed audio refuses it with [`CaptureError::PushedUnsupported`].
    fn start(
        &self,
        mode: RecordingMode,
        source: Source,
        stop: StopSignal,
    ) -> Result<RecordingReader, CaptureError>;
}

/// Who records one recording. `Source::Microphone(Inputs::Local)` records
/// from this machine's input device. `Source::Pushed(feed)` records the
/// samples a remote device sends as it records.
#[derive(Debug)]
pub enum Source {
    /// This machine's capture, from the inputs it may use.
    Microphone(Inputs),
    /// Audio a remote device pushes in as it records it: the device that
    /// started the recording.
    Pushed(PushedFeed),
}

/// Audio pushed in from outside, as it arrives: a remote device's 16 kHz
/// samples, chunk by chunk, until it stops sending.
#[cfg_attr(
    not(feature = "microphone"),
    expect(dead_code, reason = "only a pushed capture reads it")
)]
pub struct PushedFeed {
    pub(crate) first: Vec<i16>,
    pub(crate) rest: Receiver<Vec<i16>>,
    pub(crate) stall: Duration,
    pub(crate) finished: Box<dyn FnOnce(FeedEnd) + Send>,
}

/// What ended a capture that took a [`PushedFeed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedEnd {
    /// No samples arrived for the feed's stall.
    Stalled,
    /// Its mode, a stop, the feed closing, its duration limit, or a failure.
    Ended,
}

impl PushedFeed {
    /// The recording opens with `first` and goes on with what `rest`
    /// brings, until it closes or no samples arrive for `stall`.
    /// `finished` runs once, when a capture that took the feed ends, and
    /// hears what ended it. A capture that refuses the feed never runs it.
    pub fn new(
        first: Vec<i16>,
        rest: Receiver<Vec<i16>>,
        stall: Duration,
        finished: impl FnOnce(FeedEnd) + Send + 'static,
    ) -> Self {
        Self {
            first,
            rest,
            stall,
            finished: Box::new(finished),
        }
    }
}

impl fmt::Debug for PushedFeed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PushedFeed")
            .field("stall", &self.stall)
            .finish_non_exhaustive()
    }
}

/// Which inputs one recording may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inputs {
    /// Any input the capture offers, a remote one included.
    Any,
    /// Only this machine's own devices.
    Local,
}

/// One backend serves every operation that records, so the operations share
/// it rather than each opening a microphone of their own.
impl<C: AudioCapture> AudioCapture for std::sync::Arc<C> {
    fn start(
        &self,
        mode: RecordingMode,
        source: Source,
        stop: StopSignal,
    ) -> Result<RecordingReader, CaptureError> {
        (**self).start(mode, source, stop)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingMode {
    StopOnSilence {
        trailing_silence: SilenceDuration,
    },
    UntilStopped,
    /// Records until stopped, or until `auto_stop` passes without speech.
    /// Silence before the first word counts, and all audio is kept.
    UntilQuiet {
        auto_stop: AutoStop,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoStop(Duration);

impl AutoStop {
    pub fn new(duration: Duration) -> Result<Self, AutoStopError> {
        if !(MIN_AUTO_STOP..=MAX_AUTO_STOP).contains(&duration) {
            return Err(AutoStopError::OutOfRange {
                actual: duration,
                minimum: MIN_AUTO_STOP,
                maximum: MAX_AUTO_STOP,
            });
        }
        if !duration.subsec_nanos().is_multiple_of(1_000_000) {
            return Err(AutoStopError::SubmillisecondPrecision { actual: duration });
        }
        Ok(Self(duration))
    }

    pub fn get(self) -> Duration {
        self.0
    }
}

impl Default for AutoStop {
    fn default() -> Self {
        Self(DEFAULT_AUTO_STOP)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AutoStopError {
    #[error("auto-stop must be between {minimum:?} and {maximum:?}; got {actual:?}")]
    OutOfRange {
        actual: Duration,
        minimum: Duration,
        maximum: Duration,
    },

    #[error("auto-stop must use whole-millisecond precision; got {actual:?}")]
    SubmillisecondPrecision { actual: Duration },
}

#[derive(Debug, thiserror::Error)]
pub enum MicrophoneError {
    #[error("no microphone is available")]
    NoInputDevice,

    #[error("failed to list the microphones")]
    ListDevices(#[source] Cause),

    #[error("failed to read the microphone configuration")]
    DefaultConfiguration(#[source] Cause),

    #[error("failed to read the microphone description")]
    DeviceDescription(#[source] Cause),

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
    BuildStream(#[source] Cause),

    #[error("failed to start the microphone input stream")]
    StartStream(#[source] Cause),

    #[error("failed to pause the microphone input stream")]
    PauseStream(#[source] Cause),

    #[error("the microphone input stream failed")]
    Stream(#[source] Cause),

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_stop_accepts_its_bounds_and_refuses_beyond_them() {
        assert!(AutoStop::new(MIN_AUTO_STOP).is_ok());
        assert!(AutoStop::new(MAX_AUTO_STOP).is_ok());
        assert!(matches!(
            AutoStop::new(MIN_AUTO_STOP - Duration::from_millis(1)),
            Err(AutoStopError::OutOfRange { .. })
        ));
        assert!(matches!(
            AutoStop::new(MAX_AUTO_STOP + Duration::from_millis(1)),
            Err(AutoStopError::OutOfRange { .. })
        ));
    }
}
