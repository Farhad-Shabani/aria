//! Whether a recording held speech. A decoder can name words in silence
//! that look like real ones, so the audio decides.

use std::time::Duration;

use aria_kit::audio::{canonical_duration, pcm16_from_f32};
use aria_kit::voice_activity::{SPEECH_THRESHOLD, VoiceActivityDetector, VoiceActivityError};

use crate::audio::{CaptureError, RecordingReader};
use crate::primitives::error_chain;

/// Speech a recording must hold before its words are trusted. Silent and
/// noise-only recordings measure 0 ms; a lone short word, 290 ms or more.
const MIN_SPEECH: Duration = Duration::from_millis(100);

/// Adds up the speech a detector finds in a stream read in pieces. An
/// unfinished frame waits for the next piece; a final one is never scored.
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

/// Scans a recording for speech on its own fork, so it hears every sample
/// whatever the decoder keeps.
pub(super) struct Scan<'a, V> {
    /// `None` once speech is heard, or when there is no detector to judge by.
    listening: Option<(&'a mut Tally<V>, RecordingReader)>,
}

impl<'a, V: VoiceActivityDetector> Scan<'a, V> {
    /// Call before anything reads `recording`, so the fork starts where it does.
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

    /// Scores up to stream `position` only. Speech after it did not produce
    /// the words read so far.
    pub(super) fn hear_through(&mut self, position: u64) {
        self.hear(|fork| fork.fill_through(position).map(drop), position);
    }

    /// Scores the rest of the recording, waiting for capture to end.
    pub(super) fn hear_rest(&mut self) {
        self.hear(RecordingReader::fill, u64::MAX);
    }

    /// Scores the rest of the recording and says whether it held speech.
    pub(super) fn held_speech(mut self) -> bool {
        self.hear_rest();
        let Some((tally, _)) = &self.listening else {
            return true;
        };
        tracing::info!(
            speech_ms = tally.speech().as_millis(),
            min_speech_ms = MIN_SPEECH.as_millis(),
            "too little speech in the recording; its words are dropped"
        );
        false
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
                Err(error) => unjudged(&error),
            },
            Err(error) => unjudged(&error),
        };
        fork.release_before(end);
        if heard {
            self.listening = None;
        }
    }
}

/// A recording that cannot be judged counts as speech: losing words is
/// worse than letting a guess through.
fn unjudged(error: &dyn std::error::Error) -> bool {
    tracing::warn!(
        error = %error_chain(error),
        "the recording could not be judged for speech; its words pass"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation::testing::Loud;

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
