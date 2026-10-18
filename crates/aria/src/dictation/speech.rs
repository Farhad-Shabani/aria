//! Whether a recording held speech. A decoder can name words in silence,
//! and nothing in the words tells them apart, so the audio decides.

use std::time::Duration;

use aria_kit::audio::{canonical_duration, pcm16_from_f32};
use aria_kit::voice_activity::{SPEECH_THRESHOLD, VoiceActivityDetector, VoiceActivityError};

use crate::audio::{CaptureError, RecordingReader};

/// Speech a recording must hold before its words are trusted. Silent and
/// noise-only recordings measure 0 ms; a lone short word, 290 ms or more.
const MIN_SPEECH: Duration = Duration::from_millis(100);

/// Adds up the speech a detector finds in a stream read in pieces. A
/// piece's unfinished frame waits for the next one, so uneven reads score
/// like one read at once. A final unfinished frame is never scored.
pub(super) struct Tally<V> {
    detector: V,
    frame: Vec<i16>,
    speech_samples: u64,
}

impl<V: VoiceActivityDetector> Tally<V> {
    pub(super) fn new(detector: V) -> Self {
        Self {
            detector,
            frame: Vec::with_capacity(V::FRAME_SAMPLES),
            speech_samples: 0,
        }
    }

    fn feed(&mut self, samples: &[f32]) -> Result<(), VoiceActivityError> {
        for &sample in samples {
            self.frame.push(pcm16_from_f32(sample));
            if self.frame.len() < V::FRAME_SAMPLES {
                continue;
            }
            let probability = self.detector.probability(&self.frame);
            self.frame.clear();
            if probability? >= SPEECH_THRESHOLD {
                self.speech_samples += V::FRAME_SAMPLES as u64;
            }
        }
        Ok(())
    }

    fn speech(&self) -> Duration {
        canonical_duration(self.speech_samples)
    }

    fn reset(&mut self) {
        self.detector.reset();
        self.frame.clear();
        self.speech_samples = 0;
    }
}

/// Scans one recording for speech on a fork of it, so it hears every
/// sample however little of it the decoder keeps.
pub(super) struct Scan<'a, V> {
    /// `None` once speech is heard, or when there is no detector to judge by.
    listening: Option<(&'a mut Tally<V>, RecordingReader)>,
}

impl<'a, V: VoiceActivityDetector> Scan<'a, V> {
    /// Opens before anything reads `recording`, so the fork starts where it
    /// does. Without a detector, every recording counts as speech.
    pub(super) fn new(tally: Option<&'a mut Tally<V>>, recording: &mut RecordingReader) -> Self {
        let listening = tally.map(|tally| {
            tally.reset();
            (tally, recording.fork_window())
        });
        Self { listening }
    }

    pub(super) fn heard(&self) -> bool {
        self.listening.is_none()
    }

    /// Scores the recording up to stream `position`, and no further: audio past
    /// it may be speech the words so far were not heard from.
    pub(super) fn hear_through(&mut self, position: u64) {
        self.hear(|fork| fork.fill_through(position).map(drop), position);
    }

    /// Scores the rest of the recording, waiting for capture to end.
    pub(super) fn hear_rest(&mut self) {
        self.hear(RecordingReader::fill, u64::MAX);
    }

    fn hear(
        &mut self,
        fill: impl FnOnce(&mut RecordingReader) -> Result<(), CaptureError>,
        position: u64,
    ) {
        let Some((tally, fork)) = self.listening.as_mut() else {
            return;
        };
        let filled = fill(fork);
        let end = position.min(fork.held_to());
        let heard = match filled {
            Ok(()) => match tally.feed(fork.span(fork.held_from()..end)) {
                Ok(()) => tally.speech() >= MIN_SPEECH,
                Err(error) => unjudged(error),
            },
            Err(error) => unjudged(error),
        };
        fork.release_before(end);
        if heard {
            self.listening = None;
        }
    }
}

/// A recording that cannot be judged counts as speech: losing words is
/// worse than letting a guess through.
fn unjudged(error: impl std::fmt::Display) -> bool {
    tracing::warn!(%error, "the recording could not be judged for speech; its words pass");
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Hears speech in any frame that is not silent.
    pub(crate) struct Loud;

    impl VoiceActivityDetector for Loud {
        const FRAME_SAMPLES: usize = 4;

        fn probability(&mut self, frame: &[i16]) -> Result<f32, VoiceActivityError> {
            Ok(if frame.iter().any(|&sample| sample != 0) {
                1.0
            } else {
                0.0
            })
        }
    }

    const LOUD: f32 = 0.5;

    #[test]
    fn only_frames_the_detector_calls_speech_count() {
        let mut tally = Tally::new(Loud);

        tally
            .feed(&[0.0, 0.0, 0.0, 0.0, LOUD, 0.0, 0.0, 0.0])
            .unwrap();

        assert_eq!(tally.speech_samples, 4);
    }

    #[test]
    fn uneven_pieces_score_like_one_read() {
        let stream = [0.0, 0.0, 0.0, LOUD, 0.0, 0.0, LOUD, 0.0, 0.0, 0.0];
        let mut whole = Tally::new(Loud);
        whole.feed(&stream).unwrap();

        let mut pieces = Tally::new(Loud);
        for piece in [&stream[..3], &stream[3..5], &stream[5..]] {
            pieces.feed(piece).unwrap();
        }

        assert_eq!(pieces.speech_samples, 8);
        assert_eq!(pieces.speech_samples, whole.speech_samples);
    }

    #[test]
    fn an_unfinished_frame_is_not_scored() {
        let mut tally = Tally::new(Loud);

        tally.feed(&[LOUD, LOUD, LOUD]).unwrap();

        assert_eq!(tally.speech_samples, 0);
    }

    #[test]
    fn reset_forgets_the_last_stream() {
        let mut tally = Tally::new(Loud);
        tally.feed(&[LOUD, 0.0, 0.0]).unwrap();

        tally.reset();
        tally.feed(&[0.0, 0.0, 0.0, 0.0]).unwrap();

        assert_eq!(tally.speech_samples, 0);
    }
}
