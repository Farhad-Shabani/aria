//! Voice embeddings through the WeSpeaker ONNX graph.
//!
//! One window of 16 kHz audio in, one unit-norm vector out; two vectors of
//! the same voice point the same way, so a cosine says whether two stretches
//! of speech came from one person. Grouping those vectors into speakers is
//! the caller's policy, not this crate's.
//!
//! ```no_run
//! use aria_speaker::wespeaker::WeSpeaker;
//! use aria_kit::speaker::Embedder;
//!
//! # fn main() -> Result<(), aria_kit::speaker::SpeakerError> {
//! let mut engine = WeSpeaker::load(std::path::Path::new("model.onnx"))?;
//! let embedding = engine.embed(&vec![0.0; 16_000])?;
//! # Ok(())
//! # }
//! ```

mod fbank;

use std::path::{Path, PathBuf};

use aria_kit::backend::{BackendError, ModelDownload, Unpack};
use aria_kit::speaker::{Embedder, Engine, SpeakerError, normalized, validate_model_file};
use ort::session::Session;
use ort::value::Tensor;

use fbank::fbank;

pub const NAME: &str = "wespeaker";

/// ResNet34-LM, trained on VoxCeleb.
pub const MODEL: &[ModelDownload] = &[ModelDownload {
    file_name: "model.onnx",
    url: "https://huggingface.co/Wespeaker/wespeaker-voxceleb-resnet34-LM/resolve/f0c48c298fd835726c27956a5d617bad7115627e/voxceleb_resnet34_LM.onnx",
    bytes: 26_530_309,
    unpack: Unpack::Plain,
}];

/// The width of every embedding this model produces, and so of every
/// enrolled profile that can be compared against one.
pub const EMBEDDING_DIM: usize = 256;

/// Under half a second of voiced audio embeds too noisily to trust. Calibration knob.
const MIN_WINDOW_SAMPLES: usize = 8_000;
/// RMS below this is silence, not a voice. Calibration knob.
const SILENCE_RMS: f32 = 0.002;
const FEATURES_INPUT: &str = "feats";
const EMBEDDING_OUTPUT: &str = "embs";

pub struct Config {
    pub model: PathBuf,
}

/// A loaded WeSpeaker graph. Holding one is proof the model runs: loading
/// scores a dummy window before returning.
pub struct WeSpeaker {
    session: Session,
}

impl WeSpeaker {
    pub fn load(model: &Path) -> Result<Self, SpeakerError> {
        Self::load_from(Config {
            model: model.to_path_buf(),
        })
    }
}

impl Engine for WeSpeaker {
    const NAME: &'static str = NAME;
    const MODEL: &'static [ModelDownload] = MODEL;

    type Config = Config;

    fn check_from(config: &Self::Config) -> Result<PathBuf, SpeakerError> {
        validate_model_file(config.model.clone(), NAME)
    }

    fn load_from(config: Self::Config) -> Result<Self, SpeakerError> {
        let model = Self::check_from(&config)?;
        let mut session = Session::builder()
            .and_then(|mut builder| builder.commit_from_file(&model))
            .map_err(|source| SpeakerError::Load {
                backend: NAME,
                model: model.display().to_string(),
                source: BackendError::engine(source),
            })?;
        infer(&mut session, &[[0.0; fbank::N_MELS]; 100])?;
        Ok(Self { session })
    }
}

impl Embedder for WeSpeaker {
    /// Silent stretches are cut before the features are taken. A pause inside
    /// the window would otherwise skew the per-bin mean subtraction.
    fn embed(&mut self, window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
        let voiced: Vec<f32> = window
            .chunks(fbank::FRAME_LENGTH)
            .filter(|chunk| {
                let power = chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32;
                power.sqrt() >= SILENCE_RMS
            })
            .flatten()
            .copied()
            .collect();
        if voiced.len() < MIN_WINDOW_SAMPLES {
            return Ok(None);
        }
        let features = fbank(&voiced);
        if features.is_empty() {
            return Ok(None);
        }
        Ok(normalized(infer(&mut self.session, &features)?))
    }

    fn dimension(&self) -> usize {
        EMBEDDING_DIM
    }
}

fn infer(
    session: &mut Session,
    features: &[[f32; fbank::N_MELS]],
) -> Result<Vec<f32>, SpeakerError> {
    let inference = |source| SpeakerError::Inference {
        backend: NAME,
        source,
    };
    let flat: Vec<f32> = features.iter().flatten().copied().collect();
    let shape = [1, features.len() as i64, fbank::N_MELS as i64];
    let input = Tensor::from_array((shape, flat))
        .map_err(|error| inference(BackendError::engine(error)))?;
    let mut outputs = session
        .run(ort::inputs![FEATURES_INPUT => input])
        .map_err(|error| inference(BackendError::engine(error)))?;
    let value = outputs.remove(EMBEDDING_OUTPUT).ok_or_else(|| {
        inference(BackendError::Unusable(format!(
            "the model returned no {EMBEDDING_OUTPUT} output"
        )))
    })?;
    let (_shape, values) = value
        .try_extract_tensor::<f32>()
        .map_err(|error| inference(BackendError::engine(error)))?;
    if values.len() != EMBEDDING_DIM {
        return Err(inference(BackendError::Unusable(format!(
            "the model returned {} values, expected {EMBEDDING_DIM}",
            values.len()
        ))));
    }
    Ok(values.to_vec())
}
