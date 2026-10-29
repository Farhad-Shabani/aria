//! Dictation-shaped conversations for the own-voice filter, read by
//! LibriSpeech test-clean speakers: one enrolled owner per directory, the
//! other voices that interrupt them, and where each turn's audio sits.
//! Written once, so a dictation run can replay the same audio before and
//! after a change.
//!
//! The recipe is Personal VAD's (arXiv 1908.04284): single-speaker
//! utterances joined into turns, with the owner enrolled from a chapter none
//! of their turns come from.

pub mod scores;

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aria_bench_speaker::similarity;
use aria_harness::artifact::{ArtifactError, Provenance, fingerprint, write_json};
use aria_harness::own_voice::{
    ArtifactKind, OverlapControls, OverlapProbe, Placed, SPEAKERS_DIR, Truth,
};
use aria_kit::audio::{
    CANONICAL_SAMPLE_RATE, Clip, WavError, canonical_samples, read_wav, write_wav,
};
use aria_kit::speaker::{
    Embedder, Enrolled, Enrollment, EnrollmentError, ProfileError, ProfileStore, Replace, Role,
    SpeakerError, SpeakerName,
};
use aria_speaker::wespeaker::{Config as SpeakerConfig, WeSpeaker};
use serde::Serialize;

use crate::diarize::{BenchError, embedder};
use crate::stt::librispeech::{self, Chapter, Gender, LibrispeechError, Speaker, Subset};

const OWNER: &str = "owner";
const LEAD_IN: Duration = Duration::from_millis(500);
/// Long enough after the last word for a manual stop at the end of the file
/// to cut nothing off.
const TAIL: Duration = Duration::from_millis(1_500);
const ENROLLMENT_SAMPLES: usize = 5;
/// The most audio the tracker judges a turn's voice on.
const JUDGED: Duration = Duration::from_secs(3);
/// A dictated sentence, long enough to be judged whole.
const LONG: Range<Duration> = JUDGED..Duration::from_secs(5);
/// Too short to be judged whole.
const SHORT: Range<Duration> = Duration::from_secs(1)..JUDGED;
const LONG_TURNS: usize = 2;
/// How far another voice reaches back over the end of the owner's turn.
const OVERLAP: Duration = Duration::from_millis(1_500);

/// Who speaks a turn of the script.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Actor {
    /// The enrolled voice. Its turns are the ones a dictation should type.
    Owner,
    /// A voice nobody enrolled, of the other gender.
    Other,
    /// The voice nobody enrolled that scores closest to the owner's.
    Similar,
}

/// What a turn says: one of the speaker's long utterances, or their short one.
#[derive(Clone, Copy)]
enum Said {
    Long(usize),
    Short,
}

#[derive(Clone, Copy)]
enum Start {
    /// This long after every earlier turn ends.
    After(Duration),
    /// This long before the latest earlier turn ends.
    Over(Duration),
}

struct Scenario {
    name: &'static str,
    turns: Vec<(Start, Actor, Said)>,
}

pub struct Request {
    /// Where the test-clean archive is, or is downloaded to.
    pub librispeech: PathBuf,
    pub speaker_model: PathBuf,
    pub owners: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum OwnVoiceError {
    #[error(transparent)]
    Bench(#[from] BenchError),

    #[error(transparent)]
    Librispeech(#[from] LibrispeechError),

    #[error("the owner's enrollment did not take")]
    Enrollment(#[from] EnrollmentError),

    #[error("failed to store the owner's profile")]
    Profile(#[from] ProfileError),

    #[error("the speaker model failed")]
    Speaker(#[from] SpeakerError),

    #[error("failed to read the decoded utterance {id}")]
    Utterance {
        id: String,
        #[source]
        source: WavError,
    },

    #[error("failed to write {}", .path.display())]
    Wav {
        path: PathBuf,
        #[source]
        source: WavError,
    },

    #[error("test-clean holds too few speakers with two chapters for {0} owners")]
    TooFewOwners(usize),

    #[error("no test-clean speaker reads every line the {actor} needs for owner {owner}")]
    NoReader { owner: u32, actor: &'static str },

    #[error("{0} is not empty")]
    NotEmpty(PathBuf),

    #[error(transparent)]
    Artifact(#[from] ArtifactError),
}

/// The voices of one owner's corpus, and how close each sits to the owner.
#[derive(Serialize)]
struct Cast {
    owner: Reader,
    other: Reader,
    similar: Reader,
    enrollment_consistency: f32,
}

#[derive(Serialize)]
struct Reader {
    speaker: u32,
    gender: Gender,
    /// Each turn utterance's likeness to the owner, long ones first.
    likeness: Vec<Option<f32>>,
}

/// One speaker's turn utterances, decoded.
struct Lines {
    long: Vec<Utterance>,
    short: Utterance,
}

struct Utterance {
    text: String,
    samples: Vec<f32>,
}

/// Writes one directory per owner into `directory`, each with its scenarios,
/// their ground truth, and the owner's profile in `speakers/`, then
/// `voices.json` beside them. Returns the owners' speaker ids.
///
/// `directory` must be empty: the eval reads every directory in it as an
/// owner, so leftovers from another run would be scored as this one.
pub fn write_corpus(request: &Request, directory: &Path) -> Result<Vec<u32>, OwnVoiceError> {
    let mut entries = std::fs::read_dir(directory).map_err(BenchError::Workspace)?;
    if entries.next().is_some() {
        return Err(OwnVoiceError::NotEmpty(directory.to_path_buf()));
    }
    librispeech::download(&request.librispeech, Subset::TestClean)
        .map_err(LibrispeechError::from)?;
    let speakers = librispeech::speakers(&request.librispeech, Subset::TestClean)?;
    let mut readers = Readers {
        archive: Archive {
            speech: request.librispeech.clone(),
            subset: Subset::TestClean,
            decoded: tempfile::tempdir().map_err(BenchError::Workspace)?,
        },
        lines: HashMap::new(),
    };
    // An owner reads every line and enrolls from their first chapter.
    let mut enrolled = HashMap::new();
    let chosen = owners(&speakers, request.owners, &mut |speaker| {
        if !readers.readable(speaker)? {
            return Ok(false);
        }
        match enroll(
            &request.speaker_model,
            &readers.archive,
            &speaker.chapters[0],
        ) {
            Ok(owner) => {
                enrolled.insert(speaker.id, owner);
                Ok(true)
            }
            Err(OwnVoiceError::Enrollment(EnrollmentError::Engine(error))) => {
                Err(EnrollmentError::Engine(error).into())
            }
            Err(OwnVoiceError::Enrollment(_)) => Ok(false),
            Err(error) => Err(error),
        }
    })?;
    let mut scorer = embedder(speaker_config(&request.speaker_model))?;
    let mut casts = Vec::new();
    for owner in chosen {
        let folder = directory.join(owner.id.to_string());
        let speakers_dir = folder.join(SPEAKERS_DIR);
        std::fs::create_dir_all(&speakers_dir).map_err(BenchError::Workspace)?;
        let owner_enrolled = enrolled
            .remove(&owner.id)
            .expect("every chosen owner enrolled");
        ProfileStore::open(&speakers_dir).save(&owner_enrolled.profile, Replace::Allowed)?;
        let (profile, consistency) = (owner_enrolled.profile.vector, owner_enrolled.consistency);
        let other = first_readable(others(&speakers, owner), &mut |speaker| {
            readers.readable(speaker)
        })?
        .ok_or(OwnVoiceError::NoReader {
            owner: owner.id,
            actor: "other",
        })?;
        // Ranked on the utterance `alike_cut_in` gives them, as the tracker hears it.
        let mut scored = Vec::new();
        for candidate in speakers
            .iter()
            .filter(|candidate| candidate.gender == owner.gender && candidate.id != owner.id)
        {
            if !readers.readable(candidate)? {
                continue;
            }
            let samples = &readers.lines(candidate).long[0].samples;
            if let Some(likeness) = likeness(&mut scorer, samples, &profile)? {
                scored.push((candidate, likeness));
            }
        }
        let (similar, _) = *by_likeness(owner, &scored)
            .first()
            .ok_or(OwnVoiceError::NoReader {
                owner: owner.id,
                actor: "similar",
            })?;
        let voices = [
            (Actor::Owner, owner),
            (Actor::Other, other),
            (Actor::Similar, similar),
        ]
        .map(|(actor, speaker)| (actor, readers.lines(speaker)));
        let mut reader = |speaker: &Speaker, lines: &Lines| -> Result<Reader, OwnVoiceError> {
            let mut likeness = Vec::new();
            for utterance in lines.long.iter().chain([&lines.short]) {
                likeness.push(self::likeness(&mut scorer, &utterance.samples, &profile)?);
            }
            Ok(Reader {
                speaker: speaker.id,
                gender: speaker.gender,
                likeness,
            })
        };
        casts.push(Cast {
            owner: reader(owner, voices[0].1)?,
            other: reader(other, voices[1].1)?,
            similar: reader(similar, voices[2].1)?,
            enrollment_consistency: consistency,
        });
        for scenario in scenarios() {
            write_scenario(&scenario, &voices, &folder)?;
        }
    }
    write_json(
        &directory.join("voices.json"),
        &serde_json::json!({
            "speaker_model_sha256": fingerprint(&request.speaker_model)?,
            "provenance": Provenance::capture()?,
            "owners": casts,
        }),
    )?;
    Ok(casts.iter().map(|cast| cast.owner.speaker).collect())
}

fn speaker_config(model: &Path) -> SpeakerConfig {
    SpeakerConfig {
        model: model.to_path_buf(),
    }
}

/// Each speaker's turn utterances, read once.
struct Readers {
    archive: Archive,
    lines: HashMap<u32, Option<Lines>>,
}

impl Readers {
    /// Whether the speaker reads every line a scenario may give them.
    fn readable(&mut self, speaker: &Speaker) -> Result<bool, OwnVoiceError> {
        if !self.lines.contains_key(&speaker.id) {
            let lines = read_lines(&self.archive, speaker)?;
            self.lines.insert(speaker.id, lines);
        }
        Ok(self.lines[&speaker.id].is_some())
    }

    fn lines(&self, speaker: &Speaker) -> &Lines {
        self.lines[&speaker.id]
            .as_ref()
            .expect("a chosen speaker is readable")
    }
}

/// Decodes a subset's utterances on demand into a scratch directory.
struct Archive {
    speech: PathBuf,
    subset: Subset,
    decoded: tempfile::TempDir,
}

impl Archive {
    fn read(&self, id: &str) -> Result<Vec<f32>, OwnVoiceError> {
        let wav = self.decoded.path().join(format!("{id}.wav"));
        if !wav.exists() {
            let recording = librispeech::recording_for(&self.speech, self.subset, id);
            librispeech::decode(&recording, &wav).map_err(LibrispeechError::from)?;
        }
        let clip = read_wav(&wav).map_err(|source| OwnVoiceError::Utterance {
            id: id.to_owned(),
            source,
        })?;
        Ok(clip
            .into_canonical()
            .map_err(BenchError::from)?
            .into_samples())
    }
}

/// Enrolls an owner from the first long-enough utterances of `chapter`.
fn enroll(
    speaker_model: &Path,
    archive: &Archive,
    chapter: &Chapter,
) -> Result<Enrolled, OwnVoiceError> {
    let mut enrollment = Enrollment::new(embedder(speaker_config(speaker_model))?);
    let mut samples = 0;
    for (id, _) in &chapter.utterances {
        if samples == ENROLLMENT_SAMPLES {
            break;
        }
        let audio = archive.read(id)?;
        if length(audio.len()) >= LONG.start {
            enrollment.add(&audio)?;
            samples += 1;
        }
    }
    let name = SpeakerName::new(OWNER).expect("the owner's name is valid");
    Ok(enrollment.finish(name, Role::Owner)?)
}

/// The mean score against the owner's profile over the windows the tracker
/// judges a turn's voice on. `None` when the model found too little voice.
fn likeness(
    scorer: &mut WeSpeaker,
    samples: &[f32],
    profile: &[f32],
) -> Result<Option<f32>, OwnVoiceError> {
    let window = canonical_samples(JUDGED) as usize;
    let mut scores = Vec::new();
    let mut start = 0;
    loop {
        let end = (start + window).min(samples.len());
        if let Some(embedding) = scorer.embed(&samples[start..end])? {
            scores.push(similarity(&embedding, profile));
        }
        if end == samples.len() {
            break;
        }
        start += window / 2;
    }
    Ok((!scores.is_empty()).then(|| scores.iter().sum::<f32>() / scores.len() as f32))
}

/// The owners, alternating female and male in id order. Each reads a second
/// chapter, so enrollment and turns come from different sessions.
fn owners<'a>(
    speakers: &'a [Speaker],
    count: usize,
    readable: &mut impl FnMut(&Speaker) -> Result<bool, OwnVoiceError>,
) -> Result<Vec<&'a Speaker>, OwnVoiceError> {
    let mut chosen: Vec<&Speaker> = Vec::new();
    for gender in [Gender::Female, Gender::Male]
        .into_iter()
        .cycle()
        .take(count)
    {
        let candidates = speakers.iter().filter(|speaker| {
            speaker.gender == gender
                && speaker.chapters.len() >= 2
                && chosen.iter().all(|owner| owner.id != speaker.id)
        });
        let next =
            first_readable(candidates, readable)?.ok_or(OwnVoiceError::TooFewOwners(count))?;
        chosen.push(next);
    }
    Ok(chosen)
}

fn first_readable<'a>(
    candidates: impl IntoIterator<Item = &'a Speaker>,
    readable: &mut impl FnMut(&Speaker) -> Result<bool, OwnVoiceError>,
) -> Result<Option<&'a Speaker>, OwnVoiceError> {
    for candidate in candidates {
        if readable(candidate)? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Speakers of the other gender, starting after the owner and wrapping around.
fn others<'a>(speakers: &'a [Speaker], owner: &Speaker) -> impl Iterator<Item = &'a Speaker> {
    let gender = owner.gender;
    let (before, after): (Vec<&Speaker>, Vec<&Speaker>) = speakers
        .iter()
        .filter(|speaker| speaker.gender != gender)
        .partition(|speaker| speaker.id < owner.id);
    after.into_iter().chain(before)
}

/// The owner's gender, closest voice first, without the owner.
fn by_likeness<'a>(owner: &Speaker, scored: &[(&'a Speaker, f32)]) -> Vec<(&'a Speaker, f32)> {
    let mut ranked: Vec<_> = scored
        .iter()
        .filter(|(speaker, _)| speaker.gender == owner.gender && speaker.id != owner.id)
        .copied()
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked
}

/// The first long utterances and the first short one the speaker reads
/// outside their first chapter, or anywhere when they read only one. `None`
/// when they read too few of either.
fn read_lines(archive: &Archive, speaker: &Speaker) -> Result<Option<Lines>, OwnVoiceError> {
    let chapters = match speaker.chapters.as_slice() {
        [only] => std::slice::from_ref(only),
        [_, rest @ ..] => rest,
        [] => return Ok(None),
    };
    let mut long = Vec::new();
    let mut short = None;
    for (id, text) in chapters.iter().flat_map(|chapter| &chapter.utterances) {
        if long.len() == LONG_TURNS && short.is_some() {
            break;
        }
        let samples = archive.read(id)?;
        let utterance = Utterance {
            text: text.clone(),
            samples,
        };
        let length = length(utterance.samples.len());
        if LONG.contains(&length) && long.len() < LONG_TURNS {
            long.push(utterance);
        } else if SHORT.contains(&length) && short.is_none() {
            short = Some(utterance);
        }
    }
    Ok(match short {
        Some(short) if long.len() == LONG_TURNS => Some(Lines { long, short }),
        _ => None,
    })
}

fn write_scenario(
    scenario: &Scenario,
    voices: &[(Actor, &Lines)],
    folder: &Path,
) -> Result<(), OwnVoiceError> {
    let name = scenario.name;
    let (mixed, placed) = lay_out(&scenario.turns, voices, |_| true);
    let overlapping = scenario
        .turns
        .iter()
        .any(|(start, _, _)| matches!(start, Start::Over(_)));
    if !overlapping {
        return write(folder, name, &mixed, placed, None);
    }
    let control = format!("{name}_owner");
    let (owner_only, owner_placed) =
        lay_out(&scenario.turns, voices, |actor| actor == Actor::Owner);
    let probe = |kind| OverlapProbe {
        artifact_kind: kind,
        controls: OverlapControls {
            owner_control: format!("{control}.wav"),
        },
    };
    write(folder, name, &mixed, placed, Some(probe(ArtifactKind::Mix)))?;
    write(
        folder,
        &control,
        &owner_only,
        owner_placed,
        Some(probe(ArtifactKind::OwnerControl)),
    )
}

/// The scenario's audio and where each kept turn sits in it. A dropped
/// turn's audio is left silent, so every layout of one script shares a clock.
fn lay_out(
    turns: &[(Start, Actor, Said)],
    voices: &[(Actor, &Lines)],
    kept: impl Fn(Actor) -> bool,
) -> (Vec<f32>, Vec<Placed>) {
    let mut audio: Vec<f32> = Vec::new();
    let mut placed = Vec::new();
    let mut latest_end = 0;
    for &(start, actor, said) in turns {
        let lines = &voices
            .iter()
            .find(|(voice, _)| *voice == actor)
            .expect("every actor has a voice")
            .1;
        let utterance = match said {
            Said::Long(index) => &lines.long[index],
            Said::Short => &lines.short,
        };
        let begin = match start {
            Start::After(pause) => latest_end + canonical_samples(pause) as usize,
            Start::Over(overlap) => latest_end.saturating_sub(canonical_samples(overlap) as usize),
        };
        let end = begin + utterance.samples.len();
        audio.resize(audio.len().max(end), 0.0);
        latest_end = latest_end.max(end);
        if !kept(actor) {
            continue;
        }
        for (sample, spoken) in audio[begin..end].iter_mut().zip(&utterance.samples) {
            *sample += spoken;
        }
        placed.push(Placed {
            owner: actor == Actor::Owner,
            text: utterance.text.clone(),
            start: seconds(begin),
            end: seconds(end),
        });
    }
    audio.resize(latest_end + canonical_samples(TAIL) as usize, 0.0);
    (audio, placed)
}

fn write(
    folder: &Path,
    name: &str,
    audio: &[f32],
    turns: Vec<Placed>,
    overlap_probe: Option<OverlapProbe>,
) -> Result<(), OwnVoiceError> {
    let wav = folder.join(format!("{name}.wav"));
    write_wav(&wav, &Clip::canonical(audio.to_vec()))
        .map_err(|source| OwnVoiceError::Wav { path: wav, source })?;
    let truth = Truth {
        turns,
        overlap_probe,
    };
    Ok(write_json(&folder.join(format!("{name}.json")), &truth)?)
}

fn length(samples: usize) -> Duration {
    Duration::from_secs_f64(seconds(samples))
}

fn seconds(samples: usize) -> f64 {
    samples as f64 / f64::from(CANONICAL_SAMPLE_RATE)
}

/// Each speaker reads a different book, so the scorer has words only one
/// side says.
fn scenarios() -> Vec<Scenario> {
    use Actor::{Other, Owner, Similar};
    use Said::{Long, Short};
    use Start::{After, Over};
    let ms = Duration::from_millis;
    vec![
        Scenario {
            name: "owner_alone",
            turns: vec![
                (After(LEAD_IN), Owner, Long(0)),
                (After(ms(400)), Owner, Long(1)),
            ],
        },
        Scenario {
            name: "owner_short",
            turns: vec![(After(LEAD_IN), Owner, Short)],
        },
        Scenario {
            name: "other_alone",
            turns: vec![(After(LEAD_IN), Other, Long(0))],
        },
        Scenario {
            name: "cut_in_paused",
            turns: vec![
                (After(LEAD_IN), Owner, Long(0)),
                (After(ms(1_000)), Other, Long(0)),
                (After(ms(1_000)), Owner, Long(1)),
            ],
        },
        Scenario {
            name: "cut_in_tight",
            turns: vec![
                (After(LEAD_IN), Owner, Long(0)),
                (After(ms(200)), Other, Long(0)),
                (After(ms(200)), Owner, Long(1)),
            ],
        },
        Scenario {
            name: "quick_reply",
            turns: vec![
                (After(LEAD_IN), Owner, Long(0)),
                (After(ms(300)), Other, Short),
                (After(ms(300)), Owner, Long(1)),
            ],
        },
        Scenario {
            name: "other_first",
            turns: vec![
                (After(LEAD_IN), Other, Long(0)),
                (After(ms(300)), Owner, Long(0)),
            ],
        },
        Scenario {
            name: "alike_cut_in",
            turns: vec![
                (After(LEAD_IN), Owner, Long(0)),
                (After(ms(300)), Similar, Long(0)),
                (After(ms(300)), Owner, Long(1)),
            ],
        },
        Scenario {
            name: "overlap",
            turns: vec![
                (After(LEAD_IN), Owner, Long(0)),
                (Over(OVERLAP), Other, Long(0)),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speaker(id: u32, gender: Gender, chapters: u32) -> Speaker {
        Speaker {
            id,
            gender,
            chapters: (0..chapters)
                .map(|_| Chapter {
                    utterances: Vec::new(),
                })
                .collect(),
        }
    }

    fn ids(speakers: &[&Speaker]) -> Vec<u32> {
        speakers.iter().map(|speaker| speaker.id).collect()
    }

    #[test]
    fn owners_alternate_genders_and_skip_unusable_speakers() {
        let speakers = [
            speaker(1, Gender::Male, 2),
            speaker(2, Gender::Female, 1),
            speaker(3, Gender::Female, 2),
            speaker(4, Gender::Male, 3),
            speaker(5, Gender::Female, 2),
        ];

        let mut every = |_: &Speaker| Ok(true);
        assert_eq!(ids(&owners(&speakers, 3, &mut every).unwrap()), [3, 1, 5]);
        let mut not_3 = |speaker: &Speaker| Ok(speaker.id != 3);
        assert_eq!(ids(&owners(&speakers, 2, &mut not_3).unwrap()), [5, 1]);
        assert!(matches!(
            owners(&speakers, 5, &mut every),
            Err(OwnVoiceError::TooFewOwners(5))
        ));
    }

    #[test]
    fn similar_candidates_rank_the_owners_gender_by_likeness_without_the_owner() {
        let owner = speaker(1, Gender::Male, 2);
        let close = speaker(2, Gender::Male, 1);
        let closer_but_female = speaker(3, Gender::Female, 1);
        let scored = [
            (&owner, 1.0),
            (&close, 0.4),
            (&closer_but_female, 0.6),
            (&speaker(4, Gender::Male, 1), 0.2),
        ];

        let ranked: Vec<_> = by_likeness(&owner, &scored)
            .into_iter()
            .map(|(speaker, likeness)| (speaker.id, likeness))
            .collect();

        assert_eq!(ranked, [(2, 0.4), (4, 0.2)]);
    }

    fn voices() -> Vec<(Actor, Lines)> {
        let utterance = |text: &str, samples: usize| Utterance {
            text: text.to_owned(),
            samples: vec![0.5; samples],
        };
        let lines = |name: &str| Lines {
            long: (0..LONG_TURNS)
                .map(|index| utterance(&format!("{name} {index}"), 16_000))
                .collect(),
            short: utterance(name, 8_000),
        };
        vec![
            (Actor::Owner, lines("owner")),
            (Actor::Other, lines("other")),
            (Actor::Similar, lines("similar")),
        ]
    }

    #[test]
    fn an_overlapping_turn_reaches_back_and_its_owner_layout_keeps_the_clock() {
        let turns = [
            (
                Start::After(Duration::from_millis(500)),
                Actor::Owner,
                Said::Long(0),
            ),
            (
                Start::Over(Duration::from_millis(250)),
                Actor::Other,
                Said::Short,
            ),
        ];

        let voices = voices();
        let voices: Vec<(Actor, &Lines)> = voices
            .iter()
            .map(|(actor, lines)| (*actor, lines))
            .collect();
        let (mixed, placed) = lay_out(&turns, &voices, |_| true);
        let (owner_only, owner_placed) = lay_out(&turns, &voices, |actor| actor == Actor::Owner);

        let spans: Vec<_> = placed.iter().map(|turn| (turn.start, turn.end)).collect();
        assert_eq!(spans, [(0.5, 1.5), (1.25, 1.75)]);
        assert_eq!(mixed.len(), owner_only.len());
        assert_eq!(seconds(mixed.len()), 1.75 + TAIL.as_secs_f64());
        assert_eq!(
            mixed[canonical_samples(Duration::from_millis(1_300)) as usize],
            1.0
        );
        assert_eq!(owner_placed, placed[..1]);
        assert_eq!(
            owner_only[canonical_samples(Duration::from_millis(1_600)) as usize],
            0.0
        );
    }
}
