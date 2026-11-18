use std::ops::Range;

use super::Gate;
use super::{Judgment, NoSegmenter, SpeakerTracker, Timeline, Turn, VoiceprintTracker};
use crate::segmentation::Segmenter;
use crate::speaker::{Diarizer, Embedder, Role, SpeakerError, enrolled_voice};
use crate::transcription::Heard;
use crate::voice_activity::{Progress, SpeechDetector, VoiceActivityDetector, VoiceActivityError};

/// The models each recording's tracker borrows. Its trackers split a turn
/// at a speaker change unchecked.
pub(super) struct Models<E> {
    pub(super) diarizer: Diarizer<E>,
}

impl<E: Embedder> Models<E> {
    pub(super) fn new(diarizer: Diarizer<E>) -> Self {
        Self { diarizer }
    }

    /// Trackers verify each speaker change, as "Only type my voice" does.
    pub(super) fn with_segmenter<S: Segmenter>(self, segmenter: S) -> OwnerModels<E, S> {
        OwnerModels {
            diarizer: self.diarizer,
            segmenter,
        }
    }

    /// A detector and tracker for one recording, starting at `at`.
    pub(super) fn listen<V: VoiceActivityDetector>(
        &mut self,
        vad: V,
        at: u64,
    ) -> Listening<'_, V, E, NoSegmenter> {
        Listening {
            speech: SpeechDetector::new(vad, at),
            tracker: VoiceprintTracker::new(&mut self.diarizer, at),
        }
    }
}

/// The models of trackers that admit only the owner's words.
pub(super) struct OwnerModels<E, S> {
    pub(super) diarizer: Diarizer<E>,
    segmenter: S,
}

impl<E: Embedder, S: Segmenter> OwnerModels<E, S> {
    /// A detector and tracker for one recording, starting at `at`.
    pub(super) fn listen<V: VoiceActivityDetector>(
        &mut self,
        vad: V,
        at: u64,
    ) -> Listening<'_, V, E, &mut S> {
        Listening {
            speech: SpeechDetector::new(vad, at),
            tracker: VoiceprintTracker::owner_only(&mut self.diarizer, &mut self.segmenter, at)
                .expect("an owner is enrolled and the segmenter's shape is valid"),
        }
    }
}

/// A speech detector and the tracker that reads its map. Each push is
/// heard by the detector first, as the feed does.
pub(super) struct Listening<'a, V, E, S> {
    pub(super) speech: SpeechDetector<V>,
    pub(super) tracker: VoiceprintTracker<'a, E, S>,
}

impl<'a, V, E, S> std::ops::Deref for Listening<'a, V, E, S> {
    type Target = VoiceprintTracker<'a, E, S>;

    fn deref(&self) -> &VoiceprintTracker<'a, E, S> {
        &self.tracker
    }
}

impl<V, E, S> std::ops::DerefMut for Listening<'_, V, E, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tracker
    }
}

impl<V: VoiceActivityDetector, E: Embedder, S: Segmenter> Listening<'_, V, E, S> {
    pub(super) fn push(&mut self, samples: &[f32]) -> Result<(), VoiceActivityError> {
        let heard = self.speech.hear(samples);
        SpeakerTracker::hear(&mut self.tracker, self.speech.map(), samples);
        heard
    }
}

pub(super) fn release(gate: &mut Gate, timeline: &Timeline) -> Vec<Heard> {
    gate.decide(timeline, None)
        .released
        .into_iter()
        .map(|released| released.heard)
        .collect()
}

/// The preview provisional text gets, `None` when the gate hides it.
pub(super) fn preview(
    gate: &mut Gate,
    timeline: &Timeline,
    read_to: u64,
    text: &str,
    window: Option<&Range<u64>>,
) -> Option<String> {
    gate.propose(text.to_owned(), window.cloned(), read_to);
    gate.decide(timeline, None)
        .preview
        .filter(|shown| !shown.is_empty())
}

pub(super) const SECOND: usize = 16_000;

pub(super) const OWN_LEVEL: f32 = 0.5;

pub(super) const OTHER_LEVEL: f32 = 0.2;

/// Loud enough for the detector, too faint for the embedder.
pub(super) const FAINT_LEVEL: f32 = 0.05;

pub(super) const OWN: [f32; 2] = [1.0, 0.0];

pub(super) const OTHER: [f32; 2] = [0.0, 1.0];

/// Calls a frame speech when any of it is loud.
pub(super) struct Loud;

impl VoiceActivityDetector for Loud {
    const FRAME_SAMPLES: usize = 160;

    fn probability(&mut self, samples: &[i16]) -> Result<f32, VoiceActivityError> {
        let loud = samples.iter().any(|sample| sample.unsigned_abs() > 1_000);
        Ok(if loud { 1.0 } else { 0.0 })
    }
}

/// Hears the owner in loud samples and another voice in softer ones, so
/// a window that mixes both scores in between, as a real one does.
pub(super) struct VoiceByLevel;

impl Embedder for VoiceByLevel {
    fn embed(&mut self, window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
        let own = window.iter().filter(|sample| **sample >= 0.4).count();
        let other = window
            .iter()
            .filter(|sample| (0.1..0.4).contains(*sample))
            .count();
        if own + other < SECOND / 2 {
            return Ok(None);
        }
        Ok(crate::speaker::normalized(vec![own as f32, other as f32]))
    }

    fn dimension(&self) -> usize {
        2
    }
}

/// The owner and a guest enrolled, heard by [`VoiceByLevel`].
pub(super) fn enrolled() -> Models<VoiceByLevel> {
    Models::new(Diarizer::new(
        VoiceByLevel,
        vec![
            enrolled_voice("alex", Role::Owner, OWN.to_vec()),
            enrolled_voice("sam", Role::Guest, OTHER.to_vec()),
        ],
    ))
}

pub(super) fn audio(parts: &[(f32, f32)]) -> Vec<f32> {
    parts
        .iter()
        .flat_map(|&(level, seconds)| {
            std::iter::repeat_n(level, (seconds * SECOND as f32) as usize)
        })
        .collect()
}

pub(super) fn seconds(from: f32, to: f32) -> Range<u64> {
    (from * SECOND as f32) as u64..(to * SECOND as f32) as u64
}

pub(super) fn heard_text(text: &str, window: Range<u64>) -> Heard {
    Heard {
        segment: crate::transcription::TranscriptSegment::new(text, Some(window.clone())).unwrap(),
        span: Some(window.clone()),
        window,
    }
}

pub(super) fn timed_heard(word: Range<u64>, source: Option<Range<u64>>) -> Heard {
    let mut segment = crate::transcription::TranscriptSegment::new("word", Some(word.clone()))
        .unwrap()
        .with_word_spans(vec![word.clone()])
        .unwrap();
    if let Some(source) = source {
        segment = segment.with_source_window(source).unwrap();
    }
    Heard {
        segment,
        span: Some(word.clone()),
        window: word,
    }
}

pub(super) fn finished_timeline(turns: &[(Range<u64>, Judgment)], reached: u64) -> Timeline {
    Timeline {
        turns: turns
            .iter()
            .map(|(span, judgment)| Turn {
                span: span.clone(),
                judgment: *judgment,
            })
            .collect(),
        reached,
        progress: Progress::Finished,
        ..Timeline::default()
    }
}
