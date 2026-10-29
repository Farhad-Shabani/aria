//! How far apart an enrolled owner's voice and other voices score, by how
//! much speech a check hears. The tracker's own-voice bars are fitted on this,
//! on LibriSpeech subsets the own-voice corpus never uses.
//!
//! Each speaker is enrolled from one chapter and heard in the openings of
//! utterances from their other chapters, as a turn's growing checks hear it.
//! Every speaker is also heard against every other enrolled speaker of the
//! same gender, the harder half of the impostors.

use std::path::PathBuf;
use std::time::Duration;

use aria_bench_speaker::{
    Rates, Scored, equal_error_rate, rates_at, similarity, threshold_for_false_accept,
    threshold_for_false_reject,
};
use aria_harness::artifact::{Provenance, fingerprint, write_json};
use aria_kit::audio::canonical_samples;
use aria_kit::speaker::{Embedder, EnrollmentError, OWN_VOICE_THRESHOLD};
use serde::Serialize;

use super::{Archive, OwnVoiceError, enroll, length};
use crate::diarize::{BenchError, embedder};
use crate::stt::librispeech::{self, Gender, LibrispeechError, Speaker, Subset};

/// The speech a check scores: the tracker checks a growing turn every 0.5 s
/// and judges it on at most 3 s.
const WINDOWS: [Duration; 6] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_millis(1_500),
    Duration::from_secs(2),
    Duration::from_millis(2_500),
    super::JUDGED,
];
/// Utterances per speaker that trials open on.
const TRIAL_UTTERANCES: usize = 20;
/// Shares of other voices a bar may let through.
const FALSE_ACCEPTS: [f64; 2] = [0.01, 0.001];
/// Shares of the owner's voice a bar may turn away.
const FALSE_REJECTS: [f64; 2] = [0.01, 0.05];

pub struct Request {
    /// Where the subsets' archives are, or are downloaded to.
    pub librispeech: PathBuf,
    pub subsets: Vec<Subset>,
    pub speaker_model: PathBuf,
    pub out: PathBuf,
}

/// One speaker enrolled, with the windows they are heard in.
struct Voice {
    id: u32,
    gender: Gender,
    profile: Vec<f32>,
    /// By [`WINDOWS`], each window's embedding.
    windows: Vec<Vec<Vec<f32>>>,
}

#[derive(Serialize)]
struct Bar {
    rate: f64,
    at: f32,
    /// The other error at this bar.
    costs: f64,
}

#[derive(Serialize)]
struct WindowScores {
    seconds: f64,
    same: usize,
    different: usize,
    equal_error: Option<f64>,
    /// Bars that let at most a share of other voices through, and the share
    /// of the owner's voice each turns away.
    accept: Vec<Bar>,
    /// Bars that turn away at most a share of the owner's voice, and the
    /// share of other voices each lets through.
    doubt: Vec<Bar>,
    /// What the bar the owner is named at costs at this length.
    naming: Cost,
}

#[derive(Serialize)]
struct Cost {
    at: f32,
    false_accept: f64,
    false_reject: f64,
}

impl From<Rates> for Cost {
    fn from(rates: Rates) -> Self {
        Self {
            at: rates.threshold,
            false_accept: rates.false_accept,
            false_reject: rates.false_reject,
        }
    }
}

/// Scores every enrolled speaker against every same-gender window, prints a
/// table and writes the bars per window to `request.out`.
pub fn measure(request: &Request) -> Result<(), OwnVoiceError> {
    let mut scorer = embedder(super::speaker_config(&request.speaker_model))?;
    let mut voices = Vec::new();
    let mut unenrolled = Vec::new();
    let cache = &request.librispeech;
    for &subset in &request.subsets {
        librispeech::download(cache, subset).map_err(LibrispeechError::from)?;
        let archive = Archive {
            speech: cache.clone(),
            subset,
            decoded: tempfile::tempdir().map_err(BenchError::Workspace)?,
        };
        for speaker in librispeech::speakers(cache, subset)?
            .iter()
            .filter(|speaker| speaker.chapters.len() >= 2)
        {
            let profile = match enroll(&request.speaker_model, &archive, &speaker.chapters[0]) {
                Ok(enrolled) => enrolled.profile.vector,
                Err(OwnVoiceError::Enrollment(EnrollmentError::Inconsistent { .. })) => {
                    unenrolled.push(speaker.id);
                    continue;
                }
                Err(error) => return Err(error),
            };
            voices.push(Voice {
                id: speaker.id,
                gender: speaker.gender,
                profile,
                windows: windows(&archive, &mut scorer, speaker)?,
            });
        }
    }
    let summary: Vec<WindowScores> = WINDOWS
        .iter()
        .enumerate()
        .map(|(index, window)| summarize(window.as_secs_f64(), &scored(&voices, index)))
        .collect();
    print(&summary);
    let subsets: Vec<&str> = request.subsets.iter().map(|subset| subset.name()).collect();
    write_json(
        &request.out,
        &serde_json::json!({
            "subsets": subsets,
            "speaker_model_sha256": fingerprint(&request.speaker_model)?,
            "speakers": voices.len(),
            "unenrolled": unenrolled,
            "windows": summary,
            "provenance": Provenance::capture()?,
        }),
    )?;
    Ok(())
}

/// By [`WINDOWS`], the opening of each long-enough utterance outside the
/// enrollment chapter.
fn windows(
    archive: &Archive,
    scorer: &mut impl Embedder,
    speaker: &Speaker,
) -> Result<Vec<Vec<Vec<f32>>>, OwnVoiceError> {
    let longest = WINDOWS[WINDOWS.len() - 1];
    let mut openings = Vec::new();
    for (id, _) in speaker.chapters[1..]
        .iter()
        .flat_map(|chapter| &chapter.utterances)
    {
        if openings.len() == TRIAL_UTTERANCES {
            break;
        }
        let audio = archive.read(id)?;
        if length(audio.len()) >= longest {
            openings.push(audio);
        }
    }
    let mut windows = Vec::new();
    for window in WINDOWS {
        let samples = canonical_samples(window) as usize;
        let mut embeddings = Vec::new();
        for opening in &openings {
            if let Some(embedding) = scorer.embed(&opening[..samples])? {
                embeddings.push(embedding);
            }
        }
        windows.push(embeddings);
    }
    Ok(windows)
}

/// Every window of one length against every same-gender profile.
fn scored(voices: &[Voice], window: usize) -> Vec<Scored> {
    let mut scored = Vec::new();
    for heard in voices {
        for embedding in &heard.windows[window] {
            for enrolled in voices
                .iter()
                .filter(|enrolled| enrolled.gender == heard.gender)
            {
                scored.push(Scored {
                    same_speaker: enrolled.id == heard.id,
                    score: similarity(&enrolled.profile, embedding),
                });
            }
        }
    }
    scored
}

fn summarize(seconds: f64, scored: &[Scored]) -> WindowScores {
    let accept = FALSE_ACCEPTS
        .iter()
        .filter_map(|&rate| {
            let at = threshold_for_false_accept(scored, rate)?;
            Some(Bar {
                rate,
                at,
                costs: rates_at(scored, at).false_reject,
            })
        })
        .collect();
    let doubt = FALSE_REJECTS
        .iter()
        .filter_map(|&rate| {
            let at = threshold_for_false_reject(scored, rate)?;
            Some(Bar {
                rate,
                at,
                costs: rates_at(scored, at).false_accept,
            })
        })
        .collect();
    WindowScores {
        seconds,
        same: scored.iter().filter(|trial| trial.same_speaker).count(),
        different: scored.iter().filter(|trial| !trial.same_speaker).count(),
        equal_error: equal_error_rate(scored).map(|rates| rates.equal_error()),
        accept,
        doubt,
        naming: rates_at(scored, OWN_VOICE_THRESHOLD).into(),
    }
}

fn print(summary: &[WindowScores]) {
    println!(
        "{:>5} {:>6} {:>7} {:>6}   accept bar at FA (owner out)        doubt bar at FR (others in)",
        "heard", "owner", "others", "EER"
    );
    let bar = |bars: &[Bar], index: usize| {
        bars.get(index).map_or("-".into(), |bar| {
            format!(
                "{:>4.1}%: {:.3} ({:>5.1}%)",
                bar.rate * 100.0,
                bar.at,
                bar.costs * 100.0
            )
        })
    };
    for window in summary {
        println!(
            "{:>4.1}s {:>6} {:>7} {:>5.1}%   {}  {}   {}  {}",
            window.seconds,
            window.same,
            window.different,
            window.equal_error.unwrap_or(f64::NAN) * 100.0,
            bar(&window.accept, 0),
            bar(&window.accept, 1),
            bar(&window.doubt, 0),
            bar(&window.doubt, 1),
        );
    }
}
