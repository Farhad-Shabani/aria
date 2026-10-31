//! Which decoded words a session lets out, decided against the timeline.

use std::collections::VecDeque;
use std::ops::Range;
use std::time::Duration;

use super::{Tally, Timeline};
use crate::audio::canonical_samples;
use crate::speaker::Label;
use crate::transcription::Heard;
use crate::voice_activity::SpeechMap;

/// Under [`Admission::Spoken`], a segment with less speech than this, whose
/// segment before it had less too, is music or noise.
const MIN_SPEECH_SAMPLES: u64 = canonical_samples(Duration::from_millis(200));

/// Which words a session lets out. The tracker reads it to know what to
/// judge, and the gate reads it to know what to release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Only words in turns judged the owner's, or too short to judge.
    OwnerOnly,
    /// Every word, named when an enrolled voice was heard in it.
    Everyone,
    /// Every word spoken: a segment with no speech under it is dropped.
    Spoken,
}

/// Holds decoded segments until the timeline decides them.
pub struct Gate {
    admission: Admission,
    /// Where the recording starts in the stream.
    from: u64,
    held: VecDeque<Heard>,
    provisional: Option<Provisional>,
    /// Speech under the last segment judged by its speech. A segment short
    /// of speech still passes when this was enough: decode windows lag the
    /// words at a boundary.
    previous_speech: u64,
    /// A whole segment was dropped since the last release, so the next
    /// released segment's reading of its boundary describes words that
    /// never went out.
    dropped_since_release: bool,
}

/// The decoder's latest provisional text.
struct Provisional {
    text: String,
    source_window: Option<Range<u64>>,
    /// How far the decoder had read when it wrote the text.
    read_to: u64,
}

/// A segment let out, and the voice it was heard in.
pub struct Released {
    pub heard: Heard,
    pub speaker: Option<Label>,
    /// The unit-norm embedding of its audio, when the tracker numbers voices.
    pub embedding: Option<Vec<f32>>,
}

impl Released {
    fn new(heard: Heard, timeline: &Timeline) -> Self {
        Self {
            speaker: timeline.label(&heard.window).cloned(),
            embedding: timeline.embedding(&heard.window),
            heard,
        }
    }
}

/// What may go out now. Segments still undecided stay held.
#[derive(Default)]
pub struct Decision {
    pub released: Vec<Released>,
    /// Whole segments kept out: in another voice, or with no speech.
    pub dropped: Vec<Heard>,
    /// The provisional text to show, empty to hide it. `None` while the
    /// decoder has none since its last finalized segment.
    pub preview: Option<String>,
}

impl Gate {
    /// `from` is where the recording starts in the stream.
    pub fn new(admission: Admission, from: u64) -> Self {
        Self {
            admission,
            from,
            held: VecDeque::new(),
            provisional: None,
            previous_speech: 0,
            dropped_since_release: false,
        }
    }

    pub fn admission(&self) -> Admission {
        self.admission
    }

    /// Where the oldest held segment's audio starts, while one is held.
    pub fn undecided_from(&self) -> Option<u64> {
        self.held.front().map(|heard| heard.window.start)
    }

    /// A finalized segment joins the queue. It replaces the provisional text it came from.
    pub fn hold(&mut self, heard: Heard) {
        self.provisional = None;
        self.held.push_back(heard);
    }

    /// The decoder's latest provisional text replaces the last.
    pub fn propose(&mut self, text: String, source_window: Option<Range<u64>>, read_to: u64) {
        self.provisional = Some(Provisional {
            text,
            source_window,
            read_to,
        });
    }

    /// Decides what the timeline and the speech map allow out now. The
    /// preview is judged before the release, against the segments held when
    /// it was asked for. Without a speech map, every recording counts as
    /// speech.
    pub fn decide(&mut self, timeline: &Timeline, speech: Option<&SpeechMap>) -> Decision {
        let mut decision = Decision {
            preview: self.preview(timeline),
            ..Decision::default()
        };
        match self.admission {
            Admission::Everyone => decision.released.extend(
                self.held
                    .drain(..)
                    .map(|heard| Released::new(heard, timeline)),
            ),
            Admission::OwnerOnly => self.release_owner(timeline, &mut decision),
            Admission::Spoken => self.release_spoken(timeline, speech, &mut decision),
        }
        decision
    }

    /// Releases segments with speech under them, in order, once the speech
    /// map has scored their audio.
    fn release_spoken(
        &mut self,
        timeline: &Timeline,
        speech: Option<&SpeechMap>,
        decision: &mut Decision,
    ) {
        while let Some(heard) = self.held.pop_front() {
            // A map that could not be judged counts as speech.
            let spoken = match speech.filter(|speech| !speech.is_broken()) {
                None => true,
                Some(speech) => {
                    if heard.window.end > speech.reached() && !speech.is_finished() {
                        self.held.push_front(heard);
                        break;
                    }
                    let heard_speech = speech.speech(&heard.window);
                    let enough = heard_speech.max(self.previous_speech) >= MIN_SPEECH_SAMPLES;
                    self.previous_speech = heard_speech;
                    enough
                }
            };
            if spoken {
                decision.released.push(Released::new(heard, timeline));
            } else {
                tracing::debug!("a segment with no speech under it was dropped");
                decision.dropped.push(heard);
            }
        }
    }

    /// Releases admitted words in order; broken tracking discards the queue.
    fn release_owner(&mut self, timeline: &Timeline, decision: &mut Decision) {
        while let Some(mut heard) = self.held.pop_front() {
            if timeline.is_broken() {
                tracing::debug!("segment not delivered: nothing can be judged");
                decision.dropped.push(heard);
                self.dropped_since_release = true;
                continue;
            }
            let Some(keep) = word_admissions(timeline, &heard, self.from) else {
                self.held.push_front(heard);
                break;
            };
            let Some(mut segment) = heard.segment.keeping(&keep) else {
                tracing::debug!("segment in another voice not delivered");
                decision.dropped.push(heard);
                self.dropped_since_release = true;
                continue;
            };
            if segment.word_spans().is_some() {
                heard.span = segment.span();
                heard.window = segment.span().unwrap_or(heard.window);
            }
            if std::mem::take(&mut self.dropped_since_release) {
                segment = segment.with_sentence_ended_before(None);
            }
            heard.segment = segment;
            decision.released.push(Released::new(heard, timeline));
        }
    }

    fn preview(&self, timeline: &Timeline) -> Option<String> {
        let provisional = self.provisional.as_ref()?;
        Some(match self.admission {
            Admission::Everyone | Admission::Spoken => provisional.text.clone(),
            Admission::OwnerOnly => self
                .owner_preview(timeline, provisional)
                .unwrap_or_default(),
        })
    }

    /// The held words and the provisional text after them, while every one of them
    /// sits in the owner's speech so far.
    fn owner_preview(&self, timeline: &Timeline, provisional: &Provisional) -> Option<String> {
        let read_to = provisional.read_to;
        if timeline.is_broken() || timeline.reached < read_to {
            return None;
        }
        let fallback = self.from..read_to;
        let source = provisional.source_window.as_ref().unwrap_or(&fallback);
        if source.start < self.from || source.start > source.end || source.end > read_to {
            return None;
        }
        let provisional_owner = provisional.text.is_empty() || timeline.admitted_so_far(source);
        let held_owner = self
            .held
            .iter()
            .all(|heard| timeline.admitted_so_far(&heard.window));
        if !provisional_owner || !held_owner {
            return None;
        }
        let words: Vec<&str> = self
            .held
            .iter()
            .map(|heard| heard.segment.as_str())
            .chain([provisional.text.as_str()])
            .filter(|text| !text.is_empty())
            .collect();
        Some(words.join(" "))
    }
}

fn word_admissions(timeline: &Timeline, heard: &Heard, from: u64) -> Option<Vec<bool>> {
    let source = heard.segment.source_window();
    match heard.segment.word_spans() {
        Some(words) => words
            .iter()
            .map(|word| word_admission(timeline, word, source.as_ref(), from))
            .collect(),
        None => timeline
            .wholly_admitted(&heard.window)
            .map(|owner| vec![owner]),
    }
}

fn word_admission(
    timeline: &Timeline,
    word: &Range<u64>,
    source: Option<&Range<u64>>,
    from: u64,
) -> Option<bool> {
    let admitted = timeline.admission(word)?;
    if admitted
        || word.start >= timeline.reached
        || word.end > timeline.reached
        || !timeline.tally(word).is_some_and(Tally::is_empty)
    {
        return Some(admitted);
    }
    let Some(source) = source.filter(|source| source.start >= from && covers(source, word)) else {
        return Some(false);
    };
    // A coarse word timestamp can miss speech that its decode window contains.
    timeline.wholly_admitted(source)
}

fn covers(source: &Range<u64>, word: &Range<u64>) -> bool {
    source.start <= word.start && word.start < source.end && word.end <= source.end
}
