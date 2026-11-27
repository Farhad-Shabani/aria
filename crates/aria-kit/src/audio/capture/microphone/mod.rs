//! The microphone: opened once, then recording one utterance at a time.

mod devices;
mod resampling;
mod stream;
mod transfer;
mod wake_word;
mod worker;

use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::audio::limit;
use crate::audio::recording;
use crate::audio::{CaptureError, RecordingReader, StopSignal, Stopper};
use crate::voice_activity::{SileroVad, VoiceActivityDetector};
use crate::wake_word::WakeWordDetector;

use super::{AudioCapture, Inputs, MicrophoneError, RecordingMode, Source};

use crate::audio::InputDeviceId;
pub use devices::{InputDevice, input_devices, is_available, resolve_input};
pub use stream::check_fallback_input_device;
pub use wake_word::{
    WakeWordControl, WakeWordEvent, WakeWordFailure, WakeWordMonitor, WakeWordSuspension,
};

/// What the microphone stream does between recordings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IdleStream {
    /// Keeps playing and buffers the second before a press, so a word spoken
    /// on the press keeps its start. The device stays in use.
    #[default]
    Held,
    /// Pauses between recordings while no wake word listens. Each recording
    /// waits for the device to start.
    Released,
}

const DEFAULT_MAXIMUM_DURATION: Duration = Duration::from_secs(60);

type Result<T> = std::result::Result<T, CaptureError>;

/// A microphone, ready to record. `open()` records on the system default,
/// for at most 60 seconds, and holds the stream between recordings. The
/// builder overrides each. Recordings stay in memory.
pub struct Microphone {
    commands: Sender<worker::Command>,
}

impl Microphone {
    pub fn open() -> Result<Self> {
        Self::builder().open()
    }

    pub fn builder() -> MicrophoneBuilder {
        MicrophoneBuilder {
            maximum_duration: DEFAULT_MAXIMUM_DURATION,
            input: None,
            idle: IdleStream::Held,
        }
    }

    /// Records on `input` while it's available, else the fallback.
    /// `None` follows the system default. A recording in progress keeps its
    /// device; the next one uses the new choice.
    pub fn use_input(&self, input: Option<InputDeviceId>) -> Result<()> {
        self.commands
            .send(worker::Command::UseInput(input))
            .map_err(|_command| MicrophoneError::WorkerGone.into())
    }

    /// Sets what the stream does between recordings. A recording in progress
    /// keeps going, and the change applies once it ends.
    pub fn use_idle(&self, idle: IdleStream) -> Result<()> {
        self.commands
            .send(worker::Command::UseIdle(idle))
            .map_err(|_command| MicrophoneError::WorkerGone.into())
    }

    /// Records one utterance. The recording fills in the background; hand the
    /// stopper to whatever should be able to end it early.
    // `&mut self`: one capture worker serves one recording at a time, so a
    // second concurrent record would silently queue behind the first.
    pub fn record(&mut self, mode: RecordingMode) -> Result<(RecordingReader, Stopper)> {
        let stopper = Stopper::new();
        let recording = self.start(
            mode,
            Source::Microphone(Inputs::Local),
            StopSignal::from(stopper.clone()),
        )?;
        Ok((recording, stopper))
    }
}

pub struct MicrophoneBuilder {
    maximum_duration: Duration,
    input: Option<InputDeviceId>,
    idle: IdleStream,
}

impl MicrophoneBuilder {
    /// The most a single recording may hold, up to about 37 hours. Each
    /// reader holds what it has not released, about 230 MB per hour.
    pub fn maximum_duration(mut self, maximum_duration: Duration) -> Self {
        self.maximum_duration = maximum_duration;
        self
    }

    /// The device to record on while it's connected, as
    /// [`Microphone::use_input`] takes it. The first stream opens on it.
    pub fn input(mut self, input: Option<InputDeviceId>) -> Self {
        self.input = input;
        self
    }

    pub fn idle(mut self, idle: IdleStream) -> Self {
        self.idle = idle;
        self
    }

    pub fn open(self) -> Result<Microphone> {
        self.load()?.open()
    }

    pub fn load(self) -> Result<PreparedMicrophone> {
        self.prepare(|| {
            SileroVad::load().map_err(|error| MicrophoneError::VoiceActivity(error).into())
        })
    }

    /// Uses `vad` instead of the bundled Silero model, for both endpointing
    /// and the wake-word gate. A panic while it scores fails the recording
    /// in progress, or drops the held stream when idle; a panic in its
    /// `reset` ends the microphone.
    pub fn load_with<V>(self, vad: V) -> Result<PreparedMicrophone<V>>
    where
        V: VoiceActivityDetector + Send + 'static,
    {
        self.prepare(|| Ok(vad))
    }

    /// Cheap checks first, so a model loads only once they pass.
    fn prepare<V>(self, vad: impl FnOnce() -> Result<V>) -> Result<PreparedMicrophone<V>> {
        limit::maximum_samples(self.maximum_duration)?;
        Ok(PreparedMicrophone {
            maximum_duration: self.maximum_duration,
            input: self.input,
            idle: self.idle,
            vad: vad()?,
        })
    }
}

/// A microphone whose fallible setup is done. Opening it starts the worker
/// thread, which opens the input device.
pub struct PreparedMicrophone<V = SileroVad> {
    maximum_duration: Duration,
    input: Option<InputDeviceId>,
    idle: IdleStream,
    vad: V,
}

impl<V: VoiceActivityDetector + Send + 'static> PreparedMicrophone<V> {
    pub fn open(self) -> Result<Microphone> {
        let commands = worker::spawn(self.maximum_duration, self.input, self.idle, self.vad)?;
        Ok(Microphone { commands })
    }

    pub fn open_with_wake_word<W>(self, detector: W) -> Result<(Microphone, WakeWordMonitor)>
    where
        W: WakeWordDetector + Send + 'static,
    {
        let (commands, events) = worker::spawn_with_wake_word(
            self.maximum_duration,
            self.input,
            self.idle,
            self.vad,
            detector,
        )?;
        let monitor = WakeWordMonitor::new(commands.clone(), events);
        Ok((Microphone { commands }, monitor))
    }
}

impl AudioCapture for Microphone {
    fn start(
        &self,
        mode: RecordingMode,
        source: Source,
        stop: StopSignal,
    ) -> Result<RecordingReader> {
        let Source::Microphone(_inputs) = source else {
            return Err(CaptureError::PushedUnsupported);
        };
        let (mut recording, writer) = recording::ends();
        self.commands
            .send(worker::Command::Record(worker::RecordCommand {
                mode,
                writer,
                stop,
            }))
            .map_err(|_command| MicrophoneError::WorkerGone)?;
        // Waits for the worker to call `begin`, so the return marks when
        // capture started. With the stream already playing, that takes one
        // round trip. Otherwise the device starts first, or the stream opens.
        recording.wait_for_capture()?;
        Ok(recording)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::LengthError;

    #[test]
    fn a_maximum_duration_over_the_limit_is_refused_before_opening() {
        let outcome = Microphone::builder()
            .maximum_duration(crate::audio::MAX_RECORDING_DURATION + Duration::from_secs(1))
            .open();

        assert!(matches!(
            outcome,
            Err(CaptureError::Length(LengthError::InvalidDuration { .. }))
        ));
    }
}
