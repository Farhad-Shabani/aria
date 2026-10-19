use crate::audio::{SilenceDuration, canonical_samples};

use super::{SPEECH_THRESHOLD, VoiceActivityDetector, VoiceActivityError};

/// At or below this a frame starts the trailing-silence clock. Between it and
/// [`SPEECH_THRESHOLD`] the clock neither starts nor resets, so a soft
/// syllable does not end an utterance.
const SPEECH_END_THRESHOLD: f32 = 0.35;

/// A speech boundary, in canonical samples counted from the first
/// [`Endpointer::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    /// The first sample of the frame where speech opened.
    Start(u64),
    /// The end of the frame where the trailing silence completed, so the
    /// utterance before it includes that silence.
    End(u64),
}

/// Finds utterances in a stream. Speech opens on a frame scored at or above
/// [`SPEECH_THRESHOLD`] and closes once silence has lasted `trailing_silence`.
/// After an `End` it keeps listening, so the next `Start` opens the next
/// utterance. An utterance still open when input ends gets no `End`.
///
/// Positions count from the first push. To land them on a
/// [`RecordingReader`](crate::audio::RecordingReader) that did not start at
/// zero, add its `held_from()` at that push. `Start` has no lead-in: open an
/// utterance [`LEAD_IN_FRAMES`](super::LEAD_IN_FRAMES) frames earlier to cut
/// as the microphone does.
///
/// ```
/// # use aria_kit::audio::SilenceDuration;
/// # use aria_kit::voice_activity::{Boundary, Endpointer, VoiceActivityDetector, VoiceActivityError};
/// fn utterances<V: VoiceActivityDetector>(
///     vad: &mut V,
///     batches: &[Vec<i16>],
/// ) -> Result<(), VoiceActivityError> {
///     // `&mut V` leaves the loaded detector with the caller for the next run.
///     let mut endpointer = Endpointer::new(vad, SilenceDuration::default());
///     for batch in batches {
///         for boundary in endpointer.push(batch)? {
///             match boundary {
///                 Boundary::Start(at) => println!("speech from sample {at}"),
///                 Boundary::End(at) => println!("utterance ends at sample {at}"),
///             }
///         }
///     }
///     Ok(())
/// }
/// ```
pub struct Endpointer<V> {
    vad: V,
    trailing_silence_samples: u64,
    frame: Vec<i16>,
    scored_samples: u64,
    speaking: bool,
    silence_since: Option<u64>,
}

impl<V: VoiceActivityDetector> Endpointer<V> {
    /// Resets `vad`, so state from earlier audio does not leak in.
    pub fn new(mut vad: V, trailing_silence: SilenceDuration) -> Self {
        // A zero-sample frame never fills, so `push` would never return.
        const { assert!(V::FRAME_SAMPLES > 0) };
        vad.reset();
        Self {
            vad,
            trailing_silence_samples: canonical_samples(trailing_silence.get()),
            frame: Vec::with_capacity(V::FRAME_SAMPLES),
            scored_samples: 0,
            speaking: false,
            silence_since: None,
        }
    }

    /// A partial frame waits for the next push.
    pub fn push(&mut self, mut samples: &[i16]) -> Result<Vec<Boundary>, VoiceActivityError> {
        let mut boundaries = Vec::new();
        while !samples.is_empty() {
            let copied = (V::FRAME_SAMPLES - self.frame.len()).min(samples.len());
            self.frame.extend_from_slice(&samples[..copied]);
            samples = &samples[copied..];
            if self.frame.len() == V::FRAME_SAMPLES {
                boundaries.extend(self.score_frame()?);
                self.frame.clear();
            }
        }
        Ok(boundaries)
    }

    fn score_frame(&mut self) -> Result<Option<Boundary>, VoiceActivityError> {
        let probability = self.vad.probability(&self.frame)?;
        // Counted only once scored, so a push retried after an error keeps
        // its positions.
        let frame_start = self.scored_samples;
        self.scored_samples += V::FRAME_SAMPLES as u64;

        if !self.speaking {
            self.speaking = probability >= SPEECH_THRESHOLD;
            return Ok(self.speaking.then_some(Boundary::Start(frame_start)));
        }
        if probability >= SPEECH_THRESHOLD {
            self.silence_since = None;
        } else if probability <= SPEECH_END_THRESHOLD && self.silence_since.is_none() {
            self.silence_since = Some(frame_start);
        }
        let silence = self
            .silence_since
            .map_or(0, |since| self.scored_samples - since);
        if silence < self.trailing_silence_samples {
            return Ok(None);
        }
        self.speaking = false;
        self.silence_since = None;
        Ok(Some(Boundary::End(self.scored_samples)))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct Scripted {
        scores: Vec<f32>,
        calls: usize,
        resets: usize,
    }

    impl VoiceActivityDetector for Scripted {
        const FRAME_SAMPLES: usize = 160;

        fn reset(&mut self) {
            self.resets += 1;
        }

        fn probability(&mut self, _samples: &[i16]) -> Result<f32, VoiceActivityError> {
            let score = self.scores[self.calls];
            self.calls += 1;
            Ok(score)
        }
    }

    fn scripted(scores: &[f32]) -> Scripted {
        Scripted {
            scores: scores.to_vec(),
            calls: 0,
            resets: 0,
        }
    }

    const FRAME: u64 = Scripted::FRAME_SAMPLES as u64;

    fn ten_frames_of_silence() -> SilenceDuration {
        SilenceDuration::new(Duration::from_millis(100)).expect("the duration is in range")
    }

    fn boundaries(scores: &[f32], batch: usize) -> Vec<Boundary> {
        let mut endpointer = Endpointer::new(scripted(scores), ten_frames_of_silence());
        let samples = vec![0; scores.len() * Scripted::FRAME_SAMPLES];
        let mut found = Vec::new();
        for piece in samples.chunks(batch) {
            found.extend(endpointer.push(piece).expect("the scripted VAD scores"));
        }
        found
    }

    #[test]
    fn consecutive_utterances_each_get_a_start_and_an_end() {
        let mut scores = vec![0.0, 0.9, 0.9];
        scores.extend([0.0; 10]);
        scores.extend([0.9, 0.0]);
        let ten_frames = 10 * FRAME;

        for batch in [1, 100, 160, 1_024] {
            assert_eq!(
                boundaries(&scores, batch),
                [
                    Boundary::Start(FRAME),
                    Boundary::End(3 * FRAME + ten_frames),
                    Boundary::Start(13 * FRAME),
                ],
                "batches of {batch} samples"
            );
        }
    }

    #[test]
    fn a_score_between_the_thresholds_does_not_end_the_utterance() {
        let mut scores = vec![0.9];
        scores.extend([0.4; 20]);

        assert_eq!(boundaries(&scores, 160), [Boundary::Start(0)]);
    }

    #[test]
    fn a_new_endpointer_resets_its_detector() {
        let mut vad = scripted(&[]);

        Endpointer::new(&mut vad, ten_frames_of_silence());

        assert_eq!(vad.resets, 1);
    }
}
