//! Word-weighted label accuracy against the script's ground truth.

use std::collections::HashMap;

use aria::speaker::Label;

use crate::stt::words;

use crate::diarize::replay::{Dropped, Labeler, Segment};
use crate::diarize::synth::Scenario;
use crate::diarize::{SAMPLE_RATE, ScenarioReport};

pub(crate) struct Labeled {
    pub(crate) text: String,
    pub(crate) label: Option<Label>,
}

impl Scenario {
    /// Scores one heard scenario twice: the live labels as they streamed,
    /// and the final ones after the re-cluster.
    pub(crate) fn scored(
        &self,
        labeler: &Labeler,
        segments: &[Segment],
        dropped: &Dropped,
    ) -> ScenarioReport {
        let live: Vec<Labeled> = segments
            .iter()
            .map(|segment| Labeled {
                text: segment.text.clone(),
                label: segment.label.clone(),
            })
            .collect();
        let embeddings: Vec<Option<Vec<f32>>> = segments
            .iter()
            .map(|segment| segment.embedding.clone())
            .collect();
        // The same `settle` the listen sink runs, with a different rule for
        // openings: a slot nobody could embed and nobody labeled live opens an utterance, so
        // it takes the voice after it rather than the one before. Live labels
        // never stand in — they number their speakers differently.
        let mut settled = labeler.labeler().relabel(&embeddings);
        aria::speaker::settle(&mut settled, |index| segments[index].label.is_none());
        let finals: Vec<Labeled> = segments
            .iter()
            .zip(settled)
            .map(|(segment, label)| Labeled {
                text: segment.text.clone(),
                label,
            })
            .collect();

        ScenarioReport {
            name: self.name,
            segments: segments.len(),
            dropped: dropped.segments,
            dropped_seconds: dropped.samples as f64 / SAMPLE_RATE,
            live_accuracy: self.accuracy(&live),
            final_accuracy: self.accuracy(&finals),
            word_recall: self.word_recall(&finals),
        }
    }
}

type Votes<'a> = HashMap<(&'a Label, &'static str), usize>;

impl Scenario {
    /// Fraction of scoreable words whose segment label maps to the right
    /// speaker, under the best one-to-one label-to-speaker assignment.
    fn accuracy(&self, segments: &[Labeled]) -> f64 {
        let owners = self.word_owners();
        let (votes, scoreable) = tally(segments, &owners);
        if scoreable == 0 {
            return 0.0;
        }
        matched_words(self, &votes) as f64 / scoreable as f64
    }

    /// The share of the script's own evidence words that reached the
    /// transcript at all. Accuracy divides by the words the pipeline kept, so
    /// it cannot see speech the gate or the engine threw away; this can.
    fn word_recall(&self, segments: &[Labeled]) -> f64 {
        let owners = self.word_owners();
        let spoken = self
            .turns
            .iter()
            .flat_map(|turn| words(turn.text))
            .filter(|word| matches!(owners.get(word), Some(Some(_))))
            .count();
        if spoken == 0 {
            return 0.0;
        }
        let (_, heard) = tally(segments, &owners);
        (heard as f64 / spoken as f64).min(1.0)
    }

    /// A word owned by exactly one speaker across the script is evidence;
    /// words several speakers say map to `None` and are ignored.
    fn word_owners(&self) -> HashMap<String, Option<&'static str>> {
        let mut owners: HashMap<String, Option<&'static str>> = HashMap::new();
        for turn in &self.turns {
            for word in words(turn.text) {
                owners
                    .entry(word)
                    .and_modify(|owned| {
                        if *owned != Some(turn.speaker) {
                            *owned = None;
                        }
                    })
                    .or_insert(Some(turn.speaker));
            }
        }
        owners
    }
}

/// Counts evidence words per (label, speaker) pairing, plus how many words
/// were scoreable at all — unlabeled evidence counts toward the total.
fn tally<'a>(
    segments: &'a [Labeled],
    owners: &HashMap<String, Option<&'static str>>,
) -> (Votes<'a>, usize) {
    let mut votes = Votes::new();
    let mut scoreable = 0_usize;
    for segment in segments {
        for word in words(&segment.text) {
            let Some(Some(speaker)) = owners.get(&word) else {
                continue;
            };
            scoreable += 1;
            if let Some(label) = &segment.label {
                *votes.entry((label, speaker)).or_default() += 1;
            }
        }
    }
    (votes, scoreable)
}

/// The words a best one-to-one label-to-speaker assignment explains.
fn matched_words(scenario: &Scenario, votes: &Votes<'_>) -> usize {
    let labels: Vec<&Label> = {
        let mut labels: Vec<&Label> = votes.keys().map(|(label, _)| *label).collect();
        labels.sort_unstable();
        labels.dedup();
        labels
    };
    let speakers: Vec<&'static str> = {
        let mut speakers: Vec<&'static str> = scenario.turns.iter().map(|t| t.speaker).collect();
        speakers.sort_unstable();
        speakers.dedup();
        speakers
    };
    best_assignment(&labels, &speakers, votes)
}

/// Exhaustive best one-to-one mapping — label and speaker counts are single
/// digits, so brute force is exact and instant.
fn best_assignment(labels: &[&Label], speakers: &[&'static str], votes: &Votes<'_>) -> usize {
    fn recurse(labels: &[&Label], free: &mut Vec<&'static str>, votes: &Votes<'_>) -> usize {
        let Some((label, rest)) = labels.split_first() else {
            return 0;
        };
        let mut best = recurse(rest, free, votes);
        for index in 0..free.len() {
            let speaker = free.swap_remove(index);
            let here = votes.get(&(*label, speaker)).copied().unwrap_or(0);
            best = best.max(here + recurse(rest, free, votes));
            free.push(speaker);
            let last = free.len() - 1;
            free.swap(index, last);
        }
        best
    }
    let mut free = speakers.to_vec();
    recurse(labels, &mut free, votes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diarize::synth::{Scenario, Turn};

    fn scenario() -> Scenario {
        Scenario {
            name: "test",
            turns: vec![
                Turn {
                    speaker: "alpha",
                    text: "apples grow slowly",
                },
                Turn {
                    speaker: "bravo",
                    text: "bricks stack neatly",
                },
            ],
        }
    }

    fn labeled(text: &str, label: Label) -> Labeled {
        Labeled {
            text: text.to_owned(),
            label: Some(label),
        }
    }

    #[test]
    fn a_consistent_labeling_scores_perfectly_whatever_the_names() {
        let segments = [
            labeled("apples grow slowly", Label::Anonymous(2)),
            labeled("bricks stack neatly", Label::Anonymous(1)),
        ];
        assert_eq!(scenario().accuracy(&segments), 1.0);
    }

    #[test]
    fn a_dropped_turn_costs_recall_but_leaves_accuracy_perfect() {
        // Everything the pipeline kept is labeled right, so accuracy cannot
        // see the missing half. Recall is the only column that falls.
        let segments = [labeled("apples grow slowly", Label::Anonymous(1))];
        assert_eq!(scenario().accuracy(&segments), 1.0);
        assert_eq!(scenario().word_recall(&segments), 0.5);
    }

    #[test]
    fn merging_two_speakers_under_one_label_costs_the_smaller_turn() {
        let segments = [
            labeled("apples grow slowly", Label::Anonymous(1)),
            labeled("bricks stack", Label::Anonymous(1)),
        ];
        assert_eq!(scenario().accuracy(&segments), 0.6);
    }

    #[test]
    fn unlabeled_words_never_score() {
        let segments = [
            labeled("apples grow slowly", Label::Anonymous(1)),
            Labeled {
                text: "bricks stack neatly".to_owned(),
                label: None,
            },
        ];
        assert_eq!(scenario().accuracy(&segments), 0.5);
    }
}
