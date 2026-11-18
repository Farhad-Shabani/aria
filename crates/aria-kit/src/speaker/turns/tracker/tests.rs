use std::cell::Cell;

use super::*;
use crate::speaker::turns::testing::*;
use crate::speaker::turns::{Admission, Gate, Speaker};
use crate::speaker::{LevelSegmenter, Profile, Role, SpeakerError, enrolled_voice};
use crate::voice_activity::VoiceActivityError;
use crate::voice_activity::{SpeechDetector, VoiceActivityDetector};

fn heard<'a>(
    models: &'a mut Models<VoiceByLevel>,
    parts: &[(f32, f32)],
) -> Listening<'a, Loud, VoiceByLevel, NoSegmenter> {
    let mut tracker = models.listen(Loud, 0);
    tracker.push(&audio(parts)).expect("the detector scores");
    tracker
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
fn the_owner_taking_over_a_doubted_turn_leaves_the_voice_before_out() {
    let mut models = enrolled().with_segmenter(LevelSegmenter);
    let mut voices = models.listen(Loud, 0);
    voices
        .push(&audio(&[(OTHER_LEVEL, 1.5), (0.0, 0.2), (OWN_LEVEL, 2.5)]))
        .unwrap();
    voices.finish();

    assert_eq!(judgments(&voices), [Judgment::Other, Judgment::Owner]);
    assert_eq!(voices.timeline.admission(&seconds(0.0, 1.5)), Some(false));
    assert_eq!(voices.timeline.admission(&seconds(2.0, 4.0)), Some(true));
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
    let mut voices = ambiguous_tracker(&mut models, &score);
    resolved.set(true);
    voices.push(&audio(&[(OWN_LEVEL, 2.0)])).unwrap();
    voices.finish();

    assert!(!judgments(&voices).contains(&Judgment::Pending));
    assert_eq!(voices.timeline.admission(&seconds(5.0, 5.5)), Some(false));
    assert_eq!(voices.timeline.admission(&seconds(6.0, 7.0)), Some(true));
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
    let score = Cell::new(Some(0.15));
    let mut models = scored(&score);
    let mut voices = models.listen(Loud, 0);
    let stream = audio(&[(OWN_LEVEL, 1.0)]);
    voices.push(&stream).unwrap();
    score.set(Some(0.33));
    voices.push(&audio(&[(OWN_LEVEL, 0.5)])).unwrap();
    voices.finish();
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
    assert!(voices.timeline.admitted_audio(&stream, 0).is_empty());
}

#[test]
fn a_weak_score_rejects_once_its_length_reads_it_as_another_voice() {
    for (seconds_heard, closed) in [(1.0, Speaker::Unverified), (1.5, Speaker::Other(None))] {
        let score = Cell::new(Some(0.3));
        let mut models = scored(&score);
        let mut voices = models.listen(Loud, 0);
        voices.push(&audio(&[(OWN_LEVEL, seconds_heard)])).unwrap();
        assert!(matches!(
            voices.timeline.speaking(),
            Some(Speaker::Pending(_))
        ));
        voices.finish();
        assert_eq!(voices.timeline.speaking(), Some(closed));
    }
}

#[test]
fn a_short_turn_after_the_owners_is_another_voice_only_when_the_segmenter_hears_a_change() {
    for (level, settled) in [
        (OTHER_LEVEL, Judgment::Other),
        (OWN_LEVEL, Judgment::Unverified),
    ] {
        let mut models = scored_by(|window| {
            Some(if window.iter().any(|sample| *sample >= 0.4) {
                0.9
            } else {
                0.25
            })
        })
        .with_segmenter(LevelSegmenter);
        let mut voices = models.listen(Loud, 0);
        voices
            .push(&audio(&[(OWN_LEVEL, 2.0), (0.0, 1.0), (level, 0.4)]))
            .unwrap();
        voices.finish();

        assert_eq!(judgments(&voices), [Judgment::Owner, settled]);
    }
}

#[test]
fn the_owners_turn_that_dips_once_before_passing_is_admitted_whole() {
    let checks = Cell::new(0);
    let mut models = scored_by(|_| {
        let check = checks.get();
        checks.set(check + 1);
        Some([0.2, 0.2, 0.33, 0.38].get(check).copied().unwrap_or(0.6))
    })
    .with_segmenter(LevelSegmenter);
    let mut voices = models.listen(Loud, 0);
    voices
        .push(&audio(&[(OWN_LEVEL, 3.0), (0.0, 1.0)]))
        .unwrap();
    voices.finish();

    assert_eq!(voices.timeline.admission(&seconds(0.0, 3.0)), Some(true));
    assert!(
        voices
            .timeline
            .turns
            .iter()
            .all(|turn| turn.judgment == Judgment::Owner || turn.span.is_empty())
    );
}

#[test]
fn a_turn_not_passed_by_three_seconds_is_another_voice_before_it_closes() {
    let score = Cell::new(Some(0.33));
    let mut models = scored(&score);
    let mut voices = models.listen(Loud, 0);
    voices.push(&vec![OWN_LEVEL; TURN_JUDGED_SAMPLES]).unwrap();
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
    assert_eq!(voices.timeline.admission(&seconds(0.0, 1.0)), Some(false));
    voices.push(&audio(&[(OWN_LEVEL, 2.0)])).unwrap();
    voices.finish();
    assert_eq!(voices.timeline.speaking(), Some(Speaker::Other(None)));
}

#[test]
fn terminal_weak_scores_do_not_admit_an_enrolled_guest() {
    let score = Cell::new(Some(0.3));
    let mut models = scored(&score);
    models.diarizer.set_enrolled(vec![
        enrolled_voice("alex", Role::Owner, OWN.to_vec()),
        enrolled_voice("sam", Role::Guest, vec![0.3, (1.0_f32 - 0.3 * 0.3).sqrt()]),
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
fn a_pass_short_of_stable_cannot_admit_audio_before_its_change_window() {
    let mut models = scored_by(|window| Some(if window.len() == SECOND { 0.4 } else { 0.0 }));
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
            if self.0 > 100 {
                Err(VoiceActivityError::InvalidProbability(f32::NAN))
            } else {
                Ok(1.0)
            }
        }
    }
    let mut models = scored_by(|_| Some(1.0));
    let mut voices = models.listen(FailsAfterOwner(0), 0);
    let stream = audio(&[(OWN_LEVEL, 1.0)]);
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
    let mut models = Models::new(Diarizer::new(Failing(2), owner()));
    let mut voices = models.listen(Loud, 0);
    let stream = audio(&[(OWN_LEVEL, 1.0), (OTHER_LEVEL, 2.0)]);
    voices.push(&stream[..SECOND]).unwrap();
    assert!(!voices.timeline.admitted_audio(&stream, 0).is_empty());
    voices.push(&stream[SECOND..]).unwrap();
    voices.finish();
    assert!(voices.timeline.is_broken());
    assert!(voices.timeline.admitted_audio(&stream, 0).is_empty());
    assert_eq!(voices.timeline.speaker(&seconds(0.0, 1.0)), None);
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
        voices.push(&audio(&[(OWN_LEVEL, 1.0)])).unwrap();
        let name = Some(SpeakerName::new("alex").unwrap());
        let speaking = match role {
            Role::Owner => Speaker::Owner(name),
            Role::Guest => Speaker::Pending(name),
        };
        assert_eq!(voices.timeline.speaking(), Some(speaking));
        for frame in audio(&[(OWN_LEVEL, 3.0), (OTHER_LEVEL, 3.0)]).chunks(Loud::FRAME_SAMPLES) {
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
