//! The default input device's stream, and the ring that carries its samples
//! off the real-time callback.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, SupportedStreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::audio::samples_at;

use super::resampling::SampleRateConverter;
use super::worker::IdleObservation;
use super::{MicrophoneError, Result};

const SAMPLE_RING_DURATION: Duration = Duration::from_millis(500);
const MAX_INPUT_SAMPLE_RATE: u32 = 384_000;
pub(super) const DRAIN_BUFFER_SAMPLES: usize = 1_024;
const STREAM_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(2);

/// A playing microphone stream and the state that decodes it.
pub(super) struct LiveStream {
    /// Held only to keep the input stream playing.
    _stream: cpal::Stream,
    consumer: Consumer<i16>,
    status: Arc<CaptureStatus>,
    converter: SampleRateConverter,
    errors: Receiver<cpal::Error>,
}

/// Checks the default microphone as `LiveStream::open` does, without opening
/// a stream, because that triggers the permission prompt. Returns the device
/// name.
pub fn check_default_input_device() -> Result<String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or(MicrophoneError::NoInputDevice)?;
    // Not `Display`: it panics if the description fails, which happens when
    // the device was unplugged after it was chosen.
    let description = device
        .description()
        .map_err(|source| MicrophoneError::DeviceDescription(Box::new(source)))?;
    select_input_config(&device)?;
    Ok(description.name().to_owned())
}

impl LiveStream {
    pub(super) fn open() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(MicrophoneError::NoInputDevice)?;
        let config = select_input_config(&device)?;
        let sample_rate = config.sample_rate();
        let (producer, consumer, status) = sample_ring(sample_rate, config.channels())?;
        let converter = SampleRateConverter::new(sample_rate)?;
        let (error_sender, errors) = sync_channel(1);
        let stream = build_stream(&device, config, producer, error_sender)?;

        stream
            .play()
            .map_err(|source| MicrophoneError::StartStream(Box::new(source)))?;
        Ok(Self {
            _stream: stream,
            consumer,
            status,
            converter,
            errors,
        })
    }

    pub(super) fn pop<'samples>(
        &mut self,
        destination: &'samples mut [i16],
    ) -> Result<&'samples mut [i16]> {
        Ok(self.converter.pop(&mut self.consumer, destination)?)
    }

    pub(super) fn check_state(&self) -> Result<()> {
        self.check_failures()?;
        if self.consumer.is_abandoned() {
            return Err(MicrophoneError::StreamEnded.into());
        }
        Ok(())
    }

    pub(super) fn check_failures(&self) -> Result<()> {
        if self.status.incomplete_frame.load(Ordering::Acquire) {
            return Err(MicrophoneError::IncompleteFrame.into());
        }
        if self.status.overrun.load(Ordering::Acquire) {
            return Err(MicrophoneError::Overrun.into());
        }
        if let Ok(error) = self.errors.try_recv() {
            return Err(MicrophoneError::Stream(Box::new(error)).into());
        }
        Ok(())
    }

    /// Moves pending samples into the bounded pre-press buffer, keeping its
    /// newest `capacity` samples, so the ring cannot overrun while idle.
    pub(super) fn drain_into(
        &mut self,
        pre_press: &mut VecDeque<i16>,
        capacity: usize,
        mut observe: impl FnMut(&[i16]) -> IdleObservation,
    ) -> Result<()> {
        let mut scratch = [0_i16; DRAIN_BUFFER_SAMPLES];
        loop {
            self.check_state()?;
            let samples = self.pop(&mut scratch)?;
            if samples.is_empty() {
                return Ok(());
            }
            if observe(samples) == IdleObservation::Detected {
                pre_press.clear();
                continue;
            }
            pre_press.extend(samples.iter().copied());
            if pre_press.len() > capacity {
                pre_press.drain(..pre_press.len() - capacity);
            }
        }
    }
}

struct CaptureProducer {
    samples: Producer<i16>,
    channels: u16,
    status: Arc<CaptureStatus>,
}

impl CaptureProducer {
    fn push<T>(&mut self, input: &[T])
    where
        T: Sample,
        i16: FromSample<T>,
    {
        let mut frames = input.chunks_exact(usize::from(self.channels));
        if !frames.remainder().is_empty() {
            self.status.incomplete_frame.store(true, Ordering::Release);
            return;
        }

        let frame_count = frames.len();
        if self.samples.slots() < frame_count {
            self.status.overrun.store(true, Ordering::Release);
            return;
        }

        let mut converted = [0_i16; DRAIN_BUFFER_SAMPLES];
        let mut converted_count = 0;
        for frame in &mut frames {
            converted[converted_count] = downmix(frame, self.channels);
            converted_count += 1;

            if converted_count == converted.len() {
                self.push_converted(&converted);
                converted_count = 0;
            }
        }
        self.push_converted(&converted[..converted_count]);
    }

    fn push_converted(&mut self, samples: &[i16]) {
        if self.samples.push_entire_slice(samples).is_err() {
            self.status.overrun.store(true, Ordering::Release);
        }
    }
}

#[derive(Default)]
struct CaptureStatus {
    incomplete_frame: AtomicBool,
    overrun: AtomicBool,
}

fn sample_ring(
    sample_rate: u32,
    channels: u16,
) -> Result<(CaptureProducer, Consumer<i16>, Arc<CaptureStatus>)> {
    let capacity = duration_sample_count(SAMPLE_RING_DURATION, sample_rate)?;
    let (producer, consumer) = RingBuffer::new(capacity);
    let status = Arc::new(CaptureStatus::default());

    Ok((
        CaptureProducer {
            samples: producer,
            channels,
            status: Arc::clone(&status),
        },
        consumer,
        status,
    ))
}

fn select_input_config(device: &cpal::Device) -> Result<SupportedStreamConfig> {
    let config = device
        .default_input_config()
        .map_err(|source| MicrophoneError::DefaultConfiguration(Box::new(source)))?;
    if config.channels() == 0 {
        return Err(MicrophoneError::NoInputChannels.into());
    }
    let sample_rate = config.sample_rate();
    if sample_rate == 0 {
        return Err(MicrophoneError::ZeroInputSampleRate.into());
    }
    if sample_rate > MAX_INPUT_SAMPLE_RATE {
        return Err(MicrophoneError::InputSampleRateTooHigh {
            actual: sample_rate,
            maximum: MAX_INPUT_SAMPLE_RATE,
        }
        .into());
    }
    if !supported_sample_format(config.sample_format()) {
        return Err(
            MicrophoneError::UnsupportedSampleFormat(config.sample_format().to_string()).into(),
        );
    }
    tracing::debug!(
        sample_rate,
        channels = config.channels(),
        sample_format = ?config.sample_format(),
        "selected native microphone configuration"
    );
    Ok(config)
}

fn duration_sample_count(duration: Duration, sample_rate: u32) -> Result<usize> {
    usize::try_from(samples_at(duration, sample_rate))
        .map_err(|_error| MicrophoneError::SampleCountOverflow.into())
}

fn supported_sample_format(format: SampleFormat) -> bool {
    matches!(
        format,
        SampleFormat::I16 | SampleFormat::F32 | SampleFormat::U16
    )
}

fn build_stream(
    device: &cpal::Device,
    config: SupportedStreamConfig,
    producer: CaptureProducer,
    errors: SyncSender<cpal::Error>,
) -> Result<cpal::Stream> {
    match config.sample_format() {
        SampleFormat::I16 => build_typed_stream::<i16>(device, config, producer, errors),
        SampleFormat::F32 => build_typed_stream::<f32>(device, config, producer, errors),
        SampleFormat::U16 => build_typed_stream::<u16>(device, config, producer, errors),
        format => Err(MicrophoneError::UnsupportedSampleFormat(format.to_string()).into()),
    }
}

fn build_typed_stream<T>(
    device: &cpal::Device,
    config: SupportedStreamConfig,
    mut producer: CaptureProducer,
    errors: SyncSender<cpal::Error>,
) -> Result<cpal::Stream>
where
    T: Sample + SizedSample,
    i16: FromSample<T>,
{
    device
        .build_input_stream(
            config.config(),
            move |input: &[T], _info| producer.push(input),
            move |error| {
                errors.try_send(error).ok();
            },
            Some(STREAM_INITIALIZATION_TIMEOUT),
        )
        .map_err(|source| MicrophoneError::BuildStream(Box::new(source)).into())
}

fn downmix<T>(frame: &[T], channels: u16) -> i16
where
    T: Sample,
    i16: FromSample<T>,
{
    let sum = frame
        .iter()
        .map(|&sample| i64::from(i16::from_sample(sample)))
        .sum::<i64>();
    let average = sum / i64::from(channels);
    match i16::try_from(average) {
        Ok(sample) => sample,
        Err(_error) if average.is_negative() => i16::MIN,
        Err(_error) => i16::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_the_channels() {
        assert_eq!(downmix(&[1_000_i16, 3_000], 2), 2_000);
        assert_eq!(downmix(&[-400_i16, 0, 400, -800], 4), -200);
    }
}
