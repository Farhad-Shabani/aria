//! What one check's similarity to the owner's voiceprint says, for how much
//! speech it scored. A short window scores lower and spreads wider, for the
//! owner and other voices alike, so one fixed bar cannot read every length.
//!
//! Fitted for the WeSpeaker ResNet34-LM embedder by `aria-bench
//! own-voice-scores` on LibriSpeech dev-clean and dev-other: 59 speakers,
//! each enrolled from one chapter and heard from where speech starts in
//! utterances from their others, against every enrolled speaker of the same
//! gender. test-clean, which the own-voice corpus is read from, reproduces
//! every accept bar within 0.02 and every doubt bar within 0.05.

use std::time::Duration;

use crate::audio::{CANONICAL_SAMPLE_RATE, canonical_samples};

/// No bars were fitted on less speech than this.
pub(super) const LEAST_READ_SAMPLES: usize = canonical_samples(Duration::from_millis(500)) as usize;

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
const FITTED: [Bars; 6] = [
    Bars {
        seconds: 0.5,
        accept: 0.313,
        doubt: 0.156,
    },
    Bars {
        seconds: 1.0,
        accept: 0.358,
        doubt: 0.232,
    },
    Bars {
        seconds: 1.5,
        accept: 0.379,
        doubt: 0.346,
    },
    Bars {
        seconds: 2.0,
        accept: 0.390,
        doubt: 0.390,
    },
    Bars {
        seconds: 2.5,
        accept: 0.398,
        doubt: 0.398,
    },
    Bars {
        seconds: 3.0,
        accept: 0.403,
        doubt: 0.403,
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
    fn under_half_a_second_no_score_decides() {
        for similarity in [-1.0, 0.0, 0.99] {
            assert_eq!(read(similarity, samples(0.4)), Reading::Pending);
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
        assert_eq!(read(0.36, samples(1.5)), Reading::Pending);
    }

    #[test]
    fn a_length_between_rows_reads_by_bars_between_theirs() {
        assert_eq!(read(0.367, samples(1.25)), Reading::Pending);
        assert_eq!(read(0.369, samples(1.25)), Reading::Owner);
    }
}
