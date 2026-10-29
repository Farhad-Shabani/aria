//! Audio capture: the sources, the recordings they write, and the canonical
//! 16 kHz mono format shared by everything that reads them. A [`Clip`] is
//! finished audio at its own rate.
//!
//! [`RecordingReader`] builds on every target. Everything that opens a
//! microphone, a model, or a file is native.

mod clip;
mod recording;
mod stop;

#[cfg(not(target_family = "wasm"))]
mod capture;
#[cfg(not(target_family = "wasm"))]
mod resample;
#[cfg(not(target_family = "wasm"))]
mod spool;
#[cfg(all(not(target_family = "wasm"), any(test, feature = "test-support")))]
pub(crate) mod testing;
#[cfg(not(target_family = "wasm"))]
mod wav;

use std::time::Duration;

pub use clip::{Clip, ClipError};
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
pub use recording::RecordingWaker;
#[cfg(not(target_family = "wasm"))]
pub use recording::SpooledRecordingWriter;
pub use recording::{CaptureError, RecordedAudio, Recording, RecordingReader, RecordingWriter};
#[cfg(not(target_family = "wasm"))]
pub use resample::{ResampleError, Resampler};
pub use stop::{StopSignal, Stopper};

#[cfg(not(target_family = "wasm"))]
pub use capture::{
    AudioCapture, AutoStop, AutoStopError, MicrophoneError, RecordingMode, ReplayPace,
    SilenceDuration, SilenceDurationError, WavReplayCapture, WavReplayError,
};
#[cfg(all(feature = "microphone", not(target_family = "wasm")))]
pub use capture::{
    Microphone, MicrophoneBuilder, PreparedMicrophone, WakeWordControl, WakeWordEvent,
    WakeWordFailure, WakeWordMonitor, WakeWordSuspension, check_default_input_device,
};
#[cfg(not(target_family = "wasm"))]
pub use spool::{SpoolError, remove_all_spools};
#[cfg(all(not(target_family = "wasm"), any(test, feature = "test-support")))]
pub use testing::{canonical_fixture, fixture_of};
#[cfg(not(target_family = "wasm"))]
pub use wav::{WavError, read_wav, write_wav};

pub const CANONICAL_SAMPLE_RATE: u32 = 16_000;

/// The longest audio a spool writes or `from_wav_file` reads. hound sums a
/// 16-bit mono WAV's RIFF size as data bytes plus 36 in a `u32`, so one more
/// sample wraps it. That is about 37 hours.
#[cfg(not(target_family = "wasm"))]
pub(crate) const MAX_RECORDING_SAMPLES: u64 = (u32::MAX as u64 - 36) / 2;
#[cfg(not(target_family = "wasm"))]
pub(crate) const MAX_RECORDING_DURATION: Duration =
    Duration::from_nanos(MAX_RECORDING_SAMPLES * (1_000_000_000 / CANONICAL_SAMPLE_RATE as u64));
pub const CHANNELS: u16 = 1;

/// What a 16-bit sample divides by to land in -1.0..1.0.
pub(crate) const PCM_SCALE: f32 = 32_768.0;

/// The nearest 16-bit sample to `sample`, saturating outside -1.0..1.0.
#[cfg(not(target_family = "wasm"))]
pub fn pcm16_from_f32(sample: f32) -> i16 {
    (sample * PCM_SCALE)
        .round()
        .clamp(-PCM_SCALE, PCM_SCALE - 1.0) as i16
}

pub fn f32_from_pcm16(sample: i16) -> f32 {
    f32::from(sample) / PCM_SCALE
}

/// How many canonical samples `duration` spans, rounded down. Saturates at
/// `u64::MAX`.
pub const fn canonical_samples(duration: Duration) -> u64 {
    let samples = samples_at(duration, CANONICAL_SAMPLE_RATE);
    if samples > u64::MAX as u128 {
        u64::MAX
    } else {
        samples as u64
    }
}

/// How long `samples` canonical samples last.
pub fn canonical_duration(samples: u64) -> Duration {
    duration_at(samples, CANONICAL_SAMPLE_RATE)
}

pub(crate) const fn samples_at(duration: Duration, sample_rate: u32) -> u128 {
    let rate = sample_rate as u128;
    duration.as_secs() as u128 * rate + duration.subsec_nanos() as u128 * rate / 1_000_000_000
}

pub(crate) fn duration_at(samples: u64, sample_rate: u32) -> Duration {
    let rate = u64::from(sample_rate);
    Duration::from_secs(samples / rate)
        + Duration::from_nanos(samples % rate * 1_000_000_000 / rate)
}
