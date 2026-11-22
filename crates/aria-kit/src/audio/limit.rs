//! The longest recording capture may keep, and the writer that holds it to
//! that length.

use std::sync::Arc;
use std::time::Duration;

use super::recording::{Recording, RecordingWriter};
use super::{CANONICAL_SAMPLE_RATE, MAX_RECORDING_DURATION, canonical_samples};

const MIN_RECORDING_DURATION: Duration =
    Duration::from_nanos(1_000_000_000 / CANONICAL_SAMPLE_RATE as u64);

type Result<T> = std::result::Result<T, LengthError>;

/// Appends capture's 16-bit samples to its [`Recording`], refusing any that
/// would take it past its maximum length.
pub(super) struct LimitedWriter {
    recording: Arc<Recording>,
    samples_written: u64,
    maximum_samples: u64,
}

impl LimitedWriter {
    pub(super) fn new(maximum_duration: Duration, writer: &RecordingWriter) -> Result<Self> {
        Ok(Self {
            recording: writer.recording(),
            samples_written: 0,
            maximum_samples: maximum_samples(maximum_duration)?,
        })
    }

    /// Appends samples to the recording. Fails once the maximum length would
    /// be exceeded.
    pub(super) fn write_samples(&mut self, samples: &[i16]) -> Result<()> {
        let sample_count =
            u64::try_from(samples.len()).map_err(|_error| LengthError::SampleCountOverflow)?;
        let attempted_samples = self
            .samples_written
            .checked_add(sample_count)
            .ok_or(LengthError::SampleCountOverflow)?;
        if attempted_samples > self.maximum_samples {
            return Err(LengthError::LimitExceeded {
                attempted_samples,
                maximum_samples: self.maximum_samples,
            });
        }
        self.samples_written = attempted_samples;
        self.recording.append(samples);
        Ok(())
    }

    /// Ends the writing. A recording that got no samples is refused.
    pub(super) fn finish(self) -> Result<()> {
        if self.samples_written == 0 {
            return Err(LengthError::Empty);
        }
        Ok(())
    }
}

pub(in crate::audio) fn maximum_samples(duration: Duration) -> Result<u64> {
    if !(MIN_RECORDING_DURATION..=MAX_RECORDING_DURATION).contains(&duration) {
        return Err(LengthError::InvalidDuration {
            actual: duration,
            minimum: MIN_RECORDING_DURATION,
            maximum: MAX_RECORDING_DURATION,
        });
    }

    Ok(canonical_samples(duration))
}

#[derive(Debug, thiserror::Error)]
pub enum LengthError {
    #[error("recording duration must be between {minimum:?} and {maximum:?}; got {actual:?}")]
    InvalidDuration {
        actual: Duration,
        minimum: Duration,
        maximum: Duration,
    },

    #[error(
        "recording would contain {attempted_samples} samples; maximum is {maximum_samples} samples"
    )]
    LimitExceeded {
        attempted_samples: u64,
        maximum_samples: u64,
    },

    #[error("recording sample count overflowed")]
    SampleCountOverflow,

    #[error("cannot finish an empty recording")]
    Empty,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_past_its_limit_is_refused() {
        let (_recording, writer) = super::super::recording::ends();
        let mut limited =
            LimitedWriter::new(Duration::from_secs(1), &writer).expect("the limit is valid");

        let error = limited
            .write_samples(&vec![0; CANONICAL_SAMPLE_RATE as usize + 1])
            .expect_err("an oversized recording is refused");

        assert!(matches!(error, LengthError::LimitExceeded { .. }));
    }

    #[test]
    fn a_duration_past_the_wav_format_is_refused() {
        let past_the_format = MAX_RECORDING_DURATION + Duration::from_secs(1);

        assert!(matches!(
            maximum_samples(past_the_format),
            Err(LengthError::InvalidDuration { .. })
        ));
        assert_eq!(
            maximum_samples(MAX_RECORDING_DURATION).expect("the bound itself is allowed"),
            crate::audio::MAX_RECORDING_SAMPLES
        );
    }
}
