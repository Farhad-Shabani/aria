//! How loud a recording is right now, for a meter to draw.
//!
//! A meter is a picture rather than a measurement: a level that arrives late
//! is worth less than one that arrives, and a dropped one costs a frame.

use std::thread;

use aria_kit::audio::RecordingReader;

/// One frame of the meter, in samples at the canonical rate: fast enough to
/// look live, slow enough that a stalled subscriber does not fill its queue
/// between two stages.
const FRAME_SAMPLES: u64 = 800;

/// Quietest sound that moves the meter off its floor, in RMS. Below this is
/// a room, not a voice.
const NOISE_FLOOR: f32 = 0.002;

/// Where a meter pins, in RMS, by what it hears.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Scale {
    /// Loud speech close to the mic. Tuned by eye against a built-in MacBook
    /// mic; a hot interface may want it higher.
    Microphone,
    /// Mastered media plays near full scale: speech at a normal volume reads
    /// about 0.16 RMS, so the microphone's scale would pin on most frames.
    SystemAudio,
}

impl Scale {
    fn loud(self) -> f32 {
        match self {
            Self::Microphone => 0.12,
            Self::SystemAudio => 0.35,
        }
    }
}

/// A recording's levels, on a thread of its own. Ends when capture does, or
/// once `should_stop` says so.
pub(crate) struct Meter {
    thread: Option<thread::JoinHandle<()>>,
}

impl Meter {
    /// Reads its own fork of the recording, so it neither waits on the
    /// recognizer nor pins the audio the recognizer has finished with.
    pub(crate) fn watching(
        recording: &RecordingReader,
        scale: Scale,
        thread_name: &str,
        should_stop: impl Fn() -> bool + Send + 'static,
        emit: impl Fn(f32) + Send + 'static,
    ) -> Self {
        let mut fork = recording.fork();
        let thread = thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                while !should_stop() {
                    let Some(level) = next_level(&mut fork, scale) else {
                        return;
                    };
                    emit(level);
                }
            })
            .inspect_err(|error| tracing::warn!(%error, "the level meter did not start"))
            .ok();
        Self { thread }
    }
}

impl Drop for Meter {
    /// The recording is over, so the last level is already stale. Joining
    /// keeps the thread from outliving the recording it reads.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("the level meter thread panicked; levels stopped early");
        }
    }
}

/// Waits for one frame past what the fork holds, then measures and forgets
/// it. `None` once capture ends.
fn next_level(fork: &mut RecordingReader, scale: Scale) -> Option<f32> {
    let frame_end = fork.held_to() + FRAME_SAMPLES;
    // Blocks for one frame of audio, or until capture ends.
    if fork.fill_through(frame_end).is_err() || fork.reached_end() {
        return None;
    }
    let level = loudness(fork.samples(), scale);
    // Read, then forget: a meter holding its window would grow without bound.
    let end = fork.held_to();
    fork.release_before(end);
    Some(level)
}

/// Root mean square, mapped onto the range a meter draws. The square root
/// lifts ordinary speech off the floor, the way an ear hears it, without
/// the swing of decibels.
fn loudness(samples: &[f32], scale: Scale) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|sample| sample * sample).sum();
    let rms = (sum / samples.len() as f32).sqrt();
    ((rms - NOISE_FLOOR) / (scale.loud() - NOISE_FLOOR))
        .clamp(0.0, 1.0)
        .sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_kit::audio::Recording;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    #[test]
    fn a_meter_reports_each_frame_until_capture_ends() {
        let (reader, mut writer) = Recording::open();
        let (sent, levels) = channel();
        let meter = Meter::watching(
            &reader,
            Scale::Microphone,
            "aria-test-level",
            || false,
            move |level| {
                let _ = sent.send(level);
            },
        );
        writer.push(&[1.0; FRAME_SAMPLES as usize]);
        assert_eq!(levels.recv_timeout(Duration::from_secs(5)), Ok(1.0));
        writer.push(&[0.0; FRAME_SAMPLES as usize]);
        assert_eq!(levels.recv_timeout(Duration::from_secs(5)), Ok(0.0));
        writer.finish();
        // Returns only once the thread has seen capture end.
        drop(meter);
        assert!(levels.try_recv().is_err());
    }

    #[test]
    fn a_large_write_leaves_no_empty_frames() {
        let (reader, mut writer) = Recording::open();
        let mut fork = reader.fork();
        writer.push(&[1.0; 16_000]);
        assert_eq!(next_level(&mut fork, Scale::Microphone), Some(1.0));
        writer.push(&[1.0; FRAME_SAMPLES as usize]);
        assert_eq!(next_level(&mut fork, Scale::Microphone), Some(1.0));
    }

    #[test]
    fn a_quiet_room_sits_on_the_floor() {
        let mic = Scale::Microphone;
        assert_eq!(loudness(&[], mic), 0.0);
        assert_eq!(loudness(&[0.0; 64], mic), 0.0);
        assert_eq!(loudness(&[NOISE_FLOOR / 2.0; 64], mic), 0.0);
    }

    #[test]
    fn speech_moves_the_meter_without_pinning_it() {
        let mic = Scale::Microphone;
        let level = loudness(&[mic.loud() / 2.0; 64], mic);
        assert!(level > 0.0 && level < 1.0, "half-loud speech reads {level}");
    }

    #[test]
    fn anything_louder_than_speech_pins_it() {
        assert_eq!(loudness(&[1.0; 64], Scale::Microphone), 1.0);
        assert_eq!(loudness(&[-1.0; 64], Scale::Microphone), 1.0);
    }

    #[test]
    fn media_speech_moves_the_system_audio_meter_without_pinning_it() {
        // Median RMS of speech played at a normal volume.
        let level = loudness(&[0.16; 64], Scale::SystemAudio);
        assert!(level > 0.5 && level < 0.9, "media speech reads {level}");
    }
}
