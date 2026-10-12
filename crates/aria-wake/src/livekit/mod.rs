//! livekit scores a whole two-second window at a time.

use std::collections::VecDeque;

pub use crate::{Config, ConfigError};

use crate::frontend::{
    AudioScale, Classifier, ClassifierSignature, Embedding, Error, FEATURE_LEN, FEATURE_WIDTH,
    FEATURE_WINDOW_FRAMES, MEL_WINDOW_FRAMES, Mel, ModelFiles,
};
use aria_kit::wake_word::{WakeWordDetector, WakeWordMatch};

pub const NAME: &str = "livekit";

/// livekit exports with a dynamic batch dimension; openWakeWord pins it to one.
const SIGNATURE: ClassifierSignature = ClassifierSignature {
    input: "embeddings",
    input_shape: &[-1, 16, 96],
    output: "score",
    output_shape: &[-1, 1],
};

const SCALE: AudioScale = AudioScale::Unit;

/// Yields exactly the 16 embeddings the head reads.
const WINDOW_SAMPLES: usize = 32_000;
/// 320 ms between scored windows, as livekit's own listener uses.
const HOP_SAMPLES: usize = 5_120;
/// Mel frames between consecutive embedding windows.
const EMBEDDING_STRIDE: usize = 8;

pub struct LiveKitWakeWord {
    mel: Mel,
    embedding: Embedding,
    classifier: Classifier,
    threshold: f32,
    window: VecDeque<i16>,
    since_last_score: usize,
}

impl LiveKitWakeWord {
    pub fn check_from(config: &Config) -> Result<ModelFiles, Error> {
        let files = ModelFiles::beside(config.classifier())?;
        Classifier::load(&files.classifier, SIGNATURE)?;
        Ok(files)
    }

    pub fn load_from(config: &Config) -> Result<Self, Error> {
        let files = Self::check_from(config)?;
        Ok(Self {
            mel: Mel::load(&files.melspectrogram, SCALE)?,
            embedding: Embedding::load(&files.embedding)?,
            classifier: Classifier::load(&files.classifier, SIGNATURE)?,
            threshold: config.threshold(),
            window: VecDeque::with_capacity(WINDOW_SAMPLES),
            since_last_score: 0,
        })
    }

    fn detect_inner(&mut self, canonical_samples: &[i16]) -> Result<WakeWordMatch, Error> {
        let mut matched = WakeWordMatch::NoMatch;
        for sample in canonical_samples {
            self.window.push_back(*sample);
            if self.window.len() > WINDOW_SAMPLES {
                self.window.pop_front();
            }
            self.since_last_score += 1;
            if self.since_last_score < HOP_SAMPLES || self.window.len() < WINDOW_SAMPLES {
                continue;
            }
            self.since_last_score = 0;
            if self.score()? >= self.threshold {
                matched = WakeWordMatch::Detected;
            }
        }
        Ok(matched)
    }

    /// Scores the buffered window the way livekit's own runtime does: mel over
    /// the whole window, then the last 16 embeddings taken at stride 8.
    fn score(&mut self) -> Result<f32, Error> {
        let samples: Vec<i16> = self.window.iter().copied().collect();
        let frames = self.mel.frames(&samples)?;
        let last_start = frames
            .len()
            .checked_sub(MEL_WINDOW_FRAMES)
            .map(|last| last / EMBEDDING_STRIDE * EMBEDDING_STRIDE)
            .filter(|start| start / EMBEDDING_STRIDE + 1 >= FEATURE_WINDOW_FRAMES)
            .ok_or_else(|| Error::Incompatible {
                role: "melspectrogram",
                detail: format!(
                    "returned {} frames for a {WINDOW_SAMPLES}-sample window, too few for {FEATURE_WINDOW_FRAMES} embeddings",
                    frames.len()
                ),
            })?;

        let mut features: Vec<[f32; FEATURE_WIDTH]> = Vec::with_capacity(FEATURE_WINDOW_FRAMES);
        for step in (0..FEATURE_WINDOW_FRAMES).rev() {
            let start = last_start - step * EMBEDDING_STRIDE;
            features.push(
                self.embedding
                    .feature(&frames[start..start + MEL_WINDOW_FRAMES])?,
            );
        }

        let window: Vec<f32> = features.iter().flatten().copied().collect();
        let window: [f32; FEATURE_LEN] = window.try_into().map_err(|_| Error::Incompatible {
            role: "embedding",
            detail: "built an incomplete classifier window".to_owned(),
        })?;
        self.classifier.score(&window)
    }
}

impl WakeWordDetector for LiveKitWakeWord {
    type Error = Error;

    fn detect(&mut self, canonical_samples: &[i16]) -> Result<WakeWordMatch, Self::Error> {
        match self.detect_inner(canonical_samples) {
            Ok(matched) => Ok(matched),
            Err(error) => {
                self.reset();
                Err(error)
            }
        }
    }

    fn reset(&mut self) {
        self.window.clear();
        self.since_last_score = 0;
    }
}
