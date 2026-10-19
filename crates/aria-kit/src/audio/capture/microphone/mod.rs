//! The default microphone: opened once, then recording one utterance at a time.

mod resampling;
mod stream;
mod transfer;
mod wake_word;
mod worker;

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::audio::recording;
use crate::audio::spool;
use crate::audio::{CaptureError, RecordingReader, StopSignal, Stopper};
use crate::primitives::private_fs;
use crate::voice_activity::SileroVad;
use crate::wake_word::WakeWordDetector;

use super::{AudioCapture, MicrophoneError, RecordingMode};

pub use stream::check_default_input_device;
pub use wake_word::{
    WakeWordControl, WakeWordEvent, WakeWordFailure, WakeWordMonitor, WakeWordSuspension,
};

const DEFAULT_MAXIMUM_DURATION: Duration = Duration::from_secs(60);

type Result<T> = std::result::Result<T, CaptureError>;

/// The default microphone, ready to record. `open()` chooses a private
/// per-user spool directory and a 60-second maximum duration; the builder
/// overrides either.
pub struct Microphone {
    commands: Sender<worker::Command>,
}

impl Microphone {
    pub fn open() -> Result<Self> {
        Self::builder().open()
    }

    pub fn builder() -> MicrophoneBuilder {
        MicrophoneBuilder {
            spool_dir: None,
            maximum_duration: DEFAULT_MAXIMUM_DURATION,
        }
    }

    /// Records one utterance. The recording fills in the background; hand the
    /// stopper to whatever should be able to end it early.
    // `&mut self`: one capture worker serves one recording at a time, so a
    // second concurrent record would silently queue behind the first.
    pub fn record(&mut self, mode: RecordingMode) -> Result<(RecordingReader, Stopper)> {
        let stopper = Stopper::new();
        let recording = self.start(mode, StopSignal::from(stopper.clone()))?;
        Ok((recording, stopper))
    }
}

pub struct MicrophoneBuilder {
    spool_dir: Option<PathBuf>,
    maximum_duration: Duration,
}

impl MicrophoneBuilder {
    /// Where the bounded temporary WAV lives while recording.
    pub fn spool_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.spool_dir = Some(directory.into());
        self
    }

    /// The most a single recording may hold, up to about 37 hours. The spool
    /// grows with it, about 115 MB per hour, and each reader holds what it
    /// has not released, about 230 MB per hour.
    pub fn maximum_duration(mut self, maximum_duration: Duration) -> Self {
        self.maximum_duration = maximum_duration;
        self
    }

    pub fn open(self) -> Result<Microphone> {
        self.load()?.open()
    }

    pub fn load(self) -> Result<PreparedMicrophone> {
        // Refused before any directory or model work, so a bad maximum never
        // hands out a microphone.
        spool::maximum_samples(self.maximum_duration)?;
        let spool_dir = match self.spool_dir {
            // A caller-chosen directory is theirs: no ownership or mode
            // enforcement on a layout this crate does not own.
            Some(directory) => directory,
            None => private_spool_dir()?,
        };
        let vad = SileroVad::load().map_err(MicrophoneError::VoiceActivity)?;
        Ok(PreparedMicrophone {
            spool_dir,
            maximum_duration: self.maximum_duration,
            vad,
        })
    }
}

/// A microphone whose fallible setup is done. Opening it starts the worker
/// thread, which opens the input device.
pub struct PreparedMicrophone {
    spool_dir: PathBuf,
    maximum_duration: Duration,
    vad: SileroVad,
}

impl PreparedMicrophone {
    pub fn open(self) -> Result<Microphone> {
        let commands = worker::spawn(self.spool_dir, self.maximum_duration, self.vad)?;
        Ok(Microphone { commands })
    }

    pub fn open_with_wake_word<W>(self, detector: W) -> Result<(Microphone, WakeWordMonitor)>
    where
        W: WakeWordDetector + Send + 'static,
    {
        let (commands, events) = worker::spawn_with_wake_word(
            self.spool_dir,
            self.maximum_duration,
            self.vad,
            detector,
        )?;
        let monitor = WakeWordMonitor::new(commands.clone(), events);
        Ok((Microphone { commands }, monitor))
    }
}

fn private_spool_dir() -> Result<PathBuf> {
    let directory =
        std::env::temp_dir().join(format!("aria-{}", rustix::process::getuid().as_raw()));
    private_fs::prepare_directory(&directory).map_err(MicrophoneError::SpoolDirectory)?;
    Ok(directory)
}

impl AudioCapture for Microphone {
    fn start(&self, mode: RecordingMode, stop: StopSignal) -> Result<RecordingReader> {
        let (mut recording, writer) = recording::ends();
        self.commands
            .send(worker::Command::Record(worker::RecordCommand {
                mode,
                writer,
                stop,
            }))
            .map_err(|_command| MicrophoneError::WorkerGone)?;
        // Waits for the worker to call `begin`, so the return marks when
        // capture started. With the stream already open that is one round
        // trip; otherwise the stream opens first.
        recording.wait_for_capture()?;
        Ok(recording)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::SpoolError;

    #[test]
    fn a_maximum_duration_over_the_limit_is_refused_before_opening() {
        let outcome = Microphone::builder()
            .maximum_duration(crate::audio::MAX_RECORDING_DURATION + Duration::from_secs(1))
            .open();

        assert!(matches!(
            outcome,
            Err(CaptureError::Spool(SpoolError::InvalidDuration { .. }))
        ));
    }
}
