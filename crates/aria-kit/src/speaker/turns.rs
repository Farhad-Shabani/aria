//! Who is speaking, cut from the audio itself: turns of speech between
//! pauses, each judged as the owner's voice or another's. The audio sets the
//! clock, not the transcriber, so a decoder that finalizes late or in big
//! pieces cannot merge two voices into one turn.

use std::ops::Range;

use super::{Label, SpeakerName};
use crate::voice_activity::{Progress, SpeechMap};

mod feed;
mod gate;
mod tracker;

pub use feed::{Ahead, TurnFeed};
pub use gate::{Admission, Decision, Gate, Released};
pub use tracker::{NoSegmenter, OwnerTrackingError, VoiceprintTracker};

/// Public items here stay unnameable outside this module.
mod sealed {
    pub trait Sealed {}
}

/// Says who spoke when in one recording, reading where anyone spoke from a
/// [`SpeechMap`]. A recording's audio arrives in order, in pieces of any
/// size; the map must have scored a frame before the tracker hears it.
///
/// Only this crate implements it, since a [`Timeline`] is built here.
pub trait SpeakerTracker: sealed::Sealed {
    /// Where the next sample heard sits.
    fn heard_to(&self) -> u64;
    /// Takes the audio after what it has heard.
    fn hear(&mut self, speech: &SpeechMap, audio: &[f32]);
    /// Gives up on the audio before `at`, which it will never hear.
    fn skip_to(&mut self, speech: &SpeechMap, at: u64);
    /// Judges what is still open. Call once the recording ends.
    fn finish(&mut self);
    fn timeline(&self) -> &Timeline;
    /// Forgets the timeline before `at`, or before the open turn's start
    /// when that comes first. Returns how far the speech map may be
    /// forgotten: never past what the tracker still reads from it.
    fn forget_before(&mut self, at: u64) -> u64;
}

/// One run of speech and whose voice it is.
#[derive(Debug, Clone, PartialEq)]
struct Turn {
    /// Stream samples, through the pause that ended it. Grows while open.
    span: Range<u64>,
    judgment: Judgment,
}

#[derive(Debug, Clone, PartialEq)]
struct LabeledTurn {
    span: Range<u64>,
    label: Option<Label>,
}

/// The embedding one check computed over a span.
#[derive(Debug, Clone, PartialEq)]
struct Embedding {
    span: Range<u64>,
    vector: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Judgment {
    /// Too little heard to tell.
    Pending,
    Unverified,
    Owner,
    Other,
}

/// Whose voice the latest turn is, as judged so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Speaker {
    /// Owner admission is undecided, though the voice may already have a name.
    Pending(Option<SpeakerName>),
    Unverified,
    /// The owner's, and the name they are enrolled under.
    Owner(Option<SpeakerName>),
    /// Another voice, named when it is an enrolled guest's.
    Other(Option<SpeakerName>),
}

/// The turns of one recording so far, and how far into it they reach.
#[derive(Debug, Clone, Default)]
pub struct Timeline {
    turns: Vec<Turn>,
    // Kept apart from `turns`: a label change must not judge a turn or reset
    // its evidence.
    labels: Vec<LabeledTurn>,
    /// Kept only by a tracker that numbers voices, so a session can
    /// relabel them once it ends.
    embeddings: Vec<Embedding>,
    reached: u64,
    /// Whether the last turn is still open.
    open: bool,
    stable_to: u64,
    progress: Progress,
}

/// How much of a span lies in admitted turns and how much in rejected ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tally {
    admitted: u64,
    rejected: u64,
}

impl Tally {
    /// No judged turn lies under the span.
    fn is_empty(self) -> bool {
        self.admitted == 0 && self.rejected == 0
    }

    fn mostly_admitted(self) -> bool {
        self.admitted > self.rejected
    }

    fn wholly_admitted(self) -> bool {
        self.admitted > 0 && self.rejected == 0
    }
}

impl Timeline {
    fn admission(&self, span: &Range<u64>) -> Option<bool> {
        self.tally(span).map(Tally::mostly_admitted)
    }

    pub fn wholly_admitted(&self, span: &Range<u64>) -> Option<bool> {
        if self.is_broken() || span.start > span.end {
            return None;
        }
        self.tally(span).map(Tally::wholly_admitted)
    }

    fn admitted_so_far(&self, span: &Range<u64>) -> bool {
        self.tally_stable_to(span, u64::MAX)
            .is_some_and(Tally::wholly_admitted)
    }

    fn tally(&self, span: &Range<u64>) -> Option<Tally> {
        self.tally_stable_to(span, self.stable_to)
    }

    /// [`Self::tally`], with the open turn stable up to `stable_to`.
    fn tally_stable_to(&self, span: &Range<u64>, stable_to: u64) -> Option<Tally> {
        // A word timed at a single instant still sits in a turn.
        let span = span.start..span.end.max(span.start.saturating_add(1));
        if span.end > self.reached {
            return (self.progress == Progress::Finished).then_some(Tally {
                admitted: 0,
                rejected: 0,
            });
        }
        let open = self.open.then(|| self.turns.len() - 1);
        let mut tally = Tally {
            admitted: 0,
            rejected: 0,
        };
        for (index, turn) in self.turns.iter().enumerate() {
            let covered = overlap(&span, &turn.span);
            if covered == 0 {
                continue;
            }
            if open == Some(index) && span.end > stable_to {
                return None;
            }
            match turn.judgment {
                Judgment::Pending => return None,
                Judgment::Owner | Judgment::Unverified => tally.admitted += covered,
                Judgment::Other => tally.rejected += covered,
            }
        }
        Some(tally)
    }

    /// The end of the last whole frame the tracker has heard.
    pub fn reached(&self) -> u64 {
        self.reached
    }

    /// Forgets turns, labels and embeddings that ended before `at`, keeping
    /// the latest turn and the latest label. Only for sessions that never
    /// ask about the whole recording.
    pub(crate) fn forget_before(&mut self, at: u64) {
        let ended = |span: &Range<u64>| span.end <= at;
        let turns = self
            .turns
            .iter()
            .take_while(|turn| ended(&turn.span))
            .count();
        self.turns
            .drain(..turns.min(self.turns.len().saturating_sub(1)));
        let labels = self
            .labels
            .iter()
            .take_while(|turn| ended(&turn.span))
            .count();
        self.labels
            .drain(..labels.min(self.labels.len().saturating_sub(1)));
        self.embeddings.retain(|embedding| !ended(&embedding.span));
    }

    pub fn is_broken(&self) -> bool {
        self.progress == Progress::Broken
    }

    fn break_off(&mut self) {
        self.progress = Progress::Broken;
    }

    /// A timeline nothing can be judged by: every segment held against it
    /// is dropped.
    pub fn broken() -> Self {
        Self {
            progress: Progress::Broken,
            ..Self::default()
        }
    }

    /// The enrolled voice of the turn that covers most of `span`.
    pub fn speaker(&self, span: &Range<u64>) -> Option<&SpeakerName> {
        self.label(span)?.enrolled()
    }

    /// The unit-norm embedding of `span`: every check's embedding over it,
    /// each weighted by how much of `span` it covers.
    fn embedding(&self, span: &Range<u64>) -> Option<Vec<f32>> {
        if self.is_broken() {
            return None;
        }
        let mut sum = Vec::new();
        for embedding in &self.embeddings {
            let weight = overlap(span, &embedding.span) as f32;
            if weight == 0.0 {
                continue;
            }
            sum.resize(embedding.vector.len(), 0.0);
            for (sum, value) in sum.iter_mut().zip(&embedding.vector) {
                *sum += value * weight;
            }
        }
        super::normalized(sum)
    }

    /// The label of the turn that covers most of `span`.
    fn label(&self, span: &Range<u64>) -> Option<&Label> {
        if self.is_broken() {
            return None;
        }
        self.labels
            .iter()
            .filter(|turn| overlap(span, &turn.span) > 0)
            .max_by_key(|turn| overlap(span, &turn.span))?
            .label
            .as_ref()
    }

    /// Whose voice the latest turn is. `None` before anyone speaks.
    pub fn speaking(&self) -> Option<Speaker> {
        if self.is_broken() {
            return None;
        }
        let turn = self.turns.last()?;
        let name = self
            .labels
            .last()
            .and_then(|turn| turn.label.as_ref()?.enrolled().cloned());
        Some(match turn.judgment {
            Judgment::Pending => Speaker::Pending(name),
            Judgment::Unverified => Speaker::Unverified,
            Judgment::Owner => Speaker::Owner(name),
            Judgment::Other => Speaker::Other(name),
        })
    }

    /// The name covering the longest span of admitted owner audio.
    pub fn owner_name(&self) -> Option<&SpeakerName> {
        if self.is_broken() {
            return None;
        }
        self.labels
            .iter()
            .filter_map(|labeled| {
                let name = labeled.label.as_ref()?.enrolled()?;
                let owner_samples: u64 = self
                    .turns
                    .iter()
                    .filter(|turn| turn.judgment == Judgment::Owner)
                    .map(|turn| overlap(&labeled.span, &turn.span))
                    .sum();
                (owner_samples > 0).then_some((owner_samples, name))
            })
            .max_by_key(|(samples, _)| *samples)
            .map(|(_, name)| name)
    }

    pub fn admitted_is_owner(&self) -> bool {
        self.progress == Progress::Finished
            && self
                .turns
                .iter()
                .any(|turn| turn.judgment == Judgment::Owner)
            && self
                .turns
                .iter()
                .all(|turn| matches!(turn.judgment, Judgment::Owner | Judgment::Other))
    }

    /// The audio of turns that may be typed: the owner's, and those too
    /// short to tell.
    pub fn admitted_audio(&self, samples: &[f32], from: u64) -> Vec<f32> {
        self.audio_judged(samples, from, |judgment| {
            matches!(judgment, Judgment::Owner | Judgment::Unverified)
        })
    }

    /// The audio of turns judged the owner's, and nothing else: what may
    /// leave this computer.
    pub fn owner_audio(&self, samples: &[f32], from: u64) -> Vec<f32> {
        self.audio_judged(samples, from, |judgment| judgment == Judgment::Owner)
    }

    /// Whether every turn over `span` is the owner's, once decided.
    pub fn wholly_owner(&self, span: &Range<u64>) -> Option<bool> {
        let decided = self.wholly_admitted(span)?;
        Some(
            decided
                && self
                    .turns
                    .iter()
                    .filter(|turn| overlap(span, &turn.span) > 0)
                    .all(|turn| turn.judgment == Judgment::Owner),
        )
    }

    fn audio_judged(
        &self,
        samples: &[f32],
        from: u64,
        keep: impl Fn(Judgment) -> bool,
    ) -> Vec<f32> {
        if self.is_broken() {
            return Vec::new();
        }
        self.turns
            .iter()
            .filter(|turn| keep(turn.judgment))
            .flat_map(|turn| {
                let index =
                    |position: u64| (position.saturating_sub(from) as usize).min(samples.len());
                &samples[index(turn.span.start)..index(turn.span.end)]
            })
            .copied()
            .collect()
    }
}

fn overlap(a: &Range<u64>, b: &Range<u64>) -> u64 {
    a.end.min(b.end).saturating_sub(a.start.max(b.start))
}
