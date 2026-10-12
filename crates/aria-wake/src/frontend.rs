//! Shared mel, embedding, and classifier stages for the wake engines.

use std::path::{Path, PathBuf};

use ort::{
    session::{Session, SessionOutputs},
    value::{Outlet, Tensor, TensorElementType, ValueType},
};

use aria_kit::backend::{ModelError, validate_model_file};

pub(crate) const FEATURE_LEN: usize = FEATURE_WINDOW_FRAMES * FEATURE_WIDTH;

pub(crate) const MEL_WINDOW_FRAMES: usize = 76;
pub(crate) const MEL_BINS: usize = 32;
pub(crate) const FEATURE_WINDOW_FRAMES: usize = 16;
pub(crate) const FEATURE_WIDTH: usize = 96;
const MEL_SCALE: f32 = 10.0;
const MEL_OFFSET: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioScale {
    /// Raw `i16` magnitudes, as openWakeWord trains.
    #[cfg(any(feature = "openwakeword", test))]
    Int16,
    /// Normalized to `[-1, 1]`, as livekit-wakeword trains.
    #[cfg(any(feature = "livekit", test))]
    Unit,
}

impl AudioScale {
    fn apply(self, sample: i16) -> f32 {
        match self {
            #[cfg(any(feature = "openwakeword", test))]
            Self::Int16 => f32::from(sample),
            #[cfg(any(feature = "livekit", test))]
            Self::Unit => aria_kit::audio::f32_from_pcm16(sample),
        }
    }
}

/// The two shared models sit beside the classifier, so naming the classifier
/// names all three.
pub struct ModelFiles {
    pub melspectrogram: PathBuf,
    pub embedding: PathBuf,
    pub classifier: PathBuf,
}

impl ModelFiles {
    pub fn beside(classifier: &Path) -> Result<Self, Error> {
        let classifier = trusted_model(classifier, "classifier")?;
        let directory = classifier.parent().ok_or_else(|| Error::Incompatible {
            role: "classifier",
            detail: "its canonical path has no parent directory".to_owned(),
        })?;
        Ok(Self {
            melspectrogram: trusted_model(
                &directory.join("melspectrogram.onnx"),
                "melspectrogram",
            )?,
            embedding: trusted_model(&directory.join("embedding_model.onnx"), "embedding")?,
            classifier,
        })
    }
}

/// Applies openWakeWord's `x / 10 + 2` transform.
pub(crate) struct Mel {
    session: Session,
    scale: AudioScale,
}

impl Mel {
    pub(crate) fn load(path: &Path, scale: AudioScale) -> Result<Self, Error> {
        let session = load(path, "melspectrogram")?;
        validate_model(
            &session,
            "melspectrogram",
            TensorSignature::new("input", &[-1, -1]),
            TensorSignature::new("output", &[-1, 1, -1, 32]),
        )?;
        Ok(Self { session, scale })
    }

    pub(crate) fn frames(&mut self, samples: &[i16]) -> Result<Vec<[f32; MEL_BINS]>, Error> {
        let audio: Vec<f32> = samples.iter().map(|s| self.scale.apply(*s)).collect();
        let sample_count = i64::try_from(audio.len()).map_err(|_| Error::Incompatible {
            role: "melspectrogram",
            detail: "audio window is too large".to_owned(),
        })?;
        let input = tensor(&[1, sample_count], audio)?;
        let mut outputs = self
            .session
            .run(ort::inputs!["input" => input])
            .map_err(Error::Inference)?;
        let values = output(&mut outputs, "melspectrogram", "output", None)?;
        if values.is_empty() || !values.len().is_multiple_of(MEL_BINS) {
            return Err(Error::Incompatible {
                role: "melspectrogram",
                detail: format!("returned {} values, not complete mel frames", values.len()),
            });
        }
        values
            .as_chunks::<MEL_BINS>()
            .0
            .iter()
            .map(|frame| {
                let transformed: Vec<f32> = frame
                    .iter()
                    .map(|value| value / MEL_SCALE + MEL_OFFSET)
                    .collect();
                transformed.try_into().map_err(|_| Error::Incompatible {
                    role: "melspectrogram",
                    detail: "returned an incomplete mel frame".to_owned(),
                })
            })
            .collect()
    }
}

pub(crate) struct Embedding {
    session: Session,
}

impl Embedding {
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        let session = load(path, "embedding")?;
        validate_model(
            &session,
            "embedding",
            TensorSignature::new("input_1", &[-1, 76, 32, 1]),
            TensorSignature::new("conv2d_19", &[-1, 1, 1, 96]),
        )?;
        Ok(Self { session })
    }

    pub(crate) fn feature(
        &mut self,
        window: &[[f32; MEL_BINS]],
    ) -> Result<[f32; FEATURE_WIDTH], Error> {
        if window.len() != MEL_WINDOW_FRAMES {
            return Err(Error::Incompatible {
                role: "embedding",
                detail: format!(
                    "given {} mel frames, expected {MEL_WINDOW_FRAMES}",
                    window.len()
                ),
            });
        }
        let spectrogram: Vec<f32> = window.iter().flatten().copied().collect();
        let input = tensor(
            &[1, MEL_WINDOW_FRAMES as i64, MEL_BINS as i64, 1],
            spectrogram,
        )?;
        let mut outputs = self
            .session
            .run(ort::inputs!["input_1" => input])
            .map_err(Error::Inference)?;
        output(
            &mut outputs,
            "embedding",
            "conv2d_19",
            Some(&[1, 1, 1, FEATURE_WIDTH as i64]),
        )?
        .try_into()
        .map_err(|values: Vec<f32>| Error::Incompatible {
            role: "embedding",
            detail: format!(
                "returned {} features, expected {FEATURE_WIDTH}",
                values.len()
            ),
        })
    }
}

pub(crate) struct Classifier {
    session: Session,
    signature: ClassifierSignature,
}

impl Classifier {
    pub(crate) fn load(path: &Path, signature: ClassifierSignature) -> Result<Self, Error> {
        let session = load(path, "classifier")?;
        validate_model(
            &session,
            "classifier",
            TensorSignature::new(signature.input, signature.input_shape),
            TensorSignature::new(signature.output, signature.output_shape),
        )?;
        let mut classifier = Self { session, signature };
        classifier.score(&[0.0; FEATURE_LEN])?;
        Ok(classifier)
    }

    /// The probability is bounds-checked because a mismatched head can return
    /// numbers a threshold would silently accept.
    pub(crate) fn score(&mut self, window: &[f32; FEATURE_LEN]) -> Result<f32, Error> {
        let signature = self.signature;
        let input = tensor(
            &[1, FEATURE_WINDOW_FRAMES as i64, FEATURE_WIDTH as i64],
            window.to_vec(),
        )?;
        let mut outputs = self
            .session
            .run(ort::inputs![signature.input => input])
            .map_err(Error::Inference)?;
        let values = output(&mut outputs, "classifier", signature.output, Some(&[1, 1]))?;
        let [probability] = values.as_slice() else {
            return Err(Error::Incompatible {
                role: "classifier",
                detail: format!("returned {} probabilities, expected one", values.len()),
            });
        };
        if !probability.is_finite() || !(0.0..=1.0).contains(probability) {
            return Err(Error::InvalidProbability(*probability));
        }
        Ok(*probability)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ClassifierSignature {
    pub(crate) input: &'static str,
    pub(crate) input_shape: &'static [i64],
    pub(crate) output: &'static str,
    pub(crate) output_shape: &'static [i64],
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the {role} wake-word model cannot be used")]
    Model {
        role: &'static str,
        #[source]
        source: ModelError,
    },

    #[error("failed to load the {role} wake-word model")]
    Load {
        role: &'static str,
        #[source]
        source: ort::Error,
    },

    #[error("wake-word inference failed")]
    Inference(#[source] ort::Error),

    #[error("the {role} wake-word model is incompatible: {detail}")]
    Incompatible { role: &'static str, detail: String },

    #[error("the wake-word classifier returned invalid probability {0}")]
    InvalidProbability(f32),
}

fn trusted_model(path: &Path, role: &'static str) -> Result<PathBuf, Error> {
    validate_model_file(path.to_path_buf()).map_err(|source| Error::Model { role, source })
}

fn load(path: &Path, role: &'static str) -> Result<Session, Error> {
    Session::builder()
        .and_then(|mut builder| builder.commit_from_file(path))
        .map_err(|source| Error::Load { role, source })
}

struct TensorSignature<'a> {
    name: &'a str,
    shape: &'a [i64],
}

impl<'a> TensorSignature<'a> {
    fn new(name: &'a str, shape: &'a [i64]) -> Self {
        Self { name, shape }
    }
}

fn validate_model(
    session: &Session,
    role: &'static str,
    input: TensorSignature<'_>,
    output: TensorSignature<'_>,
) -> Result<(), Error> {
    validate_outlets(session.inputs(), role, "input", input)?;
    validate_outlets(session.outputs(), role, "output", output)
}

fn validate_outlets(
    outlets: &[Outlet],
    role: &'static str,
    direction: &'static str,
    expected: TensorSignature<'_>,
) -> Result<(), Error> {
    let [outlet] = outlets else {
        return Err(Error::Incompatible {
            role,
            detail: format!("expected one {direction}, found {}", outlets.len()),
        });
    };
    if outlet.name() != expected.name {
        return Err(Error::Incompatible {
            role,
            detail: format!(
                "expected {direction} {}, found {}",
                expected.name,
                outlet.name()
            ),
        });
    }
    match outlet.dtype() {
        ValueType::Tensor {
            ty: TensorElementType::Float32,
            shape,
            ..
        } if &shape[..] == expected.shape => Ok(()),
        actual => Err(Error::Incompatible {
            role,
            detail: format!(
                "expected {direction} {} to be a float tensor shaped {:?}, found {actual}",
                expected.name, expected.shape
            ),
        }),
    }
}

fn output(
    outputs: &mut SessionOutputs<'_>,
    role: &'static str,
    name: &str,
    expected_shape: Option<&[i64]>,
) -> Result<Vec<f32>, Error> {
    let value = outputs.remove(name).ok_or_else(|| Error::Incompatible {
        role,
        detail: format!("returned no {name} output"),
    })?;
    let (shape, values) = value
        .try_extract_tensor::<f32>()
        .map_err(Error::Inference)?;
    if expected_shape.is_some_and(|expected| &shape[..] != expected) {
        return Err(Error::Incompatible {
            role,
            detail: format!("returned tensor shaped {shape}, expected {expected_shape:?}"),
        });
    }
    Ok(values.to_vec())
}

fn tensor(shape: &[i64], values: Vec<f32>) -> Result<Tensor<f32>, Error> {
    Tensor::from_array((shape, values)).map_err(Error::Inference)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int16_audio_passes_through_and_unit_audio_is_normalized() {
        assert_eq!(AudioScale::Int16.apply(16_384), 16_384.0);
        assert_eq!(AudioScale::Unit.apply(16_384), 0.5);
    }
}

pub struct Config {
    classifier: PathBuf,
    threshold: f32,
}

impl Config {
    pub fn new(classifier: impl Into<PathBuf>, threshold: f32) -> Result<Self, ConfigError> {
        if !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
            return Err(ConfigError::InvalidThreshold(threshold));
        }
        Ok(Self {
            classifier: classifier.into(),
            threshold,
        })
    }

    pub fn classifier(&self) -> &Path {
        &self.classifier
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("wake threshold must be finite, greater than zero, and at most one; got {0}")]
    InvalidThreshold(f32),
}
