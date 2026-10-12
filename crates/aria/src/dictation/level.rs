//! How loud the microphone is, while a press records.
//!
//! The pill draws a meter from this, so it is a picture rather than a
//! measurement: a level that arrives late is worth less than one that
//! arrives, and a dropped one costs a frame.

use std::sync::Arc;
use std::thread;

use crate::audio::RecordingReader;

use super::{DictationEventEmitter, ExecutionControl, RecordingId};

/// One frame of the meter, in samples at the canonical rate: fast enough to
/// look live, slow enough that a stalled subscriber does not fill its queue
/// between two stages.
const FRAME_SAMPLES: u64 = 800;

/// Quietest sound that moves the meter off its floor, in RMS. Below this is
/// a room, not a voice.
const NOISE_FLOOR: f32 = 0.002;

/// Loud speech close to the mic. Anything above pins the meter. Tuned by
/// eye against a built-in MacBook mic; a hot interface may want it higher.
const LOUD: f32 = 0.12;

/// One press worth of levels, on a thread of its own. Ends when capture
/// does, or when the press is cancelled.
pub(crate) struct Meter {
    thread: Option<thread::JoinHandle<()>>,
}

impl Meter {
    /// Reads its own fork of the recording, so it neither waits on the
    /// transcriber nor pins the audio the transcriber has finished with.
    pub(crate) fn watching(
        recording: &RecordingReader,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Self {
        let mut fork = recording.fork();
        let id = control.recording_id();
        let control = Arc::clone(control);
        let events = events.clone();
        let thread = thread::Builder::new()
            .name("aria-dictation-level".to_owned())
            .spawn(move || {
                let mut frame_end = fork.held_to();
                while !control.is_cancelled() {
                    frame_end += FRAME_SAMPLES;
                    // Blocks for one frame of audio, or until capture ends.
                    if fork.fill_through(frame_end).is_err() || fork.reached_end() {
                        return;
                    }
                    let level = loudness(fork.samples());
                    // Read, then forget: a meter holding its window would grow
                    // without bound.
                    let end = fork.held_to();
                    fork.release_before(end);
                    events.emit(MicLevel {
                        recording: id,
                        level,
                    });
                }
            })
            .inspect_err(|error| tracing::warn!(%error, "the level meter did not start"))
            .ok();
        Self { thread }
    }
}

impl Drop for Meter {
    /// The press is over, so the last level is already stale. Joining keeps
    /// the thread from outliving the recording it reads.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How loud the microphone is right now, from silent to pinned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MicLevel {
    pub recording: RecordingId,
    /// 0 at the noise floor, 1 at loud speech.
    pub level: f32,
}

/// Root mean square, mapped onto the range a meter draws. The square root
/// lifts ordinary speech off the floor, the way an ear hears it, without
/// the swing of decibels.
fn loudness(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|sample| sample * sample).sum();
    let rms = (sum / samples.len() as f32).sqrt();
    ((rms - NOISE_FLOOR) / (LOUD - NOISE_FLOOR))
        .clamp(0.0, 1.0)
        .sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_room_sits_on_the_floor() {
        assert_eq!(loudness(&[]), 0.0);
        assert_eq!(loudness(&[0.0; 64]), 0.0);
        assert_eq!(loudness(&[NOISE_FLOOR / 2.0; 64]), 0.0);
    }

    #[test]
    fn speech_moves_the_meter_without_pinning_it() {
        let level = loudness(&[LOUD / 2.0; 64]);
        assert!(level > 0.0 && level < 1.0, "half-loud speech reads {level}");
    }

    #[test]
    fn anything_louder_than_speech_pins_it() {
        assert_eq!(loudness(&[1.0; 64]), 1.0);
        assert_eq!(loudness(&[-1.0; 64]), 1.0);
    }
}
