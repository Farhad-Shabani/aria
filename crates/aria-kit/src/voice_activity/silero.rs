//! The Silero model behind [`VoiceActivityDetector`]. It is bundled, so any
//! capture source can ask without a configured path.

use std::ops::Range;

use ::silero::{SampleRate, Session, StreamState};

use super::{SPEECH_THRESHOLD, VoiceActivityDetector, VoiceActivityError};
use crate::audio::f32_from_pcm16;

const FRAME_SAMPLES: usize = 512;
/// What counts as speech when the question is *when* somebody spoke rather
/// than what to feed a model. A timeline pays for a frame it skips and barely
/// pays for one it takes too many of, so it listens harder than the gate.
/// Measured on AMI ES2004a: 0.5 leaves 35% of the speech unaccounted for, 0.1
/// leaves 27%, and below 0.1 noise starts arriving with a speaker's name on it.
const TIMELINE_THRESHOLD: f32 = 0.1;
/// How far a speech run reaches past the frames that scored for it. The
/// detector trims quiet onsets and trailing consonants, which is exactly
/// where a word gets cut in half.
const EDGE_PAD_SAMPLES: usize = 1_600;

pub struct SileroVad {
    session: Session,
    stream: StreamState,
    frame: [f32; FRAME_SAMPLES],
}

impl SileroVad {
    /// Infers one silent frame, so a broken model fails here, not per window.
    pub fn load() -> Result<Self, VoiceActivityError> {
        let session =
            Session::bundled().map_err(|source| VoiceActivityError::LoadModel(Box::new(source)))?;
        let mut vad = Self {
            session,
            stream: StreamState::new(SampleRate::Rate16k),
            frame: [0.0; FRAME_SAMPLES],
        };
        vad.scores(&[0.0; FRAME_SAMPLES])?;
        vad.stream.reset();
        Ok(vad)
    }

    /// Where the voice is inside a window, as sample ranges — the answer a
    /// caller wants when it needs to know *when* somebody spoke rather than
    /// only what the speech sounded like.
    pub fn speech_regions(
        &mut self,
        window: &[f32],
    ) -> Result<Vec<Range<usize>>, VoiceActivityError> {
        let scores = self.scores(window)?;
        Ok(regions(&scores, window))
    }

    /// Both answers about a window from one pass over the model: where the
    /// voice is, and the samples of it worth feeding a model. The second
    /// keeps the strict threshold, since padding a run there would only mix
    /// silence back into what it hands on.
    pub fn speech(&mut self, window: &[f32]) -> Result<Speech, VoiceActivityError> {
        let scores = self.scores(window)?;
        Ok(Speech {
            regions: regions(&scores, window),
            samples: runs_at(&scores, SPEECH_THRESHOLD)
                .into_iter()
                .flat_map(|run| window[run].iter().copied())
                .collect(),
        })
    }

    /// Each frame's speech probability, with the samples it covers. The last
    /// frame is padded to length rather than dropped, because it sits
    /// exactly where a word gets cut off.
    fn scores(&mut self, window: &[f32]) -> Result<Vec<(Range<usize>, f32)>, VoiceActivityError> {
        let mut scores = Vec::with_capacity(window.len().div_ceil(FRAME_SAMPLES));
        for (index, frame) in window.chunks(FRAME_SAMPLES).enumerate() {
            let at = index * FRAME_SAMPLES;
            self.frame[..frame.len()].copy_from_slice(frame);
            self.frame[frame.len()..].fill(0.0);
            let probability = self
                .session
                .infer_chunk(&mut self.stream, &self.frame)
                .map_err(|source| VoiceActivityError::Inference(Box::new(source)))?;
            scores.push((at..at + frame.len(), probability));
        }
        Ok(scores)
    }
}

/// What one pass over a window found.
pub struct Speech {
    pub regions: Vec<Range<usize>>,
    pub samples: Vec<f32>,
}

fn regions(scores: &[(Range<usize>, f32)], window: &[f32]) -> Vec<Range<usize>> {
    widen(&runs_at(scores, TIMELINE_THRESHOLD), window.len())
}

/// The unbroken stretches scored at or above `threshold`; adjacent frames
/// merge into one run.
fn runs_at(scores: &[(Range<usize>, f32)], threshold: f32) -> Vec<Range<usize>> {
    let mut runs: Vec<Range<usize>> = Vec::new();
    for (frame, probability) in scores {
        if *probability < threshold {
            continue;
        }
        match runs.last_mut() {
            Some(open) if open.end == frame.start => open.end = frame.end,
            _ => runs.push(frame.clone()),
        }
    }
    runs
}

impl VoiceActivityDetector for SileroVad {
    const FRAME_SAMPLES: usize = FRAME_SAMPLES;

    fn reset(&mut self) {
        self.stream.reset();
    }

    fn probability(&mut self, samples: &[i16]) -> Result<f32, VoiceActivityError> {
        if samples.len() != FRAME_SAMPLES {
            return Err(VoiceActivityError::InvalidFrameLength {
                actual_samples: samples.len(),
                expected_samples: FRAME_SAMPLES,
            });
        }
        for (destination, &sample) in self.frame.iter_mut().zip(samples) {
            *destination = f32_from_pcm16(sample);
        }
        let probability = self
            .session
            .infer_chunk(&mut self.stream, &self.frame)
            .map_err(|source| VoiceActivityError::Inference(Box::new(source)))?;
        if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
            return Err(VoiceActivityError::InvalidProbability(probability));
        }
        Ok(probability)
    }
}

/// Grows each run by `EDGE_PAD_SAMPLES`, but never past the midpoint of the
/// silence separating it from its neighbour. Two runs that grew into one
/// would carry a single speaker's name across the pause between them — the
/// likeliest place for the speaker to change.
fn widen(runs: &[Range<usize>], window_samples: usize) -> Vec<Range<usize>> {
    runs.iter()
        .enumerate()
        .map(|(index, run)| {
            let behind = match index.checked_sub(1).map(|prior| &runs[prior]) {
                Some(prior) => (run.start - prior.end) / 2,
                None => run.start,
            };
            let ahead = match runs.get(index + 1) {
                Some(next) => (next.start - run.end) / 2,
                None => window_samples - run.end,
            };
            (run.start - behind.min(EDGE_PAD_SAMPLES))..(run.end + ahead.min(EDGE_PAD_SAMPLES))
        })
        .collect()
}

#[cfg(test)]
mod widen_tests {
    use super::*;

    fn runs<const N: usize>(bounds: [(usize, usize); N]) -> Vec<Range<usize>> {
        bounds.into_iter().map(|(start, end)| start..end).collect()
    }

    #[test]
    fn a_lone_run_grows_by_the_full_pad() {
        let widened = widen(&runs([(10_000, 20_000)]), 40_000);
        let expected = runs([(10_000 - EDGE_PAD_SAMPLES, 20_000 + EDGE_PAD_SAMPLES)]);
        assert_eq!(widened, expected);
    }

    #[test]
    fn two_close_runs_split_the_silence_between_them_and_stay_apart() {
        let widened = widen(&runs([(0, 1_000), (1_400, 2_000)]), 10_000);
        assert_eq!(widened[0].end, 1_200);
        assert_eq!(widened[1].start, 1_200);
    }

    #[test]
    fn a_run_never_grows_past_the_window() {
        assert_eq!(widen(&runs([(0, 1_000)]), 1_200), runs([(0, 1_200)]));
    }
}
