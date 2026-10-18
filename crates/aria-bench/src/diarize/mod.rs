//! Diarization accuracy over synthesized ground-truth conversations.
//!
//! A TTS engine renders scripted multi-speaker conversations, the real
//! listen stack (streaming STT, speech gate, speaker labeler) hears them
//! replayed in realtime, and the labels are scored against the script.
//! One approximation: the final pass reclusters per-segment embeddings,
//! where production reclusters paragraph means.
//!
//! The speaker engine is what this measures, so this crate owns it. TTS and
//! STT are the harness that produces and reads the audio; the caller loads
//! those and hands them in. The scenario scripts speak in kokoro voice
//! names, so another TTS must know them.

mod cache;
mod cluster;
mod journal;
mod pyannote;
mod replay;
mod score;
mod shipped;
mod sibling;
mod synth;

pub use aria::speaker::Grouping;
pub use pyannote::{Clustering, Tuning, label_by_segmentation};
pub use shipped::{Run, label_as_attributed, label_as_shipped};
pub use sibling::{Windowing, label_by_voice};

use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use aria::speaker::Diarizer;
use aria::transcription::{StreamingEngine, Transcription, TranscriptionError};
use aria_kit::audio::{CANONICAL_SAMPLE_RATE, CaptureError, ReplayPace, ResampleError, WavError};
use aria_kit::speaker::{Engine as _, SpeakerError};
use aria_kit::synthesis::{SpeechTextError, Synthesizer, VoiceError};
use aria_kit::voice_activity::VoiceActivityError;
use aria_speaker::wespeaker::{Config as SpeakerConfig, WeSpeaker};

/// Longer than any scenario's realtime replay.
const SCENARIO_LIMIT: Duration = Duration::from_secs(600);
/// What `WavReplayCapture` will accept; a longer recording must be cut first.
const RECORDING_LIMIT: Duration = Duration::from_secs(900);
pub(crate) const SAMPLE_RATE: f64 = CANONICAL_SAMPLE_RATE as f64;

/// Where a recording's speech boundaries come from. Word spans stop at the
/// last word, so the pauses inside a turn read as silence; the voice detector
/// answers over the audio itself, and scores nine points better on AMI
/// ES2004a for it. `Transcriber` stays available to measure the difference.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Timeline {
    Transcriber,
    VoiceActivity,
}

/// One labeled stretch of a recording: when it starts, how long it runs, and
/// whose voice it is. The shape an RTTM row wants.
pub struct Turn {
    pub start: f64,
    pub duration: f64,
    pub speaker: String,
}

/// What one recording came to: the turns worth writing, and the speech the
/// gate refused. An RTTM shows only the first, so the second has to be
/// reported separately or a coverage problem stays invisible.
pub struct Labeled {
    pub turns: Vec<Turn>,
    pub dropped_segments: usize,
    pub dropped_seconds: f64,
    /// One entry per segment the pipeline heard, in order. Clustering is what
    /// turns these into names, and tuning it against a 15-minute replay is
    /// hopeless — dumped, they can be re-clustered offline in seconds.
    pub embeddings: Vec<Embedded>,
}

/// A segment's voiceprint and where its audio sits, in seconds.
pub struct Embedded {
    pub start: f64,
    pub end: f64,
    pub embedding: Option<Vec<f32>>,
}

/// Labels one real recording end to end, for scoring against a corpus's own
/// ground truth. No script to synthesize, so no TTS — the audio already
/// exists. It plays at its own speed because streaming transcription decodes
/// a window at a time; handed a whole recording at once, the encoder outruns
/// its positional limit.
pub fn label_recording<Stt: StreamingEngine>(
    stt: Stt,
    speaker: SpeakerConfig,
    wav: &Path,
    timeline: Timeline,
) -> Result<Labeled, BenchError> {
    let transcription = Transcription::streaming(stt);
    let workspace = tempfile::tempdir().map_err(BenchError::Workspace)?;
    let speakers_dir = workspace.path().join("speakers");
    std::fs::create_dir_all(&speakers_dir).map_err(BenchError::Workspace)?;

    let mut labeler = replay::labeler(&speaker, &speakers_dir)?;
    let (segments, dropped) = replay::hear(
        &transcription,
        wav,
        workspace.path(),
        &mut labeler,
        "recording",
        ReplayPace::Realtime,
        RECORDING_LIMIT,
    )?;

    let embeddings: Vec<Option<Vec<f32>>> = segments.iter().map(|s| s.embedding.clone()).collect();
    let mut settled = labeler.labeler().relabel(&embeddings);
    aria::speaker::settle(&mut settled, |index| segments[index].label.is_none());
    let mut turns: Vec<Turn> = segments
        .iter()
        .zip(settled)
        .filter_map(|(segment, speaker)| {
            let start = segment.span.start as f64 / SAMPLE_RATE;
            let end = segment.span.end as f64 / SAMPLE_RATE;
            speaker.map(|speaker| Turn {
                start,
                duration: (end - start).max(0.0),
                speaker,
            })
        })
        .collect();
    if timeline == Timeline::VoiceActivity {
        turns = attribute(speech_regions(wav)?, &turns);
    }
    sequence(&mut turns);
    Ok(Labeled {
        turns,
        dropped_segments: dropped.segments,
        dropped_seconds: dropped.samples as f64 / SAMPLE_RATE,
        embeddings: segments
            .iter()
            .map(|segment| Embedded {
                start: segment.span.start as f64 / SAMPLE_RATE,
                end: segment.span.end as f64 / SAMPLE_RATE,
                embedding: segment.embedding.clone(),
            })
            .collect(),
    })
}

/// The speaker model every labeling strategy embeds with, enrolled with
/// nobody.
fn diarizer(speaker: SpeakerConfig) -> Result<Diarizer<WeSpeaker>, BenchError> {
    let embedder = WeSpeaker::load_from(speaker).map_err(BenchError::SpeakerModel)?;
    Ok(Diarizer::new(embedder, Vec::new()))
}

/// The recording's speech, as the voice detector hears it rather than as the
/// transcriber chopped it.
fn speech_regions(wav: &Path) -> Result<Vec<Range<u64>>, BenchError> {
    let samples = sibling::read(wav)?;
    Ok(sibling::speech(&samples)?
        .into_iter()
        .map(|region| region.start as u64..region.end as u64)
        .collect())
}

/// How far a stretch of speech looks for a name when no words land on it.
/// Past this the nearest turn is too far away to be the same voice, and the
/// stretch is better left unnamed than named wrong.
const NAMING_REACH: f64 = 1.0;

/// Names each stretch of speech after the words spoken over it, or failing
/// that after the closest words within `NAMING_REACH`. Speech the transcriber
/// never put words to is otherwise dropped, and a dropped stretch costs the
/// same as a misnamed one — so a nearby name beats no name.
fn attribute(regions: Vec<Range<u64>>, turns: &[Turn]) -> Vec<Turn> {
    regions
        .into_iter()
        .filter_map(|region| {
            let start = region.start as f64 / SAMPLE_RATE;
            let end = region.end as f64 / SAMPLE_RATE;
            let overlapping = turns
                .iter()
                .filter_map(|turn| {
                    let overlap = (turn.start + turn.duration).min(end) - turn.start.max(start);
                    (overlap > 0.0).then_some((overlap, &turn.speaker))
                })
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, speaker)| speaker);
            let speaker = overlapping.or_else(|| {
                turns
                    .iter()
                    .filter_map(|turn| {
                        let gap = (turn.start - end).max(start - (turn.start + turn.duration));
                        (gap <= NAMING_REACH).then_some((gap, &turn.speaker))
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .map(|(_, speaker)| speaker)
            })?;
            Some(Turn {
                start,
                duration: end - start,
                speaker: speaker.clone(),
            })
        })
        .collect()
}

/// Lays the turns end to end in time. A decode's span can cover audio an
/// earlier one already claimed, and this pipeline hears one voice at a time,
/// so a turn starts where the last one stopped. Turns left with no time of
/// their own are dropped: their words belong to a stretch already spoken for.
pub(crate) fn sequence(turns: &mut Vec<Turn>) {
    turns.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut spoken_through = 0.0_f64;
    turns.retain_mut(|turn| {
        let end = turn.start + turn.duration;
        turn.start = turn.start.max(spoken_through);
        turn.duration = end - turn.start;
        if turn.duration <= 0.0 {
            return false;
        }
        spoken_through = end;
        true
    });
}

#[cfg(test)]
mod attribute_tests {
    use super::*;

    fn seconds(at: f64) -> u64 {
        (at * SAMPLE_RATE) as u64
    }

    fn spoken(start: f64, duration: f64, speaker: &str) -> Turn {
        Turn {
            start,
            duration,
            speaker: speaker.to_owned(),
        }
    }

    #[test]
    fn a_region_takes_the_voice_it_shares_the_most_time_with() {
        let turns = vec![spoken(0.0, 3.0, "a"), spoken(3.0, 1.0, "b")];
        let region = seconds(0.0)..seconds(4.0);
        let named = attribute(Vec::from([region]), &turns);
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].speaker, "a");
    }

    #[test]
    fn a_region_with_no_words_on_it_takes_the_nearest_voice() {
        let turns = vec![spoken(0.0, 1.0, "a")];
        let region = seconds(1.5)..seconds(2.0);
        let named = attribute(Vec::from([region]), &turns);
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].speaker, "a");
    }

    #[test]
    fn a_region_too_far_from_any_words_stays_unnamed() {
        let turns = vec![spoken(0.0, 1.0, "a")];
        let region = seconds(9.0)..seconds(10.0);
        assert!(attribute(Vec::from([region]), &turns).is_empty());
    }
}

#[cfg(test)]
mod sequence_tests {
    use super::*;

    fn turn(start: f64, duration: f64, speaker: &str) -> Turn {
        Turn {
            start,
            duration,
            speaker: speaker.to_owned(),
        }
    }

    #[test]
    fn a_turn_starts_where_the_one_before_it_stopped() {
        let mut turns = vec![turn(0.0, 10.0, "a"), turn(4.0, 8.0, "b")];
        sequence(&mut turns);
        assert_eq!(turns.len(), 2);
        assert_eq!((turns[1].start, turns[1].duration), (10.0, 2.0));
    }

    #[test]
    fn a_turn_an_earlier_one_already_covers_is_dropped() {
        let mut turns = vec![turn(0.0, 10.0, "a"), turn(2.0, 3.0, "a")];
        sequence(&mut turns);
        assert_eq!(turns.len(), 1);
        assert_eq!((turns[0].start, turns[0].duration), (0.0, 10.0));
    }
}

pub struct Request<Tts, Stt> {
    pub tts: Tts,
    pub stt: Stt,
    pub speaker: SpeakerConfig,
}

pub struct Report {
    pub scenarios: Vec<ScenarioReport>,
}

pub struct ScenarioReport {
    pub name: &'static str,
    pub segments: usize,
    pub dropped: usize,
    /// Seconds of audio the speech gate threw away — the budget any coverage
    /// fix could win back.
    pub dropped_seconds: f64,
    /// Word-weighted label accuracy of the live, tentative labels.
    pub live_accuracy: f64,
    /// The same after the final re-cluster — what the transcript ends with.
    pub final_accuracy: f64,
    /// The share of the script's evidence words that reached the transcript.
    /// Accuracy alone cannot fall when speech goes missing; this can.
    pub word_recall: f64,
}

impl<Tts: Synthesizer, Stt: StreamingEngine> Request<Tts, Stt> {
    pub fn run(self) -> Result<Report, BenchError> {
        let Self { tts, stt, speaker } = self;
        // The same composition the listen daemon runs.
        let transcription = Transcription::streaming(stt);
        let workspace = tempfile::tempdir().map_err(BenchError::Workspace)?;
        // An empty speakers directory keeps enrolled profiles out of the score.
        let speakers_dir = workspace.path().join("speakers");
        std::fs::create_dir_all(&speakers_dir).map_err(BenchError::Workspace)?;

        let mut scenarios = Vec::new();
        for scenario in synth::Scenario::all() {
            let wav = scenario.synthesize(&tts, workspace.path())?;
            let mut labeler = replay::labeler(&speaker, &speakers_dir)?;
            let (segments, dropped) = replay::hear(
                &transcription,
                &wav,
                workspace.path(),
                &mut labeler,
                scenario.name,
                ReplayPace::Realtime,
                SCENARIO_LIMIT,
            )?;
            scenarios.push(scenario.scored(&labeler, &segments, &dropped));
        }
        Ok(Report { scenarios })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("failed to prepare the bench workspace")]
    Workspace(#[source] std::io::Error),

    #[error("failed to keep the chunks the nets read")]
    Cache(#[source] std::io::Error),

    #[error("the {scenario} scenario holds a turn no engine will speak")]
    Script {
        scenario: &'static str,
        #[source]
        source: ScriptError,
    },

    #[error("failed to synthesize the {scenario} scenario")]
    Synthesis {
        scenario: &'static str,
        #[source]
        source: aria_kit::synthesis::SynthesisError,
    },

    #[error("failed to resample synthesized speech to 16 kHz")]
    Resample(#[from] ResampleError),

    #[error("failed to read or write the {scenario} scenario's audio")]
    Wav {
        scenario: &'static str,
        #[source]
        source: WavError,
    },

    // The bench exists to score labels, so an unlabeled run is a failure
    // rather than the graceful degradation a listen session would take.
    #[error("the speaker model did not load, so the run would measure nothing")]
    SpeakerModel(#[source] SpeakerError),

    #[error("the speech gate failed, so the run would measure nothing")]
    SpeechGate(#[source] VoiceActivityError),

    // Same failure, reached where no engine error was produced to carry.
    #[error("no speaker model is installed, so the run would measure nothing")]
    NoSpeakerModel,

    #[error("failed to replay the {scenario} scenario's audio")]
    Replay {
        scenario: &'static str,
        #[source]
        source: CaptureError,
    },

    #[error("failed to transcribe the {scenario} scenario")]
    Transcription {
        scenario: &'static str,
        #[source]
        source: TranscriptionError,
    },
}

/// A scripted turn the synthesis contract refuses before any engine sees it.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error(transparent)]
    Text(#[from] SpeechTextError),

    #[error(transparent)]
    Voice(#[from] VoiceError),
}
