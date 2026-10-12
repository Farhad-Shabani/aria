//! Conversion from the microphone's native sample rate to canonical PCM.

use std::collections::VecDeque;

use rtrb::Consumer;

use crate::audio::{CANONICAL_SAMPLE_RATE, Resampler, f32_from_pcm16, pcm16_from_f32};

use super::MicrophoneError;

const RING_READ_SAMPLES: usize = 1_024;

type Result<T> = std::result::Result<T, MicrophoneError>;

pub(super) struct SampleRateConverter(Converter);

enum Converter {
    /// Reads the ring straight into the caller's buffer. `Resampler` passes
    /// 16 kHz through too, but only after an f32 round trip and a queue copy.
    Passthrough,
    Resampling(RingResampler),
}

impl SampleRateConverter {
    pub(super) fn new(input_sample_rate: u32) -> Result<Self> {
        if input_sample_rate == CANONICAL_SAMPLE_RATE {
            return Ok(Self(Converter::Passthrough));
        }
        Ok(Self(Converter::Resampling(RingResampler::new(
            input_sample_rate,
        )?)))
    }

    pub(super) fn pop<'samples>(
        &mut self,
        source: &mut Consumer<i16>,
        destination: &'samples mut [i16],
    ) -> Result<&'samples mut [i16]> {
        match &mut self.0 {
            Converter::Passthrough => Ok(pop_source(source, destination)),
            Converter::Resampling(converter) => converter.pop(source, destination),
        }
    }
}

/// Pulls native-rate samples off the ring and queues them at 16 kHz.
struct RingResampler {
    resampler: Resampler,
    read: Vec<i16>,
    floats: Vec<f32>,
    output: VecDeque<i16>,
}

impl RingResampler {
    fn new(input_sample_rate: u32) -> Result<Self> {
        Ok(Self {
            resampler: Resampler::new(input_sample_rate)?,
            read: vec![0; RING_READ_SAMPLES],
            floats: Vec::with_capacity(RING_READ_SAMPLES),
            output: VecDeque::new(),
        })
    }

    fn pop<'samples>(
        &mut self,
        source: &mut Consumer<i16>,
        destination: &'samples mut [i16],
    ) -> Result<&'samples mut [i16]> {
        let mut written = self.drain_output(destination);
        while written < destination.len() {
            let popped = pop_source(source, &mut self.read).len();
            if popped == 0 {
                break;
            }
            self.floats.clear();
            self.floats
                .extend(self.read[..popped].iter().copied().map(f32_from_pcm16));
            let output = &mut self.output;
            self.resampler.push(&self.floats, |block| {
                output.extend(block.iter().copied().map(pcm16_from_f32));
            })?;
            written += self.drain_output(&mut destination[written..]);
        }
        Ok(&mut destination[..written])
    }

    fn drain_output(&mut self, destination: &mut [i16]) -> usize {
        let count = destination.len().min(self.output.len());
        for (destination, sample) in destination[..count]
            .iter_mut()
            .zip(self.output.drain(..count))
        {
            *destination = sample;
        }
        count
    }
}

fn pop_source<'samples>(
    source: &mut Consumer<i16>,
    destination: &'samples mut [i16],
) -> &'samples mut [i16] {
    source.pop_partial_slice(destination).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_48k_ring_comes_out_at_16k_at_the_same_level() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::new(48_000);
        for _ in 0..48_000 {
            producer.push(16_384).expect("the ring holds one second");
        }
        let mut converter = SampleRateConverter::new(48_000).expect("48 kHz converts");
        let mut destination = vec![0; 32_000];

        let output = converter
            .pop(&mut consumer, &mut destination)
            .expect("the ring resamples");

        // No flush: the filter's tail waits for more audio.
        assert!(
            (15_000..=16_000).contains(&output.len()),
            "got {}",
            output.len()
        );
        for (index, &sample) in output[1_000..output.len() - 1_000].iter().enumerate() {
            let drift = (i32::from(sample) - 16_384).abs();
            assert!(drift <= 33, "sample {} drifted by {drift}", index + 1_000);
        }
    }
}
