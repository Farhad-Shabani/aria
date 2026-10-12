//! Conversion of mono audio to the canonical sample rate.

use rubato::Resampler as _;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync};

use super::CANONICAL_SAMPLE_RATE;

const INPUT_CHUNK: usize = 1_024;

/// Converts mono f32 audio at any sample rate to canonical 16 kHz.
pub struct Resampler {
    mode: Mode,
}

enum Mode {
    Passthrough,
    Fft(Box<FftState>),
}

impl Resampler {
    pub fn new(sample_rate: u32) -> Result<Self, ResampleError> {
        let mode = if sample_rate == CANONICAL_SAMPLE_RATE {
            Mode::Passthrough
        } else {
            Mode::Fft(Box::new(FftState::new(sample_rate)?))
        };
        Ok(Self { mode })
    }

    /// Converts a whole clip at `sample_rate` in one call.
    pub fn convert(sample_rate: u32, samples: &[f32]) -> Result<Vec<f32>, ResampleError> {
        let mut resampler = Self::new(sample_rate)?;
        let mut converted = Vec::new();
        resampler.push(samples, |block| converted.extend_from_slice(block))?;
        resampler.finish(|block| converted.extend_from_slice(block))?;
        Ok(converted)
    }

    /// Hands each resampled block to `emit`.
    pub fn push(
        &mut self,
        samples: &[f32],
        mut emit: impl FnMut(&[f32]),
    ) -> Result<(), ResampleError> {
        match &mut self.mode {
            Mode::Passthrough => {
                emit(samples);
                Ok(())
            }
            Mode::Fft(state) => state.push(samples, &mut emit),
        }
    }

    /// Flushes the filter's tail, so the total output matches the input's
    /// duration.
    pub fn finish(mut self, mut emit: impl FnMut(&[f32])) -> Result<(), ResampleError> {
        match &mut self.mode {
            Mode::Passthrough => Ok(()),
            Mode::Fft(state) => state.flush(&mut emit),
        }
    }
}

struct FftState {
    fft: Fft<f32>,
    sample_rate: u32,
    chunk: Vec<f32>,
    filled: usize,
    output: Vec<f32>,
    delay: usize,
    samples_in: u64,
    samples_out: u64,
}

impl FftState {
    fn new(sample_rate: u32) -> Result<Self, ResampleError> {
        let fft = Fft::<f32>::new(
            sample_rate as usize,
            CANONICAL_SAMPLE_RATE as usize,
            INPUT_CHUNK,
            1,
            FixedSync::Input,
        )
        .map_err(ResampleError::initialize)?;
        let chunk = vec![0.0; fft.input_frames_next()];
        let output = vec![0.0; fft.output_frames_max()];
        let delay = fft.output_delay();
        Ok(Self {
            fft,
            sample_rate,
            chunk,
            filled: 0,
            output,
            delay,
            samples_in: 0,
            samples_out: 0,
        })
    }

    fn push(
        &mut self,
        mut samples: &[f32],
        emit: &mut impl FnMut(&[f32]),
    ) -> Result<(), ResampleError> {
        while !samples.is_empty() {
            let count = samples.len().min(self.chunk.len() - self.filled);
            self.chunk[self.filled..self.filled + count].copy_from_slice(&samples[..count]);
            self.filled += count;
            self.samples_in = self
                .samples_in
                .checked_add(count as u64)
                .ok_or(ResampleError::SampleCountOverflow)?;
            samples = &samples[count..];
            if self.filled == self.chunk.len() {
                self.process(emit)?;
            }
        }
        Ok(())
    }

    /// Pads with silence until the output has caught up with the input.
    fn flush(&mut self, emit: &mut impl FnMut(&[f32])) -> Result<(), ResampleError> {
        while self.samples_out < self.target_samples() {
            self.chunk[self.filled..].fill(0.0);
            self.process(emit)?;
        }
        Ok(())
    }

    fn target_samples(&self) -> u64 {
        (u128::from(self.samples_in) * u128::from(CANONICAL_SAMPLE_RATE)
            / u128::from(self.sample_rate)) as u64
    }

    fn process(&mut self, emit: &mut impl FnMut(&[f32])) -> Result<(), ResampleError> {
        let input = InterleavedSlice::new(&self.chunk, 1, self.chunk.len())
            .map_err(ResampleError::process)?;
        let output_capacity = self.output.len();
        let mut output = InterleavedSlice::new_mut(&mut self.output, 1, output_capacity)
            .map_err(ResampleError::process)?;
        let (_, produced) = self
            .fft
            .process_into_buffer(&input, &mut output, None)
            .map_err(ResampleError::process)?;
        self.filled = 0;
        let skipped = self.delay.min(produced);
        self.delay -= skipped;
        // Padding flushes the filter's tail but must not lengthen the output.
        let count = (produced - skipped).min((self.target_samples() - self.samples_out) as usize);
        emit(&self.output[skipped..skipped + count]);
        self.samples_out += count as u64;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResampleError {
    #[error("failed to set up resampling to {CANONICAL_SAMPLE_RATE} Hz")]
    Initialize(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("failed to resample audio to {CANONICAL_SAMPLE_RATE} Hz")]
    Process(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("resampler input sample count overflowed")]
    SampleCountOverflow,
}

impl ResampleError {
    fn initialize(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Initialize(Box::new(error))
    }

    fn process(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Process(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(rate: u32, input: &[f32], chunk: usize) -> Vec<f32> {
        let mut resampler = Resampler::new(rate).unwrap();
        let mut output = Vec::new();
        for samples in input.chunks(chunk) {
            resampler
                .push(samples, |block| output.extend_from_slice(block))
                .unwrap();
        }
        resampler
            .finish(|block| output.extend_from_slice(block))
            .unwrap();
        output
    }

    #[test]
    fn converting_a_whole_clip_matches_pushing_it_in_chunks() {
        let input: Vec<f32> = (0..4_801)
            .map(|index| (index as f32 * 0.01).sin())
            .collect();

        let converted = Resampler::convert(48_000, &input).unwrap();

        assert_eq!(converted, convert(48_000, &input, 137));
    }

    #[test]
    fn canonical_samples_pass_through_unchanged() {
        let samples = [0.0, 0.25, -0.5, 1.0];
        assert_eq!(convert(CANONICAL_SAMPLE_RATE, &samples, 3), samples);
    }

    #[test]
    fn resampling_preserves_duration_across_chunk_boundaries() {
        for count in [0, 1, 2, 3, 1_023, 1_024, 1_025, 48_017] {
            let input = vec![0.5; count];
            let output = convert(48_000, &input, 137);
            assert_eq!(output.len(), count / 3, "input length {count}");
            if output.len() > 1_000 {
                assert!(
                    output[100..output.len() - 100]
                        .iter()
                        .all(|x| (x - 0.5).abs() < 0.001)
                );
            }
        }
    }

    #[test]
    fn flushing_retains_an_impulse_near_the_end() {
        let mut input = vec![0.0; 4_101];
        input[4_080] = 1.0;
        let output = convert(48_000, &input, 257);
        assert_eq!(output.len(), 1_367);
        assert!(output[1_350..].iter().any(|sample| sample.abs() > 0.1));
        assert_eq!(output, convert(48_000, &input, input.len()));
    }
}
