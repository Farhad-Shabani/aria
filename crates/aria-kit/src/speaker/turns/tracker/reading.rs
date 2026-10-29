//! What one check's similarity to the owner's voiceprint says, for how much
//! speech it scored. A short window scores lower and spreads wider, for the
//! owner and other voices alike, so one fixed bar cannot read every length.
//!
//! Fitted for the WeSpeaker ResNet34-LM embedder by `aria-bench
//! own-voice-scores` on LibriSpeech dev-clean and dev-other: 59 speakers,
//! each enrolled from one chapter and heard in the openings of utterances
//! from their others, against every enrolled speaker of the same gender.
//! test-clean, which the own-voice corpus is read from, reproduces every bar
//! within 0.02.

use std::time::Duration;

use crate::audio::{CANONICAL_SAMPLE_RATE, canonical_samples};

/// Under this much speech a score says nothing: on half a second the owner
/// and other voices share an equal error rate of 20–31%.
pub(super) const LEAST_READ_SAMPLES: usize = canonical_samples(Duration::from_secs(1)) as usize;

/// How a check's score reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reading {
    Owner,
    Other,
    /// Neither, yet: more speech decides.
    Pending,
}

struct Bars {
    seconds: f32,
    /// Lets 1% of other voices through.
    accept: f32,
    /// Turns away 1% of the owner's voice, or at most `accept`.
    doubt: f32,
}

/// The bars by seconds of speech, from [`LEAST_READ_SAMPLES`] on.
const FITTED: [Bars; 5] = [
    Bars {
        seconds: 1.0,
        accept: 0.346,
        doubt: 0.179,
    },
    Bars {
        seconds: 1.5,
        accept: 0.370,
        doubt: 0.300,
    },
    Bars {
        seconds: 2.0,
        accept: 0.385,
        doubt: 0.385,
    },
    Bars {
        seconds: 2.5,
        accept: 0.395,
        doubt: 0.395,
    },
    Bars {
        seconds: 3.0,
        accept: 0.401,
        doubt: 0.401,
    },
];

/// Reads `similarity`, scored on `scored` samples of speech. Between rows the
/// bars run straight; past the last they stay.
pub(super) fn read(similarity: f32, scored: usize) -> Reading {
    if scored < LEAST_READ_SAMPLES {
        return Reading::Pending;
    }
    let (accept, doubt) = bars(scored as f32 / CANONICAL_SAMPLE_RATE as f32);
    if similarity >= accept {
        Reading::Owner
    } else if similarity < doubt {
        Reading::Other
    } else {
        Reading::Pending
    }
}

fn bars(seconds: f32) -> (f32, f32) {
    for pair in FITTED.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        if seconds < after.seconds {
            let along = (seconds - before.seconds) / (after.seconds - before.seconds);
            let between = |from: f32, to: f32| from + (to - from) * along;
            return (
                between(before.accept, after.accept),
                between(before.doubt, after.doubt),
            );
        }
    }
    let last = &FITTED[FITTED.len() - 1];
    (last.accept, last.doubt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(seconds: f32) -> usize {
        (seconds * CANONICAL_SAMPLE_RATE as f32) as usize
    }

    #[test]
    fn under_a_second_no_score_decides() {
        for similarity in [-1.0, 0.0, 0.99] {
            assert_eq!(read(similarity, samples(0.9)), Reading::Pending);
        }
    }

    #[test]
    fn each_fitted_length_reads_by_its_own_bars() {
        for row in &FITTED {
            let scored = samples(row.seconds);
            assert_eq!(read(row.accept, scored), Reading::Owner);
            assert_eq!(read(row.doubt.next_down(), scored), Reading::Other);
        }
        assert_eq!(read(0.25, samples(1.5)), Reading::Other);
        assert_eq!(read(0.32, samples(1.5)), Reading::Pending);
    }

    #[test]
    fn a_length_between_rows_reads_by_bars_between_theirs() {
        assert_eq!(read(0.357, samples(1.25)), Reading::Pending);
        assert_eq!(read(0.359, samples(1.25)), Reading::Owner);
    }
}
