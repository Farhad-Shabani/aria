//! Who is speaking *when*: the contract every speech-segmentation engine
//! implements.
//!
//! An embedder answers "whose voice is this window" and needs the window to
//! hold one person. A segmenter answers the question underneath: given a
//! fixed stretch of audio, which voices are active in each frame of it, and
//! where does one stop and the next begin. Two people talking at once is an
//! ordinary answer here rather than a window nobody can embed.
//!
//! What a caller does with those frames — masking audio per voice, grouping
//! the voices across a recording, naming them — is policy, not an engine's
//! business.

use std::path::PathBuf;

use crate::backend::{BackendError, ModelDownload, ModelError};

/// Speakers active in each frame of one fixed-width chunk of audio.
///
/// Implementations map their own failures through
/// [`BackendError::engine`](crate::backend::BackendError::engine) and name
/// themselves with [`Engine::NAME`].
pub trait Segmenter {
    /// The chunk width this engine reads and the frame grid it answers on.
    /// Fixed for the life of the engine — it is a property of the graph.
    fn shape(&self) -> ChunkShape;

    /// Who speaks in each frame of `chunk`, which must be exactly
    /// [`ChunkShape::samples`] long. A caller with less audio than that pads
    /// it; a caller with more cuts it.
    fn segment(&mut self, chunk: &[f32]) -> Result<Vec<Activity>, SegmentationError>;
}

/// How an engine is constructed from its own configuration. Implementing
/// this beside [`Segmenter`] is the whole checklist for a new engine.
pub trait Engine: Segmenter + Sized {
    const NAME: &'static str;

    /// The files to download for this engine, all into one folder. Empty
    /// when nothing is downloaded for it. An engine that reads a second
    /// folder lists that folder's files in a const of its own.
    const MODEL: &'static [ModelDownload];

    type Config;

    /// Resolves what a load would read, without reading it.
    fn check_from(config: &Self::Config) -> Result<PathBuf, SegmentationError>;

    fn load_from(config: Self::Config) -> Result<Self, SegmentationError>;
}

/// The grid one engine answers on.
///
/// `frame_size` is wider than `frame_shift` — frames overlap — so a frame's
/// time is the centre of the audio behind it rather than its leading edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkShape {
    /// Samples in one chunk, at 16 kHz.
    pub samples: usize,
    /// Samples from one frame to the next.
    pub frame_shift: usize,
    /// Samples one frame answers for.
    pub frame_size: usize,
    /// Voices the engine can tell apart inside a single chunk. They are
    /// local to the chunk: voice 0 of one chunk and voice 0 of the next are
    /// not the same person.
    pub speakers: usize,
}

/// Which of a chunk's local voices speak in one frame.
///
/// A bitset rather than a `Vec<bool>`: there is one of these per frame and a
/// ten-second chunk holds hundreds, so the whole answer stays in cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Activity(u8);

impl Activity {
    /// Nobody speaking.
    pub const SILENT: Self = Self(0);

    /// The most local voices any engine may report. Beyond this an activity
    /// would not fit its bitset, and no published segmenter comes close.
    pub const MAX_SPEAKERS: usize = 8;

    /// Marks `local` as speaking. Positions at or past [`Self::MAX_SPEAKERS`]
    /// are ignored — an engine claiming more voices than the set holds is a
    /// bug in that engine, and dropping the overflow keeps the frames it did
    /// get right.
    pub fn speaking(mut self, local: usize) -> Self {
        if local < Self::MAX_SPEAKERS {
            self.0 |= 1 << local;
        }
        self
    }

    pub fn speaks(self, local: usize) -> bool {
        local < Self::MAX_SPEAKERS && self.0 & (1 << local) != 0
    }

    /// How many voices are speaking at once.
    pub fn voices(self) -> usize {
        self.0.count_ones() as usize
    }
}

/// The shared model validator, answering in this capability's vocabulary.
#[cfg(not(target_family = "wasm"))]
pub fn validate_model_file(
    path: PathBuf,
    backend: &'static str,
) -> Result<PathBuf, SegmentationError> {
    crate::backend::validate_model_file(path)
        .map_err(|source| SegmentationError::Model { backend, source })
}

#[derive(Debug, thiserror::Error)]
pub enum SegmentationError {
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
    fn silence_names_nobody() {
        assert_eq!(Activity::SILENT.voices(), 0);
        assert!(!Activity::SILENT.speaks(0));
    }

    #[test]
    fn two_voices_at_once_is_an_ordinary_answer() {
        let both = Activity::SILENT.speaking(0).speaking(2);
        assert_eq!(both.voices(), 2);
        assert!(both.speaks(0) && both.speaks(2));
        assert!(!both.speaks(1));
    }

    #[test]
    fn a_voice_the_set_cannot_hold_is_dropped_not_folded_onto_another() {
        let overflowed = Activity::SILENT.speaking(Activity::MAX_SPEAKERS);
        assert_eq!(overflowed.voices(), 0);
        assert!(!overflowed.speaks(0));
    }
}
