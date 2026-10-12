//! Building one enrolled profile out of several spoken samples.
//!
//! Averages embeddings from several utterances to reduce variation between samples.
//! Rejects enrollment when the samples disagree too much.

use super::{Engine, Profile, Role, SpeakerError, SpeakerName, normalized};

/// Below two samples there is nothing to average and no disagreement to
/// measure.
pub const MIN_SAMPLES: usize = 2;
/// Minimum mean pairwise cosine. Leaves room for noisier recordings than the
/// LibriSpeech calibration corpus. Calibration results are recorded in
/// `crates/aria-bench-speaker/baselines/speaker.json`; rerun `aria-bench-speaker`
/// before changing the threshold.
const MIN_CONSISTENCY: f32 = 0.40;

/// Samples gathered so far, and the engine that measures them.
pub struct Enrollment<E> {
    embedder: E,
    samples: Vec<Vec<f32>>,
}

impl<E: Engine> Enrollment<E> {
    pub fn new(embedder: E) -> Self {
        Self {
            embedder,
            samples: Vec::new(),
        }
    }

    /// Measures one utterance.
    ///
    /// [`EnrollmentError::NoVoice`] means the window held nothing to embed —
    /// too short, or too quiet. Whether that ends the enrollment or asks for
    /// the sample again is the caller's to decide.
    pub fn add(&mut self, window: &[f32]) -> Result<(), EnrollmentError> {
        match self.embedder.embed(window)? {
            Some(embedding) => {
                self.samples.push(embedding);
                Ok(())
            }
            None => Err(EnrollmentError::NoVoice),
        }
    }

    /// The profile the samples agree on, and how strongly they agree.
    pub fn finish(self, name: SpeakerName, role: Role) -> Result<Enrolled, EnrollmentError> {
        if self.samples.len() < MIN_SAMPLES {
            return Err(EnrollmentError::TooFewSamples {
                samples: self.samples.len(),
                minimum: MIN_SAMPLES,
            });
        }

        let consistency = consistency(&self.samples);
        if consistency < MIN_CONSISTENCY {
            return Err(EnrollmentError::Inconsistent {
                consistency,
                minimum: MIN_CONSISTENCY,
            });
        }

        let vector = centroid(&self.samples).ok_or(EnrollmentError::NoDirection)?;
        Ok(Enrolled {
            profile: Profile {
                name,
                role,
                engine: E::NAME.to_owned(),
                enrolled_at: std::time::SystemTime::now(),
                vector,
            },
            consistency,
        })
    }
}

/// A finished enrollment: the profile to store, and the score worth showing
/// the person who just recorded it.
pub struct Enrolled {
    pub profile: Profile,
    /// Mean pairwise cosine of the samples, 0.0 to 1.0.
    pub consistency: f32,
}

/// Normalizes the elementwise mean of unit-length embeddings.
/// Returns `None` when the mean has no direction.
pub fn centroid(samples: &[Vec<f32>]) -> Option<Vec<f32>> {
    let width = samples.first()?.len();
    let mut sum = vec![0.0; width];
    for sample in samples {
        for (total, value) in sum.iter_mut().zip(sample) {
            *total += value;
        }
    }
    for total in &mut sum {
        *total /= samples.len() as f32;
    }
    normalized(sum)
}

/// Mean pairwise cosine similarity of unit-length embeddings.
pub fn consistency(samples: &[Vec<f32>]) -> f32 {
    let mut total = 0.0;
    let mut pairs = 0;
    for (at, sample) in samples.iter().enumerate() {
        for other in &samples[at + 1..] {
            total += sample.iter().zip(other).map(|(a, b)| a * b).sum::<f32>();
            pairs += 1;
        }
    }
    total / pairs as f32
}

#[derive(Debug, thiserror::Error)]
pub enum EnrollmentError {
    #[error("that sample carried no voice")]
    NoVoice,

    #[error("enrolling needs {minimum} samples; {samples} were recorded")]
    TooFewSamples { samples: usize, minimum: usize },

    #[error(
        "the samples sound like different voices (consistency {consistency:.2}, needs {minimum:.2})"
    )]
    Inconsistent { consistency: f32, minimum: f32 },

    #[error("the samples average to nothing")]
    NoDirection,

    #[error(transparent)]
    Engine(#[from] SpeakerError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speaker::Embedder;
    use std::path::PathBuf;

    /// Answers with whatever it was told to, in order.
    struct Replay {
        embeddings: Vec<Option<Vec<f32>>>,
    }

    impl Embedder for Replay {
        fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
            Ok(self.embeddings.remove(0))
        }

        fn dimension(&self) -> usize {
            2
        }
    }

    impl Engine for Replay {
        const NAME: &'static str = "replay";

        type Config = ();

        fn check_from(_config: &Self::Config) -> Result<PathBuf, SpeakerError> {
            Ok(PathBuf::new())
        }

        fn load_from(_config: Self::Config) -> Result<Self, SpeakerError> {
            Ok(Self {
                embeddings: Vec::new(),
            })
        }
    }

    fn enrolling(embeddings: Vec<Option<Vec<f32>>>) -> Enrollment<Replay> {
        Enrollment::new(Replay { embeddings })
    }

    fn ada() -> SpeakerName {
        SpeakerName::new("ada").expect("the name is usable")
    }

    #[test]
    fn one_voice_averages_to_itself_and_scores_perfectly() {
        let voice = vec![0.6, 0.8];
        let mut enrollment = enrolling(vec![Some(voice.clone()), Some(voice.clone())]);
        enrollment.add(&[]).expect("the sample carries a voice");
        enrollment.add(&[]).expect("the sample carries a voice");

        let enrolled = enrollment
            .finish(ada(), Role::Owner)
            .expect("the samples agree");
        assert!((enrolled.consistency - 1.0).abs() < 1e-6);
        assert!((enrolled.profile.vector[0] - voice[0]).abs() < 1e-6);
        assert_eq!(enrolled.profile.engine, "replay");
    }

    #[test]
    fn the_profile_is_unit_norm() {
        let mut enrollment = enrolling(vec![Some(vec![1.0, 0.0]), Some(vec![0.8, 0.6])]);
        enrollment.add(&[]).expect("the sample carries a voice");
        enrollment.add(&[]).expect("the sample carries a voice");

        let enrolled = enrollment
            .finish(ada(), Role::Guest)
            .expect("the samples agree");
        let norm: f32 = enrolled.profile.vector.iter().map(|v| v * v).sum();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_silent_sample_is_reported_as_one_sample() {
        let mut enrollment = enrolling(vec![None]);
        assert!(matches!(enrollment.add(&[]), Err(EnrollmentError::NoVoice)));
        assert!(enrollment.samples.is_empty());
    }

    #[test]
    fn two_different_voices_do_not_enroll() {
        let mut enrollment = enrolling(vec![Some(vec![1.0, 0.0]), Some(vec![0.0, 1.0])]);
        enrollment.add(&[]).expect("the sample carries a voice");
        enrollment.add(&[]).expect("the sample carries a voice");

        assert!(matches!(
            enrollment.finish(ada(), Role::Owner),
            Err(EnrollmentError::Inconsistent { .. })
        ));
    }

    #[test]
    fn one_sample_is_not_an_enrollment() {
        let mut enrollment = enrolling(vec![Some(vec![1.0, 0.0])]);
        enrollment.add(&[]).expect("the sample carries a voice");

        assert!(matches!(
            enrollment.finish(ada(), Role::Owner),
            Err(EnrollmentError::TooFewSamples { samples: 1, .. })
        ));
    }
}
