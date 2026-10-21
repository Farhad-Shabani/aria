//! Who is speaking, as a vector: the contract every speaker-embedding engine
//! implements.
//!
//! An engine turns a window of 16 kHz samples into a unit-norm embedding.
//! Two embeddings of one voice point the same way, so their cosine says
//! whether two stretches of speech came from the same person. What to do
//! with that number is policy, kept out of every engine: [`Diarizer`] groups
//! voices and claims enrolled names, and `SegmentReader` applies it to the
//! segments a transcription session emits.

mod diarizer;
#[cfg(not(target_family = "wasm"))]
mod enrollment;
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
mod labeler;
mod profile;
#[cfg(not(target_family = "wasm"))]
mod store;

use std::path::PathBuf;

use crate::backend::{BackendError, ModelDownload, ModelError};

pub use diarizer::{CLUSTER_THRESHOLD, Diarizer, ENROLLED_THRESHOLD, settle};
#[cfg(not(target_family = "wasm"))]
pub use enrollment::{Enrolled, Enrollment, EnrollmentError, MIN_SAMPLES, centroid, consistency};
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
pub use labeler::{
    Heard, HeardUpdate, OWN_VOICE_THRESHOLD, SegmentLabeler, SegmentReader, Verdict,
};
#[cfg(any(test, feature = "test-support"))]
pub use profile::enrolled_voice;
pub use profile::{Profile, Role, SpeakerName, SpeakerNameError};
#[cfg(not(target_family = "wasm"))]
pub use store::{ProfileError, ProfileStore, Replace, StoredProfiles};

/// Turns audio into a comparable voice vector.
///
/// Implementations map their own failures through
/// [`BackendError::engine`](crate::backend::BackendError::engine) and name
/// themselves with [`Engine::NAME`].
pub trait Embedder {
    /// A unit-norm embedding for one window of 16 kHz samples.
    ///
    /// `Ok(None)` means the window carried no voice worth embedding — too
    /// short, or too quiet. That is an ordinary answer, not a failure, and a
    /// caller usually keeps whatever label it already had. `Err` means the
    /// model itself broke.
    fn embed(&mut self, window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError>;

    /// The width of every embedding this engine produces. A profile of any
    /// other width was made by a different engine and cannot be compared.
    fn dimension(&self) -> usize;
}

/// How an engine is constructed from its own configuration. `NAME` names the
/// engine in stored profiles and errors; `Config` carries whatever it needs
/// to load.
pub trait Engine: Embedder + Sized {
    const NAME: &'static str;

    /// The files to download for this engine, all into one folder. Empty
    /// when nothing is downloaded for it. An engine that reads a second
    /// folder lists that folder's files in a const of its own.
    const MODEL: &'static [ModelDownload];

    type Config;

    /// Resolves what a load would read, without reading it.
    fn check_from(config: &Self::Config) -> Result<PathBuf, SpeakerError>;

    fn load_from(config: Self::Config) -> Result<Self, SpeakerError>;
}

/// Scales a vector to unit length, so cosines are plain dot products.
/// `None` when the vector has no direction to keep.
pub fn normalized(mut values: Vec<f32>) -> Option<Vec<f32>> {
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_normal() {
        return None;
    }
    for value in &mut values {
        *value /= norm;
    }
    Some(values)
}

/// [`crate::backend::validate_model_file`], failing as a [`SpeakerError`].
#[cfg(not(target_family = "wasm"))]
pub fn validate_model_file(path: PathBuf, backend: &'static str) -> Result<PathBuf, SpeakerError> {
    crate::backend::validate_model_file(path)
        .map_err(|source| SpeakerError::Model { backend, source })
}

#[derive(Debug, thiserror::Error)]
pub enum SpeakerError {
    #[error("the {backend} model cannot be used")]
    Model {
        backend: &'static str,
        #[source]
        source: ModelError,
    },

    #[error("failed to load the {backend} model {model}")]
    Load {
        backend: &'static str,
        model: String,
        #[source]
        source: BackendError,
    },

    #[error("{backend} inference failed")]
    Inference {
        backend: &'static str,
        #[source]
        source: BackendError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_leaves_a_unit_vector_pointing_the_same_way() {
        let unit = normalized(vec![3.0, 4.0]).expect("the vector has a direction");
        assert!((unit[0] - 0.6).abs() < 1e-6);
        assert!((unit[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn a_vector_without_direction_cannot_be_normalized() {
        assert!(normalized(vec![0.0, 0.0]).is_none());
    }
}
