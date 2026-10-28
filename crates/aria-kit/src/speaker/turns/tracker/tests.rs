use std::cell::Cell;

use super::*;
use crate::audio::RecordingReader;
use crate::speaker::turns::{Admission, Ahead, Gate, Speaker, TurnFeed};
use crate::speaker::{LevelSegmenter, Profile, Role, SpeakerError, enrolled_voice};
use crate::transcription::Heard;
use crate::voice_activity::VoiceActivityError;
use crate::voice_activity::{SpeechDetector, VoiceActivityDetector};

/// The models each recording's tracker borrows.
struct Models<E, G = NoSegmenter> {
    diarizer: Diarizer<E>,
    /// Without one, a tracker splits a turn at a speaker change unchecked.
    segmenter: Option<G>,
}

impl<E: Embedder> Models<E> {
    fn new(diarizer: Diarizer<E>) -> Self {
        Self {
            diarizer,
            segmenter: None,
        }
    }

    /// Trackers verify each speaker change, as "Only type my voice" does.
    fn with_segmenter<G: Segmenter>(self, segmenter: G) -> Models<E, G> {
        Models {
            diarizer: self.diarizer,
            segmenter: Some(segmenter),
        }
    }
}

impl<E: Embedder, G: Segmenter> Models<E, G> {
    /// A detector and tracker for one recording, starting at `at`.
    fn listen<V: VoiceActivityDetector>(&mut self, vad: V, at: u64) -> Listening<'_, V, E, G> {
        Listening {
            speech: SpeechDetector::new(vad, at),
            tracker: VoiceprintTracker::new(&mut self.diarizer, self.segmenter.as_mut(), at)
                .expect("the segmenter's shape is valid"),
        }
    }
}

/// A speech detector and the tracker that reads its map. Each push is
/// heard by the detector first, as the feed does.
struct Listening<'a, V, E, G> {
    speech: SpeechDetector<V>,
    tracker: VoiceprintTracker<'a, E, &'a mut G>,
}

impl<'a, V, E, G> std::ops::Deref for Listening<'a, V, E, G> {
    type Target = VoiceprintTracker<'a, E, &'a mut G>;

    fn deref(&self) -> &VoiceprintTracker<'a, E, &'a mut G> {
        &self.tracker
    }
}

impl<'a, V, E, G> std::ops::DerefMut for Listening<'a, V, E, G> {
    fn deref_mut(&mut self) -> &mut VoiceprintTracker<'a, E, &'a mut G> {
        &mut self.tracker
    }
}

impl<V: VoiceActivityDetector, E: Embedder, G: Segmenter> Listening<'_, V, E, G> {
    fn push(&mut self, samples: &[f32]) -> Result<(), VoiceActivityError> {
        let heard = self.speech.hear(samples);
        self.tracker.push(self.speech.map(), samples);
        heard
    }
}

impl<V: VoiceActivityDetector + Default, E: Embedder, G: Segmenter> Listening<'_, V, E, G> {
    /// The detector starts over where the tracker does.
    fn skip_to(&mut self, at: u64) {
        self.speech = SpeechDetector::new(V::default(), at);
        self.tracker.skip_to(at);
    }
}

fn release(gate: &mut Gate, timeline: &Timeline) -> Vec<Heard> {
    gate.decide(timeline)
        .released
        .into_iter()
        .map(|released| released.heard)
        .collect()
}

/// The preview provisional text gets, `None` when the gate hides it.
fn preview(
    gate: &mut Gate,
    timeline: &Timeline,
    read_to: u64,
    text: &str,
    window: Option<&Range<u64>>,
) -> Option<String> {
    gate.propose(text.to_owned(), window.cloned(), read_to);
    gate.decide(timeline)
        .preview
        .filter(|shown| !shown.is_empty())
}

const SECOND: usize = 16_000;
const OWN_LEVEL: f32 = 0.5;
const OTHER_LEVEL: f32 = 0.2;
/// Loud enough for the detector, too faint for the embedder.
const FAINT_LEVEL: f32 = 0.05;
const OWN: [f32; 2] = [1.0, 0.0];
const OTHER: [f32; 2] = [0.0, 1.0];

/// Calls a frame speech when any of it is loud.
#[derive(Default)]
struct Loud;

impl VoiceActivityDetector for Loud {
    const FRAME_SAMPLES: usize = 160;

    fn probability(&mut self, samples: &[i16]) -> Result<f32, VoiceActivityError> {
        let loud = samples.iter().any(|sample| sample.unsigned_abs() > 1_000);
        Ok(if loud { 1.0 } else { 0.0 })
    }
}

struct SoftOnset;

impl VoiceActivityDetector for SoftOnset {
    const FRAME_SAMPLES: usize = 512;

    fn probability(&mut self, samples: &[i16]) -> Result<f32, VoiceActivityError> {
        Ok(if samples.iter().any(|sample| *sample > 16_000) {
            1.0
        } else {
            0.0
        })
    }
}

/// Hears the owner in loud samples and another voice in softer ones, so
/// a window that mixes both scores in between, as a real one does.
struct VoiceByLevel;

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
fn enrolled() -> Models<VoiceByLevel> {
    Models::new(Diarizer::new(
        VoiceByLevel,
        vec![
            enrolled_voice("alex", Role::Owner, OWN.to_vec()),
            enrolled_voice("sam", Role::Guest, OTHER.to_vec()),
        ],
    ))
}

fn audio(parts: &[(f32, f32)]) -> Vec<f32> {
    parts
        .iter()
        .flat_map(|&(level, seconds)| {
            std::iter::repeat_n(level, (seconds * SECOND as f32) as usize)
        })
        .collect()
}

fn heard<'a>(
    models: &'a mut Models<VoiceByLevel>,
    parts: &[(f32, f32)],
) -> Listening<'a, Loud, VoiceByLevel, NoSegmenter> {
    let mut tracker = models.listen(Loud, 0);
    tracker.push(&audio(parts)).expect("the detector scores");
    tracker
}

fn seconds(from: f32, to: f32) -> Range<u64> {
    (from * SECOND as f32) as u64..(to * SECOND as f32) as u64
}

fn judgments<V, E, G>(tracker: &Listening<'_, V, E, G>) -> Vec<Judgment> {
    tracker
        .timeline
        .turns
        .iter()
        .map(|turn| turn.judgment)
        .collect()
}

#[test]
fn voice_evidence_keeps_the_soft_onset_in_its_span() {
    let mut stream = vec![0.45; 3 * SoftOnset::FRAME_SAMPLES];
    stream.extend(vec![OWN_LEVEL; 13 * SoftOnset::FRAME_SAMPLES]);
    for piece in [1, 333, stream.len()] {
        let mut models = Models::new(Diarizer::new(VoiceByLevel, owner()));
        let mut tracker = models.listen(SoftOnset, 0);
        for chunk in stream.chunks(piece) {
            tracker.push(chunk).unwrap();
        }
        assert_eq!(
            tracker.open.as_ref().unwrap().audio,
            stream,
            "pieces of {piece}"
        );
        assert_eq!(tracker.timeline.reached(), stream.len() as u64);
        tracker.finish();
        assert_eq!(tracker.timeline.turns[0].span, 0..stream.len() as u64);
        assert_eq!(tracker.timeline.turns[0].judgment, Judgment::Owner);
    }
}

#[test]
fn voice_evidence_does_not_reuse_the_previous_turns_audio() {
    let mut models = Models::new(Diarizer::new(VoiceByLevel, owner()));
    let mut tracker = models.listen(SoftOnset, 0);
    tracker
        .push(&vec![OWN_LEVEL; SoftOnset::FRAME_SAMPLES])
        .unwrap();
    tracker
        .push(&vec![0.0; 22 * SoftOnset::FRAME_SAMPLES])
        .unwrap();
    assert!(tracker.open.is_none());
    let previous_end = tracker.timeline.turns[0].span.end;
    let mut next = vec![0.45; 3 * SoftOnset::FRAME_SAMPLES];
    next.extend(vec![OWN_LEVEL; 13 * SoftOnset::FRAME_SAMPLES]);
    tracker.push(&next).unwrap();

    assert_eq!(tracker.open.as_ref().unwrap().audio, next);
    assert_eq!(tracker.timeline.turns[1].span.start, previous_end);
    assert_eq!(tracker.timeline.reached(), previous_end + next.len() as u64);
}

#[test]
fn silent_lead_in_does_not_advance_the_first_voice_check() {
    let mut models = enrolled();
    let tracker = heard(&mut models, &[(0.0, 0.1), (OWN_LEVEL, 0.5)]);
    assert_eq!(judgments(&tracker), [Judgment::Owner]);
}

#[test]
fn a_short_turn_is_judged_when_its_pause_ends_it() {
    for (level, judgment) in [(OWN_LEVEL, Judgment::Owner), (OTHER_LEVEL, Judgment::Other)] {
        let mut models = enrolled();
        let tracker = heard(&mut models, &[(level, 0.8), (0.0, 1.0)]);
        assert_eq!(judgments(&tracker), [judgment]);
        assert!(tracker.open.is_none());
    }
}

#[test]
fn the_owners_new_turn_starts_at_the_pause_before_their_first_words() {
    let mut models = enrolled();
    let mut tracker = heard(
        &mut models,
        &[(OTHER_LEVEL, 4.0), (0.0, 0.2), (OWN_LEVEL, 0.8)],
    );
    tracker.finish();

    assert_eq!(judgments(&tracker), [Judgment::Other, Judgment::Owner]);
    assert_eq!(tracker.timeline.admission(&seconds(3.5, 4.0)), Some(false));
    assert_eq!(tracker.timeline.admission(&seconds(4.2, 4.5)), Some(true));
}

#[test]
fn ending_at_a_detected_voice_change_keeps_the_guest_out() {
    let mut models = enrolled();
    let mut voices = models.listen(Loud, 0);
    voices.push(&audio(&[(OWN_LEVEL, 2.0)])).unwrap();
    for _ in 0..8 {
        voices.push(&audio(&[(OTHER_LEVEL, 0.5)])).unwrap();
        if voices.timeline.turns.last().unwrap().judgment == Judgment::Pending {
            let changed = voices.timeline.turns.last().unwrap().span.clone();
            voices.finish();
            assert_eq!(voices.timeline.admission(&changed), Some(false));
            return;
        }
    }
    panic!("the tracker must detect the guest");
}

#[test]
fn another_voice_answering_the_owner_without_a_pause_is_kept_out() {
    let mut models = enrolled();
    let mut tracker = heard(
        &mut models,
        &[(OWN_LEVEL, 1.0), (OTHER_LEVEL, 1.2), (0.0, 1.0)],
    );
    tracker.finish();

    assert_eq!(judgments(&tracker), [Judgment::Owner, Judgment::Other]);
    assert_eq!(tracker.timeline.admission(&seconds(0.0, 1.0)), Some(true));
    assert_eq!(tracker.timeline.admission(&seconds(1.0, 2.2)), Some(false));
}

#[test]
fn words_a_clear_check_heard_past_are_stable_early() {
    let mut models = enrolled();
    let tracker = heard(&mut models, &[(OWN_LEVEL, 2.0)]);

    assert_eq!(tracker.timeline.admission(&seconds(1.2, 1.4)), Some(true));
    assert_eq!(tracker.timeline.admission(&seconds(1.6, 1.8)), None);
}

#[test]
fn a_turn_opens_a_few_frames_before_its_first_loud_one() {
    let mut models = enrolled();
    let tracker = heard(&mut models, &[(0.0, 1.0), (OWN_LEVEL, 1.0)]);

    let lead_in = (LEAD_IN_FRAMES * Loud::FRAME_SAMPLES) as u64;
    assert_eq!(
        tracker.timeline.turns[0].span.start,
        SECOND as u64 - lead_in
    );
}

#[test]
fn unscorable_speech_waits_until_the_turn_closes() {
    let mut models = enrolled();
    let mut tracker = heard(&mut models, &[(FAINT_LEVEL, 2.0)]);
    assert_eq!(judgments(&tracker), [Judgment::Pending]);

    tracker.push(&audio(&[(FAINT_LEVEL, 1.0)])).unwrap();
    assert_eq!(judgments(&tracker), [Judgment::Pending]);
    tracker.finish();
    assert_eq!(judgments(&tracker), [Judgment::Unverified]);
}

#[test]
fn a_span_not_heard_yet_waits() {
    let mut models = enrolled();
    let mut tracker = heard(&mut models, &[(OWN_LEVEL, 1.0), (0.0, 1.0)]);
    assert_eq!(tracker.timeline.admission(&seconds(1.5, 2.5)), None);
    tracker.finish();
    assert_eq!(tracker.timeline.admission(&seconds(1.5, 2.5)), Some(false));
}

#[test]
fn most_of_a_span_decides_it() {
    let mut models = enrolled();
    let mut tracker = heard(
        &mut models,
        &[(OTHER_LEVEL, 1.0), (0.0, 1.0), (OWN_LEVEL, 2.0)],
    );
    tracker.finish();

    assert_eq!(tracker.timeline.admission(&seconds(0.5, 4.0)), Some(true));
    assert_eq!(tracker.timeline.admission(&seconds(0.0, 2.5)), Some(false));
}

#[test]
fn each_turn_carries_the_enrolled_voice_it_sounds_like() {
    let mut models = enrolled();
    let mut tracker = heard(
        &mut models,
        &[(OTHER_LEVEL, 1.0), (0.0, 1.0), (OWN_LEVEL, 1.0)],
    );
    tracker.finish();

    let name = |from, to| {
        tracker
            .timeline
            .speaker(&seconds(from, to))
            .map(SpeakerName::as_str)
    };
    assert_eq!(name(0.0, 1.0), Some("sam"));
    assert_eq!(name(2.0, 3.0), Some("alex"));
}

/// Embeds each window at the similarity to [`OWN`] the closure gives it,
/// or not at all for `None`.
struct Scores<F: FnMut(&[f32]) -> Option<f32>>(F);

impl<F: FnMut(&[f32]) -> Option<f32>> Embedder for Scores<F> {
    fn embed(&mut self, window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
        Ok((self.0)(window).map(|score| vec![score, (1.0 - score * score).sqrt()]))
    }

    fn dimension(&self) -> usize {
        2
    }
}

/// Answers each chunk with the closure, on [`LevelSegmenter`]'s shape.
struct Segments<F: FnMut(&[f32]) -> Result<Vec<Activity>, SegmentationError>>(F);

impl<F: FnMut(&[f32]) -> Result<Vec<Activity>, SegmentationError>> Segmenter for Segments<F> {
    fn shape(&self) -> ChunkShape {
        LevelSegmenter.shape()
    }

    fn segment(&mut self, chunk: &[f32]) -> Result<Vec<Activity>, SegmentationError> {
        (self.0)(chunk)
    }
}

fn owner() -> Vec<Profile> {
    vec![enrolled_voice("alex", Role::Owner, OWN.to_vec())]
}

/// Only the owner enrolled, whose voice `score` hears.
fn scored_by(score: impl FnMut(&[f32]) -> Option<f32>) -> Models<impl Embedder> {
    Models::new(Diarizer::new(Scores(score), owner()))
}

fn scored(score: &Cell<Option<f32>>) -> Models<impl Embedder> {
    scored_by(|_| score.get())
}

#[test]
fn a_short_similarity_peak_cannot_promote_a_continuing_other_voice() {
    let peaked = Cell::new(false);
    let mut models = scored_by(|window| {
        Some(if peaked.get() && window.len() <= RECENT_SAMPLES {
            0.4206
        } else {
            0.2353
        })
    })
    .with_segmenter(LevelSegmenter);
    let mut voices = models.listen(Loud, 0);
    voices.push(&audio(&[(OTHER_LEVEL, 5.0)])).unwrap();
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
    peaked.set(true);
    voices.push(&audio(&[(OTHER_LEVEL, 1.0)])).unwrap();
    voices.finish();
    assert!(!judgments(&voices).contains(&Judgment::Owner));
    assert!(
        voices
            .timeline
            .admitted_audio(&audio(&[(OTHER_LEVEL, 6.0)]), 0)
            .is_empty()
    );
}

/// Two voices in every frame, so no speaker change resolves.
fn ambiguous(_: &[f32]) -> Result<Vec<Activity>, SegmentationError> {
    let shape = LevelSegmenter.shape();
    let count = (shape.samples - shape.frame_size) / shape.frame_shift + 1;
    Ok(vec![Activity::SILENT.speaking(0).speaking(1); count])
}

/// A tracker that has proposed a change to the owner and not verified it.
fn ambiguous_tracker<'a, E: Embedder, G: Segmenter>(
    models: &'a mut Models<E, G>,
    score: &Cell<Option<f32>>,
) -> Listening<'a, Loud, E, G> {
    let mut voices = models.listen(Loud, 0);
    voices.push(&audio(&[(OTHER_LEVEL, 5.0)])).unwrap();
    score.set(Some(0.6));
    voices.push(&audio(&[(OTHER_LEVEL, 0.5)])).unwrap();
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Pending(None)));
    voices
}

#[test]
fn unresolved_changes_are_optimistic_only_when_the_turn_ends_within_budget() {
    for (after, expected) in [
        (0.0, Judgment::Unverified),
        (1.5, Judgment::Unverified),
        (2.5, Judgment::Other),
        (3.0, Judgment::Other),
    ] {
        let score = Cell::new(Some(0.2));
        let mut models = scored(&score).with_segmenter(Segments(ambiguous));
        let mut voices = ambiguous_tracker(&mut models, &score);
        let from = voices.open.as_ref().unwrap().proposed.unwrap();
        assert!(voices.timeline.stable_to <= from);
        voices.push(&audio(&[(OTHER_LEVEL, after)])).unwrap();
        voices.finish();
        assert_eq!(voices.timeline.turns.last().unwrap().judgment, expected);
        assert!(!voices.timeline.admitted_is_owner());
        assert_eq!(voices.timeline.owner_name(), None);
    }
}

#[test]
fn an_expired_change_cannot_be_admitted_by_an_unchecked_empty_close() {
    let score = Cell::new(Some(0.2));
    let mut models = scored(&score).with_segmenter(Segments(ambiguous));
    let mut voices = ambiguous_tracker(&mut models, &score);
    let from = voices.position() - TURN_JUDGED_SAMPLES as u64;
    let open = voices.open.as_mut().unwrap();
    open.proposed = Some(from);
    open.unchecked = 0;
    voices.timeline.turns.last_mut().unwrap().span.start = from;
    voices.close_turn(0);
    assert_eq!(
        voices.timeline.turns.last().unwrap().judgment,
        Judgment::Other
    );
}

#[test]
fn filtering_recognizes_a_true_replacement_and_preserves_a_stable_prefix() {
    let mut models = enrolled().with_segmenter(LevelSegmenter);
    let mut voices = models.listen(Loud, 0);
    voices.push(&audio(&[(OTHER_LEVEL, 4.0)])).unwrap();
    let stable = voices.timeline.stable_to;
    voices.push(&audio(&[(OWN_LEVEL, 3.0)])).unwrap();
    voices.finish();
    assert_eq!(voices.timeline.admission(&(0..stable)), Some(false));
    assert_eq!(voices.timeline.admission(&seconds(5.0, 7.0)), Some(true));
    assert_eq!(
        voices.timeline.speaking(),
        Some(Speaker::Owner(Some(SpeakerName::new("alex").unwrap())))
    );
}

#[test]
fn homogeneous_identity_can_correct_an_initial_other_judgment_only_forward() {
    let score = Cell::new(Some(0.2));
    let mut models = scored(&score).with_segmenter(LevelSegmenter);
    let mut voices = models.listen(Loud, 0);
    voices.push(&audio(&[(OTHER_LEVEL, 4.0)])).unwrap();
    let stable = voices.timeline.stable_to;
    score.set(Some(0.6));
    voices.push(&audio(&[(OTHER_LEVEL, 2.0)])).unwrap();
    voices.finish();
    assert_eq!(voices.timeline.admission(&(0..stable)), Some(false));
    assert_eq!(voices.timeline.admission(&seconds(4.5, 6.0)), Some(true));
    assert!(matches!(
        voices.timeline.speaking(),
        Some(Speaker::Owner(_))
    ));
}

#[test]
fn a_later_verified_owner_resolves_the_earlier_pending_prefix() {
    let resolved = Cell::new(false);
    let score = Cell::new(Some(0.2));
    let mut models = scored(&score).with_segmenter(Segments(|chunk| {
        if resolved.get() {
            LevelSegmenter.segment(chunk)
        } else {
            ambiguous(chunk)
        }
    }));
    let mut voices = models.listen(Loud, 0);
    voices.push(&audio(&[(OTHER_LEVEL, 5.0)])).unwrap();
    score.set(Some(0.6));
    voices.push(&audio(&[(OTHER_LEVEL, 0.5)])).unwrap();
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Pending(None)));
    resolved.set(true);
    voices.push(&audio(&[(OWN_LEVEL, 2.0)])).unwrap();
    voices.finish();
    assert!(!judgments(&voices).contains(&Judgment::Pending));
    let mut held = Gate::new(Admission::OwnerOnly, 0);
    for (text, span) in [("before", seconds(5.0, 5.5)), ("owner", seconds(6.0, 7.0))] {
        held.hold(Heard {
            segment: crate::transcription::TranscriptSegment::new(text, Some(span.clone()))
                .unwrap(),
            span: Some(span.clone()),
            window: span,
        });
    }
    let decision = held.decide(&voices.timeline);
    let released: Vec<&str> = decision
        .released
        .iter()
        .map(|released| released.heard.segment.as_str())
        .collect();
    let dropped: Vec<&str> = decision
        .dropped
        .iter()
        .map(|heard| heard.segment.as_str())
        .collect();
    assert_eq!(released, ["owner"]);
    assert_eq!(dropped, ["before"]);
}

#[test]
fn filtering_replay_is_invariant_to_push_size_and_absolute_origin() {
    let source = audio(&[(OTHER_LEVEL, 4.0), (0.0, 0.2), (OWN_LEVEL, 3.0)]);
    let mut expected = None;
    for size in [137, 512, source.len()] {
        let mut models = enrolled().with_segmenter(LevelSegmenter);
        let mut voices = models.listen(Loud, 123_456);
        for chunk in source.chunks(size) {
            voices.push(chunk).unwrap();
        }
        voices.finish();
        let state = (
            voices.tracker.timeline.turns,
            voices.tracker.timeline.stable_to,
        );
        if let Some(expected) = &expected {
            assert_eq!(&state, expected);
        } else {
            expected = Some(state);
        }
    }
}

#[test]
fn verifier_failure_is_an_error_instead_of_terminal_optimism() {
    let score = Cell::new(Some(0.2));
    let mut models = scored(&score).with_segmenter(Segments(|_| {
        Err(SegmentationError::Inference {
            backend: "test",
            source: crate::backend::BackendError::Unusable("failed".into()),
        })
    }));
    let mut voices = models.listen(Loud, 0);
    voices.push(&audio(&[(OTHER_LEVEL, 4.0)])).unwrap();
    score.set(Some(0.6));
    voices.push(&audio(&[(OTHER_LEVEL, 1.0)])).unwrap();
    voices.finish();
    assert!(voices.timeline.is_broken());
    assert_eq!(voices.timeline.speaking(), None);
    assert!(
        voices
            .timeline
            .admitted_audio(&audio(&[(OTHER_LEVEL, 5.0)]), 0)
            .is_empty()
    );
}

#[test]
fn terminal_weak_scores_admit_short_speech_without_owner_provenance() {
    for score in [0.3, 0.349] {
        for pause in [false, true] {
            let score = Cell::new(Some(score));
            let mut models = scored(&score);
            let mut voices = models.listen(Loud, 0);
            let stream = audio(&[(OWN_LEVEL, 0.8)]);
            voices.push(&stream).unwrap();
            assert!(matches!(
                voices.timeline.speaking(),
                Some(Speaker::Pending(_))
            ));
            if pause {
                voices.push(&audio(&[(0.0, 1.0)])).unwrap();
            } else {
                voices.finish();
            }
            assert_eq!(voices.timeline.speaking(), Some(Speaker::Unverified));
            assert_eq!(voices.timeline.admitted_audio(&stream, 0), stream);
            assert_eq!(voices.timeline.owner_name(), None);
            assert!(!voices.timeline.admitted_is_owner());
            assert!(voices.open.is_none());
        }
    }
}

#[test]
fn terminal_weak_scores_do_not_erase_an_earlier_low_score() {
    for final_score in [Some(0.33), None] {
        let score = Cell::new(Some(0.29));
        let mut models = scored(&score);
        let mut voices = models.listen(Loud, 0);
        let stream = audio(&[(OWN_LEVEL, 0.5)]);
        voices.push(&stream).unwrap();
        score.set(final_score);
        voices.push(&stream).unwrap();
        voices.finish();
        assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
        assert!(voices.timeline.admitted_audio(&stream, 0).is_empty());
    }
}

#[test]
fn terminal_weak_scores_can_be_followed_by_a_positive_owner_match() {
    let score = Cell::new(Some(0.33));
    let mut models = scored(&score);
    let mut voices = models.listen(Loud, 0);
    let stream = audio(&[(OWN_LEVEL, 0.5)]);
    voices.push(&stream).unwrap();
    score.set(Some(OWN_VOICE_THRESHOLD));
    voices.push(&stream).unwrap();
    voices.finish();
    assert!(matches!(
        voices.timeline.speaking(),
        Some(Speaker::Owner(_))
    ));
    assert!(voices.timeline.admitted_is_owner());
}

#[test]
fn terminal_weak_scores_preserve_the_three_second_rejection_horizon() {
    for length in [
        TURN_JUDGED_SAMPLES - Loud::FRAME_SAMPLES,
        TURN_JUDGED_SAMPLES,
    ] {
        let score = Cell::new(Some(0.33));
        let mut models = scored(&score);
        let mut voices = models.listen(Loud, 0);
        voices.push(&vec![OWN_LEVEL; length]).unwrap();
        if length < TURN_JUDGED_SAMPLES {
            assert!(matches!(
                voices.timeline.speaking(),
                Some(Speaker::Pending(_))
            ));
            voices.finish();
            assert_eq!(voices.timeline.speaking(), Some(Speaker::Unverified));
        } else {
            assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
            assert_eq!(voices.timeline.admission(&seconds(0.0, 1.0)), Some(false));
            voices.push(&audio(&[(OWN_LEVEL, 2.0)])).unwrap();
            assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
            voices.finish();
            assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
        }
    }
}

#[test]
fn terminal_weak_scores_do_not_inherit_an_earlier_turn() {
    enum Earlier {
        Paused,
        Recording,
        Skipped,
    }
    for earlier in [Earlier::Paused, Earlier::Recording, Earlier::Skipped] {
        let score = Cell::new(Some(0.29));
        let mut models = scored(&score);
        let mut voices = models.listen(Loud, 0);
        let stream = audio(&[(OWN_LEVEL, 0.8)]);
        voices.push(&stream).unwrap();
        let origin = 2 * SECOND as u64;
        match earlier {
            Earlier::Paused => {
                voices
                    .push(&vec![0.0; origin as usize - stream.len()])
                    .unwrap();
                assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
            }
            Earlier::Recording => voices = models.listen(Loud, origin),
            Earlier::Skipped => voices.skip_to(origin),
        }
        score.set(Some(0.33));
        voices.push(&stream).unwrap();
        voices.finish();
        assert_eq!(voices.timeline.speaking(), Some(Speaker::Unverified));
        assert_eq!(voices.timeline.admitted_audio(&stream, origin), stream);
    }
}

#[test]
fn terminal_weak_scores_after_a_split_use_the_split_evidence() {
    for (earlier, split_score, expected) in [
        (0.29, 0.33, Speaker::Unverified),
        (0.33, 0.29, Speaker::Other(None)),
    ] {
        let score = Cell::new(Some(earlier));
        let mut models = scored(&score);
        let mut voices = models.listen(Loud, 0);
        voices.push(&audio(&[(OWN_LEVEL, 4.0)])).unwrap();
        let retained = voices.open.as_ref().unwrap().audio.len();
        voices.split(retained - SECOND / 2, Judgment::Pending, Some(split_score));
        score.set(Some(split_score));
        voices.finish();
        assert_eq!(voices.timeline.speaking(), Some(expected));
    }
}

#[test]
fn terminal_weak_scores_do_not_admit_an_enrolled_guest_or_missing_owner() {
    for known_guest in [false, true] {
        let score = Cell::new(Some(0.33));
        let mut models = scored(&score);
        if known_guest {
            models.diarizer.set_enrolled(vec![
                enrolled_voice("alex", Role::Owner, OWN.to_vec()),
                enrolled_voice(
                    "sam",
                    Role::Guest,
                    vec![0.33, (1.0_f32 - 0.33 * 0.33).sqrt()],
                ),
            ]);
        } else {
            models.diarizer.set_enrolled(Vec::new());
        }
        let mut voices = models.listen(Loud, 0);
        let stream = audio(&[(OWN_LEVEL, 0.8)]);
        voices.push(&stream).unwrap();
        voices.finish();
        assert!(matches!(
            voices.timeline.speaking(),
            Some(Speaker::Other(_))
        ));
        assert!(voices.timeline.admitted_audio(&stream, 0).is_empty());
    }
}

#[test]
fn terminal_weak_scores_do_not_include_a_nonfinite_cosine() {
    let score = Cell::new(Some(f32::NAN));
    let mut models = scored(&score);
    let mut voices = models.listen(Loud, 0);
    let stream = audio(&[(OWN_LEVEL, 0.8)]);
    voices.push(&stream).unwrap();
    voices.finish();
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
    assert!(voices.timeline.admitted_audio(&stream, 0).is_empty());
}

#[test]
fn a_guest_needs_the_naming_bar_an_owner_does_not() {
    for (role, named) in [(Role::Owner, true), (Role::Guest, false)] {
        // Past the owner's bar, short of a name's.
        let mut models = scored_by(|_| Some(0.47));
        models
            .diarizer
            .set_enrolled(vec![enrolled_voice("alex", role, OWN.to_vec())]);
        let mut tracker = models.listen(Loud, 0);
        tracker
            .push(&audio(&[(OWN_LEVEL, 1.0), (0.0, 1.0)]))
            .unwrap();

        let name = tracker.timeline.speaker(&seconds(0.0, 1.0));
        assert_eq!(name.is_some(), named, "{role:?}");
    }
}

#[test]
fn the_speaker_is_the_latest_turns_voice_as_judged_so_far() {
    let mut models = enrolled();
    let mut tracker = heard(
        &mut models,
        &[(OWN_LEVEL, 1.0), (0.0, 1.0), (OTHER_LEVEL, 1.0)],
    );
    assert_eq!(
        tracker.timeline.speaking(),
        Some(Speaker::Pending(Some(SpeakerName::new("sam").unwrap())))
    );

    tracker.push(&audio(&[(OTHER_LEVEL, 2.0)])).unwrap();
    let sam = SpeakerName::new("sam").unwrap();
    assert_eq!(tracker.timeline.speaking(), Some(Speaker::Other(Some(sam))));
}

#[test]
fn unscorable_short_speech_is_admitted_without_claiming_a_name() {
    for level in [OWN_LEVEL, OTHER_LEVEL] {
        let stream = audio(&[(level, 0.25)]);
        let mut models = enrolled();
        let mut voices = models.listen(Loud, 0);
        voices.push(&stream).unwrap();
        voices.finish();
        assert_eq!(
            voices.timeline.admitted_audio(&stream, 0).len(),
            stream.len()
        );
        assert_eq!(voices.timeline.admission(&seconds(0.0, 0.25)), Some(true));
        assert_eq!(voices.timeline.speaking(), Some(Speaker::Unverified));
        assert_eq!(voices.timeline.owner_name(), None);
        for timed in [true, false] {
            let window = seconds(0.0, 0.25);
            let mut held = Gate::new(Admission::OwnerOnly, 0);
            let mut segment =
                crate::transcription::TranscriptSegment::new("yeah", Some(window.clone())).unwrap();
            if timed {
                segment = segment.with_word_spans(vec![window.clone()]).unwrap();
            }
            held.hold(Heard {
                segment,
                span: None,
                window,
            });
            let delivered = release(&mut held, &voices.timeline);
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].segment.as_str(), "yeah");
        }
    }
}

#[test]
fn owner_provenance_requires_every_admitted_turn_to_be_verified() {
    let mut models = enrolled();
    let mut verified = heard(
        &mut models,
        &[(OTHER_LEVEL, 1.0), (0.0, 1.0), (OWN_LEVEL, 1.0)],
    );
    assert!(!verified.timeline.admitted_is_owner());
    verified.finish();
    assert!(verified.timeline.admitted_is_owner());
    let mut models = enrolled();
    let mut mixed = heard(
        &mut models,
        &[(OWN_LEVEL, 0.25), (0.0, 1.0), (OWN_LEVEL, 1.0)],
    );
    mixed.finish();
    assert_eq!(judgments(&mixed), [Judgment::Unverified, Judgment::Owner]);
    assert!(!mixed.timeline.admitted_is_owner());
}

#[test]
fn a_bounded_owner_preview_recovers_after_an_earlier_guest() {
    let mut models = enrolled();
    let mut voices = heard(
        &mut models,
        &[(OTHER_LEVEL, 1.0), (0.0, 1.0), (OWN_LEVEL, 2.0)],
    );
    voices.finish();
    let mut held = Gate::new(Admission::OwnerOnly, 0);
    let source = seconds(2.0, 4.0);
    assert_eq!(
        preview(
            &mut held,
            &voices.timeline,
            4 * SECOND as u64,
            "owner words",
            Some(&source)
        )
        .as_deref(),
        Some("owner words")
    );
    assert_eq!(
        preview(
            &mut held,
            &voices.timeline,
            4 * SECOND as u64,
            "unknown words",
            None
        ),
        None
    );
    for source in [seconds(0.0, 4.0), seconds(4.0, 5.0), seconds(3.0, 2.0)] {
        assert_eq!(
            preview(
                &mut held,
                &voices.timeline,
                4 * SECOND as u64,
                "words",
                Some(&source)
            ),
            None
        );
    }
}

#[test]
fn whole_source_admission_preserves_finalized_and_unresolved_states() {
    let mut timeline = Timeline {
        turns: vec![
            Turn {
                span: 2..8,
                judgment: Judgment::Owner,
            },
            Turn {
                span: 12..18,
                judgment: Judgment::Unverified,
            },
        ],
        reached: 20,
        finished: true,
        ..Timeline::default()
    };
    assert_eq!(timeline.wholly_admitted(&(0..20)), Some(true));
    timeline.turns[1].judgment = Judgment::Other;
    assert_eq!(timeline.wholly_admitted(&(0..20)), Some(false));
    timeline.turns[1].judgment = Judgment::Pending;
    assert_eq!(timeline.wholly_admitted(&(0..20)), None);
    timeline.turns[1].judgment = Judgment::Unverified;
    timeline.broken = true;
    assert_eq!(timeline.wholly_admitted(&(0..20)), None);
}

#[test]
fn only_owner_turns_may_leave_the_computer() {
    let timeline = Timeline {
        turns: vec![
            Turn {
                span: 0..4,
                judgment: Judgment::Owner,
            },
            Turn {
                span: 4..6,
                judgment: Judgment::Unverified,
            },
            Turn {
                span: 6..8,
                judgment: Judgment::Other,
            },
        ],
        reached: 8,
        finished: true,
        ..Timeline::default()
    };
    let samples = [1.0, 1.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0];

    assert_eq!(
        timeline.admitted_audio(&samples, 0),
        [1.0, 1.0, 1.0, 1.0, 2.0, 2.0]
    );
    assert_eq!(timeline.owner_audio(&samples, 0), [1.0, 1.0, 1.0, 1.0]);
    assert_eq!(timeline.wholly_owner(&(0..4)), Some(true));
    assert_eq!(timeline.wholly_owner(&(0..6)), Some(false));
}

#[test]
fn whole_source_admission_rejects_reversed_ranges_and_keeps_point_words() {
    let timeline = Timeline {
        turns: vec![Turn {
            span: 2..8,
            judgment: Judgment::Owner,
        }],
        reached: 10,
        finished: true,
        ..Timeline::default()
    };
    assert_eq!(timeline.wholly_admitted(&(3..3)), Some(true));
    assert_eq!(timeline.wholly_admitted(&(9..9)), Some(false));
    assert_eq!(timeline.wholly_admitted(&Range { start: 3, end: 2 }), None);
}

#[test]
fn a_preview_shows_held_words_only_while_their_voice_is_the_owners() {
    for (level, shown) in [(OWN_LEVEL, Some("hello world")), (OTHER_LEVEL, None)] {
        let mut recording = RecordingReader::from_samples(&audio(&[(level, 1.0)]));
        let mut models = enrolled().with_segmenter(LevelSegmenter);
        let Listening { speech, tracker } = models.listen(Loud, 0);
        let mut feed = TurnFeed::start(&mut recording, speech, Some(tracker));
        let mut gate = Gate::new(Admission::OwnerOnly, feed.held_from());
        feed.hear_through(SECOND as u64);
        gate.hold(Heard {
            segment: crate::transcription::TranscriptSegment::new("hello", Some(8_000..16_000))
                .expect("a segment"),
            span: Some(8_000..16_000),
            window: 8_000..16_000,
        });

        let timeline = feed.timeline().expect("a tracker runs");
        let reached = timeline.reached;
        assert!(
            release(&mut gate, timeline).is_empty(),
            "the word waits for a speaker change"
        );
        assert_eq!(
            preview(&mut gate, timeline, reached, "world", None).as_deref(),
            shown,
            "{level}"
        );
    }
}

#[test]
fn a_tracker_begins_its_recording_where_it_says() {
    let mut models = enrolled();
    heard(&mut models, &[(OWN_LEVEL, 1.0)]);

    let mut tracker = models.listen(Loud, 5 * SECOND as u64);
    assert_eq!(tracker.timeline.turns, []);
    tracker.push(&audio(&[(OWN_LEVEL, 1.0)])).unwrap();

    assert_eq!(tracker.timeline.turns[0].span.start, 5 * SECOND as u64);
    assert_eq!(judgments(&tracker), [Judgment::Owner]);
}

#[test]
fn trailing_silence_does_not_change_earlier_checks_with_batch_size() {
    let mut stream = audio(&[(OTHER_LEVEL, 3.0), (OWN_LEVEL, 0.5), (0.0, 0.6)]);
    stream.truncate(stream.len() - 7);
    let mut expected_models = enrolled();
    let mut expected = expected_models.listen(Loud, 0);
    for frame in stream.chunks(Loud::FRAME_SAMPLES) {
        expected.push(frame).unwrap();
    }
    expected.finish();
    assert_eq!(expected.timeline.admission(&seconds(3.0, 3.5)), Some(true));
    for size in [1, 333, 1_600, stream.len()] {
        let mut models = enrolled();
        let mut actual = models.listen(Loud, 0);
        for piece in stream.chunks(size) {
            actual.push(piece).unwrap();
        }
        actual.finish();
        assert_eq!(
            actual.timeline.turns, expected.timeline.turns,
            "batch {size}"
        );
        assert_eq!(
            actual.timeline.names, expected.timeline.names,
            "batch {size}"
        );
    }
}

#[test]
fn a_naming_check_cannot_judge_admission_before_its_change_window() {
    let mut models = scored_by(|window| {
        Some(match window.len() {
            CHECK_EVERY_SAMPLES => 0.4,
            SECOND => 0.5,
            _ => 0.0,
        })
    });
    let mut voices = models.listen(Loud, 0);
    voices.push(&audio(&[(OTHER_LEVEL, 1.0)])).unwrap();
    let word = seconds(0.1, 0.4);
    assert_eq!(voices.timeline.admission(&word), None);
    voices.push(&audio(&[(OTHER_LEVEL, 0.5)])).unwrap();
    voices.finish();
    assert_eq!(voices.timeline.admission(&word), Some(false));
}

#[test]
fn a_silence_trimmed_change_cannot_revise_released_audio() {
    let changed = Cell::new(false);
    let mut models = scored_by(|_| Some(if changed.get() { 0.0 } else { 1.0 }));
    let mut voices = models.listen(Loud, 0);
    for frame in audio(&[(OWN_LEVEL, 3.5)]).chunks(Loud::FRAME_SAMPLES) {
        voices.push(frame).unwrap();
    }
    let released = seconds(2.85, 2.95);
    assert_eq!(voices.timeline.admission(&released), Some(true));
    let mut held = Gate::new(Admission::OwnerOnly, 0);
    held.hold(Heard {
        segment: crate::transcription::TranscriptSegment::new("owner", Some(released.clone()))
            .unwrap(),
        span: Some(released.clone()),
        window: released.clone(),
    });
    assert_eq!(release(&mut held, &voices.timeline).len(), 1);
    changed.set(true);
    for frame in audio(&[(OTHER_LEVEL, 0.3), (0.0, 0.2)]).chunks(Loud::FRAME_SAMPLES) {
        voices.push(frame).unwrap();
    }
    voices.finish();
    assert_eq!(voices.timeline.admission(&released), Some(true));
}

#[test]
fn a_late_detector_failure_invalidates_owner_audio() {
    struct FailsAfterOwner(usize);
    impl VoiceActivityDetector for FailsAfterOwner {
        const FRAME_SAMPLES: usize = 160;
        fn probability(&mut self, _: &[i16]) -> Result<f32, VoiceActivityError> {
            self.0 += 1;
            if self.0 > 50 {
                Err(VoiceActivityError::InvalidProbability(f32::NAN))
            } else {
                Ok(1.0)
            }
        }
    }
    let mut models = scored_by(|_| Some(1.0));
    let mut voices = models.listen(FailsAfterOwner(0), 0);
    let stream = audio(&[(OWN_LEVEL, 0.5)]);
    voices.push(&stream).unwrap();
    assert!(!voices.timeline.admitted_audio(&stream, 0).is_empty());
    assert!(voices.push(&[OWN_LEVEL; 160]).is_err());
    voices.finish();
    assert!(voices.timeline.admitted_audio(&stream, 0).is_empty());
}

#[test]
fn embedding_failures_invalidate_admission_before_or_after_an_owner_passes() {
    struct Failing(usize);
    impl Embedder for Failing {
        fn embed(&mut self, _: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
            if self.0 == 0 {
                return Err(SpeakerError::Inference {
                    backend: "test",
                    source: crate::backend::BackendError::Unusable("embedding failed".into()),
                });
            }
            self.0 -= 1;
            Ok(Some(OWN.to_vec()))
        }

        fn dimension(&self) -> usize {
            2
        }
    }
    for successful_checks in [0, 1] {
        let mut models = Models::new(Diarizer::new(Failing(successful_checks), owner()));
        let mut voices = models.listen(Loud, 0);
        let stream = audio(&[(OWN_LEVEL, 0.5), (OTHER_LEVEL, 2.0)]);
        voices.push(&stream[..SECOND / 2]).unwrap();
        if successful_checks == 1 {
            assert!(!voices.timeline.admitted_audio(&stream, 0).is_empty());
        }
        voices.push(&stream[SECOND / 2..]).unwrap();
        voices.finish();
        assert!(voices.timeline.is_broken(), "{successful_checks}");
        assert!(voices.timeline.admitted_audio(&stream, 0).is_empty());
        assert_eq!(voices.timeline.speaker(&seconds(0.0, 0.5)), None);
        assert_eq!(voices.timeline.speaking(), None);
        assert_eq!(voices.timeline.owner_name(), None);
    }
}

#[test]
fn naming_tracks_a_change_between_people_with_the_same_role() {
    for role in [Role::Guest, Role::Owner] {
        let mut models = Models::new(Diarizer::new(
            VoiceByLevel,
            vec![
                enrolled_voice("alex", role, OWN.to_vec()),
                enrolled_voice("sam", role, OTHER.to_vec()),
            ],
        ));
        let mut voices = models.listen(Loud, 0);
        voices.push(&audio(&[(OWN_LEVEL, 0.5)])).unwrap();
        let name = Some(SpeakerName::new("alex").unwrap());
        let speaking = match role {
            Role::Owner => Speaker::Owner(name),
            Role::Guest => Speaker::Pending(name),
        };
        assert_eq!(voices.timeline.speaking(), Some(speaking));
        for frame in audio(&[(OWN_LEVEL, 3.5), (OTHER_LEVEL, 3.0)]).chunks(Loud::FRAME_SAMPLES) {
            voices.push(frame).unwrap();
        }
        voices.finish();
        assert_eq!(
            voices.timeline.turns.len(),
            1,
            "identity does not split admission"
        );
        assert_eq!(
            voices
                .timeline
                .speaker(&seconds(6.0, 7.0))
                .map(SpeakerName::as_str),
            Some("sam"),
            "{role:?}"
        );
        assert_eq!(
            voices
                .timeline
                .speaker(&seconds(0.0, 1.0))
                .map(SpeakerName::as_str),
            Some("alex"),
            "{role:?}"
        );
    }
}

#[test]
fn naming_follows_enrolled_and_unknown_voices_without_a_role_change() {
    for role in [Role::Owner, Role::Guest] {
        for (first, last, expected) in [
            (OWN_LEVEL, OTHER_LEVEL, None),
            (OTHER_LEVEL, OWN_LEVEL, Some("alex")),
        ] {
            let mut models = Models::new(Diarizer::new(
                VoiceByLevel,
                vec![enrolled_voice("alex", role, OWN.to_vec())],
            ));
            let mut voices = models.listen(Loud, 0);
            voices.push(&audio(&[(first, 30.0), (last, 5.0)])).unwrap();
            voices.finish();
            assert_eq!(
                voices
                    .timeline
                    .speaker(&seconds(34.0, 35.0))
                    .map(SpeakerName::as_str),
                expected,
                "{role:?}: {first} -> {last}"
            );
        }
    }
}

#[test]
fn source_admission_does_not_rescue_missing_or_rejected_evidence() {
    let owner = Timeline {
        turns: vec![Turn {
            span: 6_144..24_064,
            judgment: Judgment::Owner,
        }],
        reached: 25_600,
        finished: true,
        ..Timeline::default()
    };
    let mut mixed = owner.clone();
    mixed.turns.push(Turn {
        span: 24_064..25_600,
        judgment: Judgment::Other,
    });
    let silent = Timeline {
        turns: Vec::new(),
        ..owner.clone()
    };
    let broken = Timeline {
        broken: true,
        ..owner.clone()
    };
    for (timeline, word, source) in [
        (owner.clone(), 2_560..5_120, None),
        (owner.clone(), 25_600..25_600, Some(0..25_600)),
        (owner.clone(), 25_600..26_000, Some(0..26_000)),
        (owner, 2_560..5_120, Some(0..26_000)),
        (mixed.clone(), 2_560..5_120, Some(0..25_600)),
        (mixed, 24_064..25_600, Some(0..25_600)),
        (silent, 2_560..5_120, Some(0..25_600)),
        (broken, 2_560..5_120, Some(0..25_600)),
    ] {
        let mut held = Gate::new(Admission::OwnerOnly, 0);
        held.hold(timed_heard(word, source));
        let decision = held.decide(&timeline);
        assert!(decision.released.is_empty());
        assert_eq!(decision.dropped.len(), 1);
    }
}

#[test]
fn source_admission_does_not_reach_before_the_tracking_origin() {
    let timeline = Timeline {
        turns: vec![Turn {
            span: 20_000..30_000,
            judgment: Judgment::Owner,
        }],
        reached: 32_000,
        finished: true,
        ..Timeline::default()
    };
    let mut held = Gate::new(Admission::OwnerOnly, 16_000);
    held.hold(timed_heard(2_560..5_120, Some(0..32_000)));

    assert!(release(&mut held, &timeline).is_empty());

    let mut held = Gate::new(Admission::OwnerOnly, 16_000);
    held.hold(timed_heard(17_000..18_000, Some(16_000..32_000)));
    assert_eq!(release(&mut held, &timeline).len(), 1);
}

#[test]
fn source_admission_waits_for_its_pending_turn() {
    let mut timeline = Timeline {
        turns: vec![Turn {
            span: 6_144..24_064,
            judgment: Judgment::Pending,
        }],
        reached: 25_600,
        open: true,
        stable_to: 5_120,
        ..Timeline::default()
    };
    let mut held = Gate::new(Admission::OwnerOnly, 0);
    held.hold(timed_heard(2_560..5_120, Some(0..25_600)));
    assert!(release(&mut held, &timeline).is_empty());

    timeline.turns[0].judgment = Judgment::Unverified;
    timeline.open = false;
    timeline.finished = true;
    assert_eq!(release(&mut held, &timeline).len(), 1);
}

fn timed_heard(word: Range<u64>, source: Option<Range<u64>>) -> Heard {
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

#[test]
fn sentence_punctuation_does_not_move_an_owner_word_into_a_guest_turn() {
    use crate::transcription::{TimedWord, TranscriptSegment, word_timings};

    let timeline = Timeline {
        turns: vec![
            Turn {
                span: seconds(0.0, 0.3),
                judgment: Judgment::Owner,
            },
            Turn {
                span: seconds(0.3, 1.0),
                judgment: Judgment::Other,
            },
        ],
        reached: SECOND as u64,
        finished: true,
        ..Timeline::default()
    };
    let words = word_timings([
        (
            " own",
            TimedWord {
                start: 0.0,
                end: 0.2,
            },
        ),
        (
            "er",
            TimedWord {
                start: 0.2,
                end: 0.4,
            },
        ),
        (
            ".",
            TimedWord {
                start: 0.4,
                end: 0.8,
            },
        ),
        (
            " guest",
            TimedWord {
                start: 0.8,
                end: 1.0,
            },
        ),
    ]);
    let segment = TranscriptSegment::new("owner. guest", Some(seconds(0.0, 1.0)))
        .unwrap()
        .with_word_spans(
            words
                .iter()
                .map(|word| word.span_from(0).unwrap())
                .collect(),
        )
        .unwrap();
    let mut held = Gate::new(Admission::OwnerOnly, 0);
    held.hold(Heard {
        span: segment.span(),
        window: seconds(0.0, 1.0),
        segment,
    });
    let released = release(&mut held, &timeline);
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].segment.as_str(), "owner.");
}

#[test]
fn skipped_audio_is_unnamed_and_never_assigned_to_the_owner() {
    let stream = audio(&[(OWN_LEVEL, 4.0)]);
    let (heard_to, skipped_to) = (SECOND / 2 + 7, 3 * SECOND);
    let mut models = enrolled();
    let mut voices = models.listen(Loud, 0);
    voices.push(&stream[..heard_to]).unwrap();
    assert_eq!(voices.heard_to(), SECOND as u64 / 2 + 7);
    assert_eq!(voices.timeline.reached(), SECOND as u64 / 2);
    // The detector hears what the tracker skips.
    voices.speech.hear(&stream[heard_to..skipped_to]).unwrap();
    SpeakerTracker::skip_to(&mut voices.tracker, voices.speech.map(), skipped_to as u64);
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
    voices.push(&stream[skipped_to..]).unwrap();
    voices.finish();
    let timeline = &voices.timeline;
    assert_eq!(timeline.admission(&seconds(1.0, 2.0)), Some(false));
    assert_eq!(timeline.speaker(&seconds(1.0, 2.0)), None);
    assert_eq!(timeline.admission(&seconds(3.0, 4.0)), Some(true));
}

#[test]
fn a_feed_without_a_tracker_frees_what_the_detector_heard() {
    let mut recording = RecordingReader::from_samples(&[0.5; SECOND]);
    let speech = SpeechDetector::new(Loud, 0);
    let mut feed = TurnFeed::start(
        &mut recording,
        speech,
        None::<VoiceprintTracker<'_, VoiceByLevel>>,
    );

    assert_eq!(feed.hear_ahead(Duration::from_millis(100)), Ahead::Heard);

    assert_eq!(feed.held_from(), SECOND as u64);
}

#[test]
fn a_feed_whose_tracker_breaks_still_ends() {
    // The detector hears NaN as silence, but the segmenter cannot keep it
    // as evidence, so the timeline breaks on the first frame.
    let mut recording = RecordingReader::from_samples(&[f32::NAN; SECOND]);
    let mut models = enrolled().with_segmenter(LevelSegmenter);
    let tracker = VoiceprintTracker::new(&mut models.diarizer, models.segmenter.as_mut(), 0)
        .expect("the segmenter's shape is valid");
    let mut feed = TurnFeed::start(&mut recording, SpeechDetector::new(Loud, 0), Some(tracker));

    let mut steps = 0;
    while feed.hear_ahead(Duration::from_millis(100)) != Ahead::Ended {
        steps += 1;
        assert!(steps < 100, "the feed keeps hearing nothing");
    }

    assert!(feed.timeline().expect("a tracker runs").is_broken());
}
