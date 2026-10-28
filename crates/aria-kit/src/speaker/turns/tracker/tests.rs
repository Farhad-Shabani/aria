use std::cell::Cell;

use super::*;
use crate::audio::RecordingReader;
use crate::speaker::turns::{Admission, Ahead, Gate, Speaker, TurnFeed};
use crate::speaker::{LevelSegmenter, Profile, Role, SpeakerError, enrolled_voice};
use crate::transcription::Heard;
use crate::voice_activity::VoiceActivityError;
use crate::voice_activity::{SpeechDetector, VoiceActivityDetector};

/// The models each recording's tracker borrows. Its trackers split a turn
/// at a speaker change unchecked.
struct Models<E> {
    diarizer: Diarizer<E>,
}

impl<E: Embedder> Models<E> {
    fn new(diarizer: Diarizer<E>) -> Self {
        Self { diarizer }
    }

    /// Trackers verify each speaker change, as "Only type my voice" does.
    fn with_segmenter<S: Segmenter>(self, segmenter: S) -> OwnerModels<E, S> {
        OwnerModels {
            diarizer: self.diarizer,
            segmenter,
        }
    }

    /// A detector and tracker for one recording, starting at `at`.
    fn listen<V: VoiceActivityDetector>(
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
struct OwnerModels<E, S> {
    diarizer: Diarizer<E>,
    segmenter: S,
}

impl<E: Embedder, S: Segmenter> OwnerModels<E, S> {
    /// A detector and tracker for one recording, starting at `at`.
    fn listen<V: VoiceActivityDetector>(&mut self, vad: V, at: u64) -> Listening<'_, V, E, &mut S> {
        Listening {
            speech: SpeechDetector::new(vad, at),
            tracker: VoiceprintTracker::owner_only(&mut self.diarizer, &mut self.segmenter, at)
                .expect("an owner is enrolled and the segmenter's shape is valid"),
        }
    }
}

/// A speech detector and the tracker that reads its map. Each push is
/// heard by the detector first, as the feed does.
struct Listening<'a, V, E, S> {
    speech: SpeechDetector<V>,
    tracker: VoiceprintTracker<'a, E, S>,
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
    fn push(&mut self, samples: &[f32]) -> Result<(), VoiceActivityError> {
        let heard = self.speech.hear(samples);
        SpeakerTracker::hear(&mut self.tracker, self.speech.map(), samples);
        heard
    }
}

fn release(gate: &mut Gate, timeline: &Timeline) -> Vec<Heard> {
    gate.decide(timeline, None)
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
    gate.decide(timeline, None)
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
struct Loud;

impl VoiceActivityDetector for Loud {
    const FRAME_SAMPLES: usize = 160;

    fn probability(&mut self, samples: &[i16]) -> Result<f32, VoiceActivityError> {
        let loud = samples.iter().any(|sample| sample.unsigned_abs() > 1_000);
        Ok(if loud { 1.0 } else { 0.0 })
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

fn judgments<V, E, S>(tracker: &Listening<'_, V, E, S>) -> Vec<Judgment> {
    tracker
        .timeline
        .turns
        .iter()
        .map(|turn| turn.judgment)
        .collect()
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
fn ambiguous_tracker<'a, E: Embedder, S: Segmenter>(
    models: &'a mut OwnerModels<E, S>,
    score: &Cell<Option<f32>>,
) -> Listening<'a, Loud, E, &'a mut S> {
    let mut voices = models.listen(Loud, 0);
    voices.push(&audio(&[(OTHER_LEVEL, 5.0)])).unwrap();
    score.set(Some(0.6));
    voices.push(&audio(&[(OTHER_LEVEL, 0.5)])).unwrap();
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Pending(None)));
    voices
}

#[test]
fn unresolved_changes_are_optimistic_only_when_the_turn_ends_within_budget() {
    for (after, expected) in [(1.5, Judgment::Unverified), (2.5, Judgment::Other)] {
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
    held.hold(heard_text("before", seconds(5.0, 5.5)));
    held.hold(heard_text("owner", seconds(6.0, 7.0)));
    let decision = held.decide(&voices.timeline, None);
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
fn filtering_replay_is_invariant_to_push_size() {
    let origin = 123_456;
    let replacement = audio(&[(OTHER_LEVEL, 4.0), (0.0, 0.2), (OWN_LEVEL, 3.0)]);
    // Ends partway through a frame of trailing silence.
    let mut trailing_silence = audio(&[(OTHER_LEVEL, 3.0), (OWN_LEVEL, 0.5), (0.0, 0.6)]);
    trailing_silence.truncate(trailing_silence.len() - 7);
    let owner_words = seconds(3.0, 3.5);
    let owner_words = origin + owner_words.start..origin + owner_words.end;
    for (source, sizes, admitted) in [
        (&replacement, vec![137, 512, replacement.len()], None),
        (&trailing_silence, vec![1, 333], Some(owner_words)),
    ] {
        let mut expected = None;
        for size in sizes {
            let mut models = enrolled().with_segmenter(LevelSegmenter);
            let mut voices = models.listen(Loud, origin);
            for chunk in source.chunks(size) {
                voices.push(chunk).unwrap();
            }
            voices.finish();
            if let Some(admitted) = &admitted {
                assert_eq!(voices.timeline.admission(admitted), Some(true));
            }
            let state = (
                voices.tracker.timeline.turns,
                voices.tracker.timeline.stable_to,
                voices.tracker.timeline.labels,
            );
            if let Some(expected) = &expected {
                assert_eq!(&state, expected);
            } else {
                expected = Some(state);
            }
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
    for pause in [false, true] {
        let score = Cell::new(Some(0.3));
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

#[test]
fn terminal_weak_scores_do_not_erase_an_earlier_low_score() {
    let score = Cell::new(Some(0.29));
    let mut models = scored(&score);
    let mut voices = models.listen(Loud, 0);
    let stream = audio(&[(OWN_LEVEL, 0.5)]);
    voices.push(&stream).unwrap();
    score.set(Some(0.33));
    voices.push(&stream).unwrap();
    voices.finish();
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
    assert!(voices.timeline.admitted_audio(&stream, 0).is_empty());
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
fn terminal_weak_scores_do_not_admit_an_enrolled_guest() {
    let score = Cell::new(Some(0.33));
    let mut models = scored(&score);
    models.diarizer.set_enrolled(vec![
        enrolled_voice("alex", Role::Owner, OWN.to_vec()),
        enrolled_voice(
            "sam",
            Role::Guest,
            vec![0.33, (1.0_f32 - 0.33 * 0.33).sqrt()],
        ),
    ]);
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
fn unscorable_speech_is_admitted_without_claiming_a_name() {
    for (level, seconds_heard) in [(OWN_LEVEL, 0.25), (OTHER_LEVEL, 0.25), (FAINT_LEVEL, 3.0)] {
        let stream = audio(&[(level, seconds_heard)]);
        let mut models = enrolled();
        let mut voices = models.listen(Loud, 0);
        voices.push(&stream).unwrap();
        assert_eq!(judgments(&voices), [Judgment::Pending]);
        voices.finish();
        assert_eq!(judgments(&voices), [Judgment::Unverified]);
        assert_eq!(
            voices.timeline.admitted_audio(&stream, 0).len(),
            stream.len()
        );
        assert_eq!(voices.timeline.admission(&seconds(0.0, 0.25)), Some(true));
        assert_eq!(voices.timeline.speaking(), Some(Speaker::Unverified));
        assert_eq!(voices.timeline.owner_name(), None);
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
fn spoken_admission_drops_segments_without_speech_under_them() {
    let mut speech = SpeechDetector::new(Loud, 0);
    speech
        .hear(&audio(&[(OWN_LEVEL, 1.0), (0.0, 2.0)]))
        .unwrap();
    let mut gate = Gate::new(Admission::Spoken, 0);
    gate.hold(heard_text("hello", seconds(0.0, 1.0)));
    // Silent, but the segment before it held speech.
    gate.hold(heard_text("tail", seconds(1.0, 1.5)));
    gate.hold(heard_text("noise", seconds(1.5, 2.5)));
    // Past what the detector has scored.
    gate.hold(heard_text("later", seconds(2.5, 3.5)));

    let decision = gate.decide(&Timeline::default(), Some(speech.map()));
    let texts = |segments: Vec<&Heard>| -> Vec<String> {
        segments
            .into_iter()
            .map(|heard| heard.segment.as_str().to_owned())
            .collect()
    };

    assert_eq!(
        texts(
            decision
                .released
                .iter()
                .map(|released| &released.heard)
                .collect()
        ),
        ["hello", "tail"]
    );
    assert_eq!(texts(decision.dropped.iter().collect()), ["noise"]);
    assert!(gate.undecided_from().is_some());
}

#[test]
fn forgetting_keeps_the_latest_turn_and_what_comes_after() {
    let mut timeline = Timeline {
        turns: vec![
            Turn {
                span: 0..4,
                judgment: Judgment::Owner,
            },
            Turn {
                span: 4..8,
                judgment: Judgment::Other,
            },
        ],
        labels: vec![LabeledTurn {
            span: 0..8,
            label: None,
        }],
        reached: 8,
        ..Timeline::default()
    };

    timeline.forget_before(6);
    assert_eq!(timeline.turns.len(), 1);
    assert_eq!(timeline.admission(&(5..7)), Some(false));
    timeline.forget_before(100);
    assert_eq!(timeline.turns.len(), 1);
    assert_eq!(timeline.labels.len(), 1);
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
fn whole_source_admission_keeps_point_words() {
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
        gate.hold(heard_text("hello", 8_000..16_000));

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
    held.hold(heard_text("owner", released.clone()));
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
fn embedding_failures_invalidate_admission_after_an_owner_passes() {
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
    let mut models = Models::new(Diarizer::new(Failing(1), owner()));
    let mut voices = models.listen(Loud, 0);
    let stream = audio(&[(OWN_LEVEL, 0.5), (OTHER_LEVEL, 2.0)]);
    voices.push(&stream[..SECOND / 2]).unwrap();
    assert!(!voices.timeline.admitted_audio(&stream, 0).is_empty());
    voices.push(&stream[SECOND / 2..]).unwrap();
    voices.finish();
    assert!(voices.timeline.is_broken());
    assert!(voices.timeline.admitted_audio(&stream, 0).is_empty());
    assert_eq!(voices.timeline.speaker(&seconds(0.0, 0.5)), None);
    assert_eq!(voices.timeline.speaking(), None);
    assert_eq!(voices.timeline.owner_name(), None);
}

#[test]
fn naming_tracks_a_change_between_people_with_the_same_role() {
    for (role, second) in [
        (Role::Guest, Some("sam")),
        (Role::Owner, Some("sam")),
        (Role::Guest, None),
    ] {
        let mut profiles = vec![enrolled_voice("alex", role, OWN.to_vec())];
        if let Some(second) = second {
            profiles.push(enrolled_voice(second, role, OTHER.to_vec()));
        }
        let mut models = Models::new(Diarizer::new(VoiceByLevel, profiles));
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
            second,
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
fn a_numbering_tracker_numbers_voices_nobody_enrolled_and_splits_them_without_a_pause() {
    let mut diarizer = Diarizer::new(VoiceByLevel, vec![]);
    let mut speech = SpeechDetector::new(Loud, 0);
    let mut tracker = VoiceprintTracker::numbering(&mut diarizer, 0);
    let samples = audio(&[
        (OWN_LEVEL, 4.0),
        (OTHER_LEVEL, 4.0),
        (0.0, 1.0),
        (OWN_LEVEL, 2.0),
    ]);
    speech.hear(&samples).unwrap();
    SpeakerTracker::hear(&mut tracker, speech.map(), &samples);
    tracker.finish();

    let label = |from, to| {
        tracker
            .timeline
            .label(&seconds(from, to))
            .map(Label::to_string)
    };
    assert_eq!(label(0.0, 1.0).as_deref(), Some("Speaker 1"));
    assert_eq!(label(6.0, 7.0).as_deref(), Some("Speaker 2"));
    assert_eq!(label(9.5, 10.5).as_deref(), Some("Speaker 1"));
    let embedding = |from, to| tracker.timeline.embedding(&seconds(from, to)).unwrap();
    assert!(embedding(6.0, 7.0)[1] > 0.9);
    assert!(embedding(0.0, 1.0)[0] > 0.9);
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
        (owner, 2_560..5_120, Some(0..26_000)),
        (mixed, 2_560..5_120, Some(0..25_600)),
        (silent, 2_560..5_120, Some(0..25_600)),
        (broken, 2_560..5_120, Some(0..25_600)),
    ] {
        let mut held = Gate::new(Admission::OwnerOnly, 0);
        held.hold(timed_heard(word, source));
        let decision = held.decide(&timeline, None);
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
    assert!(held.undecided_from().is_some());

    timeline.turns[0].judgment = Judgment::Unverified;
    timeline.open = false;
    timeline.finished = true;
    assert_eq!(release(&mut held, &timeline).len(), 1);
}

fn heard_text(text: &str, window: Range<u64>) -> Heard {
    Heard {
        segment: crate::transcription::TranscriptSegment::new(text, Some(window.clone())).unwrap(),
        span: Some(window.clone()),
        window,
    }
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

    assert_eq!(
        feed.hear_ahead(Duration::from_millis(100), None),
        Ahead::Heard
    );

    assert_eq!(feed.held_from(), SECOND as u64);
}

#[test]
fn a_feed_whose_tracker_breaks_still_ends() {
    // The detector hears NaN as silence, but the segmenter cannot keep it
    // as evidence, so the timeline breaks on the first frame.
    let mut recording = RecordingReader::from_samples(&[f32::NAN; SECOND]);
    let mut models = enrolled();
    let tracker = VoiceprintTracker::owner_only(&mut models.diarizer, LevelSegmenter, 0)
        .expect("an owner is enrolled");
    let mut feed = TurnFeed::start(&mut recording, SpeechDetector::new(Loud, 0), Some(tracker));

    let mut steps = 0;
    while feed.hear_ahead(Duration::from_millis(100), None) != Ahead::Ended {
        steps += 1;
        assert!(steps < 100, "the feed keeps hearing nothing");
    }

    assert!(feed.timeline().expect("a tracker runs").is_broken());
}

#[test]
fn a_tracker_further_behind_than_the_lag_allows_skips_to_the_detector() {
    let piece = Duration::from_millis(100);
    for (max_lag, reached) in [
        (None, canonical_samples(piece)),
        (Some(Duration::from_secs(1)), 3 * SECOND as u64),
    ] {
        let mut recording = RecordingReader::from_samples(&[OWN_LEVEL; 3 * SECOND]);
        let mut diarizer = Diarizer::new(VoiceByLevel, vec![]);
        let tracker = VoiceprintTracker::numbering(&mut diarizer, 0);
        let mut feed = TurnFeed::start(&mut recording, SpeechDetector::new(Loud, 0), Some(tracker));

        feed.hear_ahead(piece, max_lag);

        let timeline = feed.timeline().expect("a tracker runs");
        assert_eq!(timeline.reached(), reached, "{max_lag:?}");
    }
}
