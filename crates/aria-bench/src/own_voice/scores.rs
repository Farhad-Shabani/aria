//! How far apart an enrolled owner's voice and other voices score, by how
//! much speech a check hears. The tracker's own-voice bars are fitted on this,
//! on LibriSpeech subsets the own-voice corpus never uses.
//!
//! Each speaker is enrolled from one chapter and heard from where speech
//! starts in utterances from their other chapters, as a turn's growing checks
//! hear it.
//! Every speaker is also heard against every other enrolled speaker of the
//! same gender, the harder half of the impostors.

use std::path::PathBuf;
use std::time::Duration;

use aria_bench_speaker::{
    Scored, equal_error_rate, rates_at, similarity, threshold_for_false_accept,
    threshold_for_false_reject,
};
use aria_harness::artifact::{Provenance, fingerprint, write_json};
use aria_kit::audio::canonical_samples;
use aria_kit::speaker::{Embedder, EnrollmentError};
use aria_kit::voice_activity::{SileroVad, VoiceActivityDetector};
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
/// The share of other voices the accept bar lets through, and of the owner's
/// voice the doubt bar turns away.
const ERROR_RATE: f64 = 0.01;

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
    /// Lets [`ERROR_RATE`] of other voices through.
    accept: Option<Bar>,
    /// Turns away [`ERROR_RATE`] of the owner's voice.
    doubt: Option<Bar>,
}

/// Scores every enrolled speaker against every same-gender window, prints a
/// table and writes the bars per window to `request.out`.
pub fn measure(request: &Request) -> Result<(), OwnVoiceError> {
    let mut scorer = embedder(super::speaker_config(&request.speaker_model))?;
    let mut vad = SileroVad::load().map_err(BenchError::SpeechGate)?;
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
                windows: windows(&archive, &mut scorer, &mut vad, speaker)?,
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

/// By [`WINDOWS`], the opening of each utterance outside the enrollment
/// chapter, counted from where speech starts. LibriSpeech utterances open on
/// silence, and a tracker's turn opens on speech.
fn windows(
    archive: &Archive,
    scorer: &mut impl Embedder,
    vad: &mut SileroVad,
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
        let mut audio = archive.read(id)?;
        vad.reset();
        let regions = vad.speech_regions(&audio).map_err(BenchError::SpeechGate)?;
        let Some(first) = regions.first() else {
            continue;
        };
        audio.drain(..first.start);
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
    let accept = threshold_for_false_accept(scored, ERROR_RATE).map(|at| Bar {
        at,
        costs: rates_at(scored, at).false_reject,
    });
    let doubt = threshold_for_false_reject(scored, ERROR_RATE).map(|at| Bar {
        at,
        costs: rates_at(scored, at).false_accept,
    });
    WindowScores {
        seconds,
        same: scored.iter().filter(|trial| trial.same_speaker).count(),
        different: scored.iter().filter(|trial| !trial.same_speaker).count(),
        equal_error: equal_error_rate(scored).map(|rates| rates.equal_error()),
        accept,
        doubt,
    }
}

fn print(summary: &[WindowScores]) {
    println!(
        "{:>5} {:>6} {:>7} {:>6}   accept (owner out)   doubt (others in)",
        "heard", "owner", "others", "EER"
    );
    let bar = |bar: &Option<Bar>| {
        bar.as_ref().map_or("-".into(), |bar| {
            format!("{:.3} ({:>5.1}%)", bar.at, bar.costs * 100.0)
        })
    };
    for window in summary {
        println!(
            "{:>4.1}s {:>6} {:>7} {:>5.1}%   {:>18}   {:>17}",
            window.seconds,
            window.same,
            window.different,
            window.equal_error.unwrap_or(f64::NAN) * 100.0,
            bar(&window.accept),
            bar(&window.doubt),
        );
    }
}
