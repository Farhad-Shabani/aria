//! Playing synthesized speech on the default output device.
//!
//! The audio callback reads buffered samples, converts their rate, and fills
//! underruns with silence. Cancellation and device-drain waits run on the caller's thread.

use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use super::{PlaybackError, SpeechStream, TryRead};
use crate::cancel::CancelToken;

/// How often the playing thread looks at the cancel token.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Grace period for the device to play buffered audio, including Bluetooth latency.
const DRAIN_GRACE: Duration = Duration::from_millis(600);

/// Checks for an output device at construction and resolves it again for each utterance.
pub struct SpeakerOutput;

impl SpeakerOutput {
    pub fn new() -> Result<Self, PlaybackError> {
        default_output_device()?;
        Ok(Self)
    }

    /// Calls `started` when the first source sample reaches the output callback.
    /// Waits a drain grace period after the final sample. Cancellation drops the stream.
    pub fn play_with(
        &self,
        speech: SpeechStream,
        cancel: &CancelToken,
        started: impl FnOnce(),
    ) -> Result<(), PlaybackError> {
        // Devices come and go, so each utterance resolves its own.
        let device = default_output_device()?;
        let config = device
            .default_output_config()
            .map_err(|source| PlaybackError::NoUsableConfig(Box::new(source)))?;

        let (outcomes, outcome) = sync_channel(2);
        let feed = Feed::new(speech, config.sample_rate(), outcomes.clone());
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => open_stream::<f32>(&device, config.into(), feed, outcomes),
            cpal::SampleFormat::I16 => open_stream::<i16>(&device, config.into(), feed, outcomes),
            cpal::SampleFormat::U16 => open_stream::<u16>(&device, config.into(), feed, outcomes),
            other => Err(PlaybackError::UnsupportedFormat(other.to_string())),
        }?;
        stream
            .play()
            .map_err(|source| PlaybackError::Stream(Box::new(source)))?;

        let mut started = Some(started);
        loop {
            match outcome.recv_timeout(CANCEL_POLL_INTERVAL) {
                Ok(PlaybackEnd::Started) => {
                    if let Some(started) = started.take() {
                        started();
                    }
                }
                Ok(PlaybackEnd::Drained) => {
                    drain(cancel);
                    return Ok(());
                }
                Ok(PlaybackEnd::DeviceLost) => return Err(PlaybackError::DeviceLost),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() {
                        // Dropping the stream on return is the stop.
                        return Err(PlaybackError::Cancelled);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(PlaybackError::DeviceLost);
                }
            }
        }
    }
}

/// Lets the device play out what it already holds. A stop cuts the wait
/// short: the caller drops the stream next, and silence is what it asked for.
fn drain(cancel: &CancelToken) {
    let until = std::time::Instant::now() + DRAIN_GRACE;
    while std::time::Instant::now() < until {
        if cancel.is_cancelled() {
            return;
        }
        std::thread::sleep(CANCEL_POLL_INTERVAL);
    }
}

fn default_output_device() -> Result<cpal::Device, PlaybackError> {
    cpal::default_host()
        .default_output_device()
        .ok_or(PlaybackError::NoOutputDevice)
}

fn open_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut feed: Feed,
    outcomes: SyncSender<PlaybackEnd>,
) -> Result<cpal::Stream, PlaybackError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                feed.fill(data, channels);
            },
            move |error| {
                tracing::warn!(%error, "the output stream failed");
                let _listener_gone = outcomes.try_send(PlaybackEnd::DeviceLost);
            },
            None,
        )
        .map_err(|source| PlaybackError::Stream(Box::new(source)))
}

enum PlaybackEnd {
    Started,
    /// Every source sample was handed to the device.
    Drained,
    DeviceLost,
}

/// The callback's side of the utterance: pulls source samples, converts the
/// rate linearly, and duplicates mono across the device's channels.
struct Feed {
    speech: SpeechStream,
    /// Source samples per output frame.
    step: f64,
    /// Fractional read position into the source, relative to `pending[0]`.
    position: f64,
    pending: Vec<f32>,
    ended: bool,
    reported: bool,
    started: bool,
    outcomes: SyncSender<PlaybackEnd>,
}

impl Feed {
    fn new(speech: SpeechStream, device_rate: u32, outcomes: SyncSender<PlaybackEnd>) -> Self {
        let step = f64::from(speech.sample_rate()) / f64::from(device_rate);
        Self {
            speech,
            step,
            position: 0.0,
            pending: Vec::new(),
            ended: false,
            reported: false,
            started: false,
            outcomes,
        }
    }

    fn fill<T>(&mut self, data: &mut [T], channels: usize)
    where
        T: SizedSample + FromSample<f32>,
    {
        for frame in data.chunks_mut(channels) {
            let value = self.next_sample().unwrap_or(0.0);
            let converted = T::from_sample(value);
            for slot in frame {
                *slot = converted;
            }
        }
        if self.ended && self.consumed() && !self.reported {
            self.reported = true;
            let _listener_gone = self.outcomes.try_send(PlaybackEnd::Drained);
        }
    }

    /// The next output-rate sample, or `None` while starved or after the end.
    fn next_sample(&mut self) -> Option<f32> {
        let wanted = self.position as usize + 2;
        while !self.ended && self.pending.len() < wanted {
            let mut chunk = [0.0; 512];
            match self.speech.try_read(&mut chunk) {
                TryRead::Samples(count) => self.pending.extend_from_slice(&chunk[..count]),
                // An underrun plays as silence; the source position holds so
                // nothing is skipped when samples catch up.
                TryRead::Starved => return None,
                TryRead::Ended => self.ended = true,
            }
        }
        let index = self.position as usize;
        let first = *self.pending.get(index)?;
        let second = self.pending.get(index + 1).copied().unwrap_or(first);
        let fraction = (self.position - index as f64) as f32;
        self.position += self.step;

        // What interpolation no longer reaches is dropped, keeping the
        // buffer a chunk long rather than the whole utterance.
        let consumed = self.position as usize;
        if consumed > 0 {
            self.pending.drain(..consumed.min(self.pending.len()));
            self.position -= consumed as f64;
        }
        if !self.started {
            self.started = true;
            let _listener_gone = self.outcomes.try_send(PlaybackEnd::Started);
        }
        Some(first + (second - first) * fraction)
    }

    fn consumed(&self) -> bool {
        self.pending.len() <= 1
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::TryRecvError;

    use super::*;
    use crate::synthesis::stream;

    fn feed(source_rate: u32, device_rate: u32, samples: &[f32]) -> Feed {
        let (mut writer, speech) = stream(source_rate);
        writer.write(samples).expect("the reader is listening");
        writer.finish();
        let (outcomes, _receiver) = sync_channel(2);
        Feed::new(speech, device_rate, outcomes)
    }

    #[test]
    fn equal_rates_pass_samples_through() {
        let mut feed = feed(24_000, 24_000, &[0.1, 0.2, 0.3]);
        let mut data = [9.9f32; 4];

        feed.fill(&mut data, 1);

        assert_eq!(data[..3], [0.1, 0.2, 0.3]);
        assert_eq!(data[3], 0.0, "past the end plays silence");
    }

    #[test]
    fn a_mono_source_fills_every_device_channel() {
        let mut feed = feed(24_000, 24_000, &[0.5]);
        let mut data = [9.9f32; 2];

        feed.fill(&mut data, 2);

        assert_eq!(data, [0.5, 0.5]);
    }

    #[test]
    fn upsampling_interpolates_between_source_samples() {
        let mut feed = feed(24_000, 48_000, &[0.0, 1.0]);
        let mut data = [9.9f32; 4];

        feed.fill(&mut data, 1);

        assert_eq!(data[..2], [0.0, 0.5]);
    }

    #[test]
    fn the_drained_report_fires_once_everything_played() {
        let (mut writer, speech) = stream(24_000);
        writer.write(&[0.1, 0.2]).expect("the reader is listening");
        writer.finish();
        let (outcomes, ended) = sync_channel(2);
        let mut feed = Feed::new(speech, 24_000, outcomes);
        let mut data = [0.0f32; 8];

        feed.fill(&mut data, 1);

        assert!(matches!(ended.try_recv(), Ok(PlaybackEnd::Started)));
        assert!(matches!(ended.try_recv(), Ok(PlaybackEnd::Drained)));
        assert!(matches!(ended.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn a_starved_stream_plays_silence_and_resumes_where_it_left_off() {
        let (mut writer, speech) = stream(24_000);
        let (outcomes, _receiver) = sync_channel(2);
        let mut feed = Feed::new(speech, 24_000, outcomes);

        let mut data = [9.9f32; 2];
        feed.fill(&mut data, 1);
        assert_eq!(data, [0.0, 0.0], "an underrun plays silence");
        assert!(matches!(_receiver.try_recv(), Err(TryRecvError::Empty)));

        writer.write(&[0.7]).expect("the reader is listening");
        writer.finish();
        let mut data = [9.9f32; 1];
        feed.fill(&mut data, 1);
        assert_eq!(data, [0.7], "no sample was skipped over the underrun");
        assert!(matches!(_receiver.try_recv(), Ok(PlaybackEnd::Started)));
    }
}
