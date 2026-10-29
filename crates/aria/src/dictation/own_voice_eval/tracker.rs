use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use aria_kit::audio::{CANONICAL_SAMPLE_RATE, canonical_duration, canonical_samples};
use aria_kit::speaker::{Speaker, TurnFeed};
use aria_kit::voice_activity::SileroVad;
use serde::Serialize;
use serde_json::{Value, json};

use super::{Truth, source_regions};
use crate::audio::{AudioCapture, RecordingMode, ReplayPace, StopSignal, WavReplayCapture};
use crate::config::SpeakerConfig;
use crate::speaker::VoiceprintTracker;

/// The judgment of the latest turn, as the tracker shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Judgment {
    Pending,
    Unverified,
    Owner,
    Other,
}

/// How much of the source audio the tracker let through, summed over turns.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub(super) struct Admitted {
    pub(super) owner_ms: f64,
    pub(super) owner_admitted_ms: f64,
    /// Another voice's audio where the owner is not speaking over it.
    pub(super) other_exclusive_ms: f64,
    pub(super) other_exclusive_admitted_ms: f64,
}

impl Admitted {
    pub(super) fn add(&mut self, other: &Self) {
        self.owner_ms += other.owner_ms;
        self.owner_admitted_ms += other.owner_admitted_ms;
        self.other_exclusive_ms += other.other_exclusive_ms;
        self.other_exclusive_admitted_ms += other.other_exclusive_admitted_ms;
    }

    pub(super) fn owner_kept(&self) -> String {
        percent(self.owner_admitted_ms, self.owner_ms)
    }

    pub(super) fn other_admitted(&self) -> String {
        percent(self.other_exclusive_admitted_ms, self.other_exclusive_ms)
    }
}

fn percent(part: f64, whole: f64) -> String {
    if whole == 0.0 {
        return "-".into();
    }
    format!("{:.1}%", part / whole * 100.0)
}

pub(super) fn evaluate(speaker: &SpeakerConfig, wav: &Path, truth: &Truth) -> (Value, Admitted) {
    let spool = tempfile::tempdir().unwrap();
    let capture = WavReplayCapture::new(
        spool.path(),
        Duration::from_secs(120),
        wav,
        ReplayPace::Immediate,
    )
    .expect("the fixture is canonical PCM16 audio");
    let mut recording = capture
        .start(RecordingMode::UntilStopped, StopSignal::from_fn(|| false))
        .expect("the fixture opens");
    let (mut diarizer, segmenter) = super::tracking_models(speaker);
    let from = recording.held_from();
    let speech = aria_kit::voice_activity::SpeechDetector::new(
        SileroVad::load().expect("silero loads"),
        from,
    );
    let tracker = VoiceprintTracker::owner_only(&mut diarizer, segmenter, from)
        .expect("the corpus enrolls an owner");
    let mut feed = TurnFeed::start(&mut recording, speech, Some(tracker));
    recording.fill().expect("the fixture finishes");
    assert_eq!(recording.held_from(), 0);
    let end = recording.held_to();
    assert!(end > 0, "the fixture contains audio");
    let step = canonical_samples(Duration::from_millis(100));
    let mut at = 0;
    let mut previous = None;
    let mut transitions = vec![(0, previous)];
    loop {
        at = (at + step).min(end);
        feed.hear_through(at);
        if at == end {
            feed.hear_rest();
        }
        let timeline = feed.timeline().expect("the tracker runs");
        assert!(!timeline.is_broken(), "speaker tracking failed");
        let state = timeline.speaking().map(|speaker| match speaker {
            Speaker::Owner(_) => Judgment::Owner,
            Speaker::Other(_) => Judgment::Other,
            Speaker::Unverified => Judgment::Unverified,
            Speaker::Pending(_) => Judgment::Pending,
        });
        if state != previous {
            transitions.push((at, state));
            previous = state;
        }
        if at == end {
            break;
        }
    }
    let timeline = feed.timeline().expect("the tracker runs").clone();
    drop(feed);
    assert_eq!(timeline.reached(), end);
    assert!(!matches!(timeline.speaking(), Some(Speaker::Pending(_))));
    let regions: Vec<_> = source_regions(truth)
        .into_iter()
        .map(|(span, owner)| (source_span(span.start, span.end, end), owner))
        .collect();
    let admitted = |span: &Range<u64>| {
        timeline
            .admitted_audio(
                &recording.samples()[span.start as usize..span.end as usize],
                span.start,
            )
            .len() as u64
    };
    let mut total = Admitted::default();
    let ms = |samples: u64| canonical_duration(samples).as_secs_f64() * 1_000.0;
    let turns: Vec<Value> = truth
        .turns
        .iter()
        .map(|turn| {
            let span = source_span(turn.start, turn.end, end);
            let source_samples = span.end - span.start;
            let admitted_samples = admitted(&span);
            let exclusive = other_exclusive(&span, turn.owner, &regions);
            let exclusive_samples: u64 = exclusive.iter().map(|span| span.end - span.start).sum();
            let exclusive_admitted: u64 = exclusive.iter().map(&admitted).sum();
            if turn.owner {
                total.owner_ms += ms(source_samples);
                total.owner_admitted_ms += ms(admitted_samples);
            } else {
                total.other_exclusive_ms += ms(exclusive_samples);
                total.other_exclusive_admitted_ms += ms(exclusive_admitted);
            }
            json!({
                "owner": turn.owner,
                "source_ms": canonical_duration(source_samples).as_secs_f64() * 1_000.0,
                "admitted_ms": canonical_duration(admitted_samples).as_secs_f64() * 1_000.0,
                "other_exclusive_ms": canonical_duration(exclusive_samples).as_secs_f64() * 1_000.0,
                "other_exclusive_admitted_ms": canonical_duration(exclusive_admitted).as_secs_f64() * 1_000.0,
                "owner_overlap_ms": if turn.owner { 0.0 } else {
                    canonical_duration(source_samples - exclusive_samples).as_secs_f64() * 1_000.0
                },
                "live": live_regions(&span, &regions, &transitions),
            })
        })
        .collect();
    assert!(
        !turns.is_empty(),
        "the fixture has labeled source intervals"
    );
    let transitions: Vec<Value> = transitions
        .into_iter()
        .map(|(at, state)| {
            json!({
                "at_ms": canonical_duration(at).as_secs_f64() * 1_000.0,
                "state": state,
            })
        })
        .collect();
    let tracking = json!({
        "metric": "source_intervals_including_silence",
        "attribution": "owner_precedes_other_during_overlap",
        "turns": turns,
        "transitions": transitions,
    });
    (tracking, total)
}

fn other_exclusive(
    span: &Range<u64>,
    owner: bool,
    regions: &[(Range<u64>, bool)],
) -> Vec<Range<u64>> {
    if owner {
        return Vec::new();
    }
    regions
        .iter()
        .filter_map(|(region, owner)| {
            let from = span.start.max(region.start);
            let to = span.end.min(region.end);
            (!owner && from < to).then_some(from..to)
        })
        .collect()
}

fn live_regions(
    span: &Range<u64>,
    regions: &[(Range<u64>, bool)],
    transitions: &[(u64, Option<Judgment>)],
) -> Value {
    let mut matching = 0;
    let mut first = None;
    let mut changes = 0;
    for (index, &(changed, state)) in transitions.iter().enumerate() {
        if index > 0 && span.contains(&changed) {
            changes += 1;
        }
        let next = transitions.get(index + 1).map_or(span.end, |&(at, _)| at);
        for (region, owner) in regions {
            let expected = Some(if *owner {
                Judgment::Owner
            } else {
                Judgment::Other
            });
            let from = changed.max(span.start).max(region.start);
            let to = next.min(span.end).min(region.end);
            if to > from && state == expected {
                matching += to - from;
                first.get_or_insert(from - span.start);
            }
        }
    }
    json!({
        "matching_role_ms": canonical_duration(matching).as_secs_f64() * 1_000.0,
        "first_matching_role_delay_ms": first.map(|delay| canonical_duration(delay).as_secs_f64() * 1_000.0),
        "state_changes": changes,
    })
}

fn source_span(start: f64, end: f64, samples: u64) -> Range<u64> {
    let sample = |seconds: f64| (seconds * f64::from(CANONICAL_SAMPLE_RATE)).round() as u64;
    let span = sample(start)..sample(end);
    assert!(
        span.start < span.end && span.end <= samples,
        "truth fits the fixture"
    );
    span
}

#[test]
fn live_scores_distinguish_a_brief_match_from_never_matching() {
    let sample = |ms| canonical_samples(Duration::from_millis(ms));
    let transitions = [
        (0, None),
        (sample(100), Some(Judgment::Other)),
        (sample(200), Some(Judgment::Owner)),
        (sample(250), Some(Judgment::Other)),
    ];
    assert_eq!(
        live_regions(
            &(sample(100)..sample(400)),
            &[(sample(100)..sample(400), true)],
            &transitions,
        ),
        json!({
            "matching_role_ms": 50.0,
            "first_matching_role_delay_ms": 100.0,
            "state_changes": 3,
        })
    );
    assert_eq!(
        live_regions(
            &(sample(400)..sample(600)),
            &[(sample(400)..sample(600), true)],
            &transitions,
        ),
        json!({
            "matching_role_ms": 0.0,
            "first_matching_role_delay_ms": null,
            "state_changes": 0,
        })
    );
    assert_eq!(
        live_regions(
            &(sample(225)..sample(250)),
            &[(sample(225)..sample(250), true)],
            &transitions,
        ),
        json!({
            "matching_role_ms": 25.0,
            "first_matching_role_delay_ms": 0.0,
            "state_changes": 0,
        })
    );
}

#[test]
fn overlap_is_owner_present_and_only_other_exclusive_time_can_be_typed() {
    let regions = [(0..10, false), (10..20, true), (20..30, false)];
    assert_eq!(
        other_exclusive(&(0..30), false, &regions),
        vec![0..10, 20..30]
    );
    assert_eq!(
        other_exclusive(&(5..25), false, &regions),
        vec![5..10, 20..25]
    );
    assert!(other_exclusive(&(10..20), true, &regions).is_empty());
    let transitions = [
        (0, Some(Judgment::Other)),
        (10, Some(Judgment::Owner)),
        (20, Some(Judgment::Other)),
    ];
    assert_eq!(
        live_regions(&(0..30), &regions, &transitions),
        json!({
            "matching_role_ms": canonical_duration(30).as_secs_f64() * 1_000.0,
            "first_matching_role_delay_ms": 0.0,
            "state_changes": 2,
        })
    );
}
