//! Measures speaker verification errors across similarity thresholds.
//!
//! Each trial compares a test recording with the normalized mean of enrollment
//! embeddings.

mod report;
mod trials;

pub use report::{
    Rates, Report, equal_error_rate, rates_at, threshold_for_false_accept,
    threshold_for_false_reject,
};
pub use trials::{Trial, read_trials};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aria_kit::audio::{WavError, read_wav};
use aria_kit::speaker::{Embedder, SpeakerError, centroid, consistency};
use aria_speaker::wespeaker::WeSpeaker;

/// One trial's outcome: whether it was the same person, and how close the
/// voices came.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scored {
    pub same_speaker: bool,
    /// Cosine of the enrolled centroid against the test utterance.
    pub score: f32,
}

/// How strongly one enrollment's samples agreed. Only enrollments built from
/// more than one recording have an answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Consistency {
    pub samples: usize,
    pub score: f32,
}

/// Every trial scored, plus each multi-sample trial's enrollment consistency.
pub struct Measured {
    pub scored: Vec<Scored>,
    pub consistency: Vec<Consistency>,
    /// Trials skipped because a recording held no voice to embed. Counted
    /// rather than dropped silently: a corpus that mostly fails to embed
    /// would otherwise report a flattering error rate over what is left.
    pub unusable: usize,
}

/// Scores every trial against one loaded model.
///
/// Recordings are embedded once and reused, so a corpus that names the same
/// file in many trials pays for it once.
pub fn measure(model: &Path, trials: &[Trial]) -> Result<Measured, VerifyError> {
    let mut engine = WeSpeaker::load(model).map_err(VerifyError::Model)?;
    let mut embeddings: HashMap<PathBuf, Option<Vec<f32>>> = HashMap::new();
    let mut measured = Measured {
        scored: Vec::new(),
        consistency: Vec::new(),
        unusable: 0,
    };

    for trial in trials {
        let mut samples = Vec::new();
        for recording in &trial.enrollment {
            match embedding(&mut engine, &mut embeddings, recording)? {
                Some(embedding) => samples.push(embedding),
                None => break,
            }
        }
        let heard = embedding(&mut engine, &mut embeddings, &trial.test)?;
        let (Some(heard), true) = (heard, samples.len() == trial.enrollment.len()) else {
            measured.unusable += 1;
            continue;
        };

        if samples.len() > 1 {
            measured.consistency.push(Consistency {
                samples: samples.len(),
                score: consistency(&samples),
            });
        }
        let Some(enrolled) = centroid(&samples) else {
            measured.unusable += 1;
            continue;
        };
        measured.scored.push(Scored {
            same_speaker: trial.same_speaker,
            score: similarity(&enrolled, &heard),
        });
    }
    Ok(measured)
}

/// How alike two voices are: the cosine of two unit-norm embeddings, so a
/// plain dot product.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn embedding(
    engine: &mut WeSpeaker,
    seen: &mut HashMap<PathBuf, Option<Vec<f32>>>,
    recording: &Path,
) -> Result<Option<Vec<f32>>, VerifyError> {
    if !seen.contains_key(recording) {
        let clip = read_wav(recording)?;
        if !clip.is_canonical() {
            return Err(VerifyError::NotCanonical {
                path: recording.to_path_buf(),
            });
        }
        let samples = clip.into_samples();
        let embedded = engine.embed(&samples).map_err(VerifyError::Model)?;
        seen.insert(recording.to_path_buf(), embedded);
    }
    Ok(seen[recording].clone())
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("the trial list {} could not be read", path.display())]
    TrialList {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{}:{line}: a trial needs a label, an enrollment, and a test recording", path.display())]
    Trial { path: PathBuf, line: usize },

    #[error("{}:{line}: the label must be 1 for one voice or 0 for two", path.display())]
    Label { path: PathBuf, line: usize },

    #[error("the recording could not be read")]
    Wav(#[from] WavError),

    #[error("the recording {} is not at 16 kHz", path.display())]
    NotCanonical { path: PathBuf },

    #[error("the speaker model could not be used")]
    Model(#[source] SpeakerError),
}
