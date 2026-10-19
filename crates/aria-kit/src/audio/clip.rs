use std::time::Duration;

use super::{CANONICAL_SAMPLE_RATE, duration_at};
#[cfg(not(target_family = "wasm"))]
use super::{ResampleError, Resampler};

// Telephone audio up to studio hardware. The resampler sizes its buffers by
// the rate, so a rate read from a file header needs bounds.
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 384_000;

/// Mono samples and the rate they were taken at.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    sample_rate: u32,
    samples: Vec<f32>,
}

impl Clip {
    pub fn new(sample_rate: u32, samples: Vec<f32>) -> Result<Self, ClipError> {
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            return Err(ClipError::SampleRate(sample_rate));
        }
        Ok(Self {
            sample_rate,
            samples,
        })
    }

    /// Labels samples as 16 kHz. It does not resample them.
    pub fn canonical(samples: Vec<f32>) -> Self {
        Self {
            sample_rate: CANONICAL_SAMPLE_RATE,
            samples,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    pub fn into_samples(self) -> Vec<f32> {
        self.samples
    }

    pub fn duration(&self) -> Duration {
        duration_at(self.samples.len() as u64, self.sample_rate)
    }

    pub fn is_canonical(&self) -> bool {
        self.sample_rate == CANONICAL_SAMPLE_RATE
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn into_canonical(self) -> Result<Self, ResampleError> {
        if self.is_canonical() {
            return Ok(self);
        }
        let samples = Resampler::convert(self.sample_rate, &self.samples)?;
        Ok(Self::canonical(samples))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClipError {
    #[error("sample rate {0} Hz is outside 8 kHz to 384 kHz")]
    SampleRate(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_accepts_the_edges_of_the_rate_band() {
        assert!(Clip::new(MIN_SAMPLE_RATE, vec![0.0]).is_ok());
        assert!(Clip::new(MAX_SAMPLE_RATE, vec![0.0]).is_ok());
    }

    #[test]
    fn a_clip_rejects_a_rate_outside_the_band() {
        for rate in [0, MIN_SAMPLE_RATE - 1, MAX_SAMPLE_RATE + 1, u32::MAX] {
            assert!(
                matches!(Clip::new(rate, vec![0.0]), Err(ClipError::SampleRate(r)) if r == rate),
                "{rate} Hz must be refused"
            );
        }
    }

    #[test]
    fn duration_follows_the_clip_rate() {
        let clip = Clip::new(24_000, vec![0.0; 36_000]).expect("the rate is usable");

        assert_eq!(clip.duration(), Duration::from_millis(1_500));
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn into_canonical_keeps_the_duration() {
        let clip = Clip::new(24_000, vec![0.1; 24_000]).expect("the rate is usable");

        let canonical = clip.into_canonical().expect("the clip resamples");

        assert!(canonical.is_canonical());
        assert_eq!(canonical.samples().len(), CANONICAL_SAMPLE_RATE as usize);
    }
}
