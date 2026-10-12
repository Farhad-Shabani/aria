//! Speech segmentation through the pyannote 3.0 ONNX graph.
//!
//! Ten seconds of 16 kHz audio in, one answer per 270 samples out: which of
//! up to three voices speak in that frame. The graph scores seven mutually
//! exclusive classes — silence, each voice alone, each pair — so the winning
//! class names the speakers and there is nothing to threshold.
//!
//! No output class represents three simultaneous voices.
//!
//! ```no_run
//! use aria_kit::segmentation::{Engine, Segmenter};
//! use aria_segmentation::pyannote::{Config, Pyannote};
//!
//! # fn main() -> Result<(), aria_kit::segmentation::SegmentationError> {
//! let mut engine = Pyannote::load_from(Config {
//!     model: "segmentation-3.0.onnx".into(),
//! })?;
//! let frames = engine.segment(&vec![0.0; 160_000])?;
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;

use aria_kit::backend::BackendError;
use aria_kit::segmentation::{
    Activity, ChunkShape, Engine, SegmentationError, Segmenter, validate_model_file,
};
use ort::session::Session;
use ort::value::Tensor;

pub const NAME: &str = "pyannote";

/// The chunk the graph was trained on: ten seconds at 16 kHz.
const WINDOW_SAMPLES: usize = 160_000;
/// Samples from one output frame to the next.
const FRAME_SHIFT: usize = 270;
/// Samples one output frame answers for. Wider than the shift, so frames
/// overlap and a frame's time is its centre.
const FRAME_SIZE: usize = 991;
/// Voices the graph tells apart inside a single chunk.
const LOCAL_SPEAKERS: usize = 3;
const AUDIO_INPUT: &str = "x";
const SCORES_OUTPUT: &str = "y";

/// Which local voices each of the seven classes names: silence, then each
/// voice alone, then each pair.
const POWERSET: [&[usize]; 7] = [&[], &[0], &[1], &[2], &[0, 1], &[0, 2], &[1, 2]];

pub struct Config {
    pub model: PathBuf,
}

/// A loaded pyannote graph. Holding one is proof the model runs: loading
/// segments a silent chunk before returning.
pub struct Pyannote {
    session: Session,
}

impl Engine for Pyannote {
    const NAME: &'static str = NAME;

    type Config = Config;

    fn check_from(config: &Self::Config) -> Result<PathBuf, SegmentationError> {
        validate_model_file(config.model.clone(), NAME)
    }

    fn load_from(config: Self::Config) -> Result<Self, SegmentationError> {
        let model = Self::check_from(&config)?;
        let session = Session::builder()
            .and_then(|mut builder| builder.commit_from_file(&model))
            .map_err(|source| SegmentationError::Load {
                backend: NAME,
                model: model.display().to_string(),
                source: BackendError::engine(source),
            })?;
        let mut engine = Self { session };
        engine.segment(&vec![0.0; WINDOW_SAMPLES])?;
        Ok(engine)
    }
}

impl Segmenter for Pyannote {
    fn shape(&self) -> ChunkShape {
        ChunkShape {
            samples: WINDOW_SAMPLES,
            frame_shift: FRAME_SHIFT,
            frame_size: FRAME_SIZE,
            speakers: LOCAL_SPEAKERS,
        }
    }

    fn segment(&mut self, chunk: &[f32]) -> Result<Vec<Activity>, SegmentationError> {
        if chunk.len() != WINDOW_SAMPLES {
            return Err(unusable(format!(
                "a chunk of {} samples reached the model, which reads {WINDOW_SAMPLES}",
                chunk.len()
            )));
        }
        let shape = [1_i64, 1, WINDOW_SAMPLES as i64];
        let input = Tensor::from_array((shape, chunk.to_vec())).map_err(inference)?;
        let mut outputs = self
            .session
            .run(ort::inputs![AUDIO_INPUT => input])
            .map_err(inference)?;
        let scores = outputs
            .remove(SCORES_OUTPUT)
            .ok_or_else(|| unusable(format!("the model returned no {SCORES_OUTPUT} output")))?;
        let (shape, values) = scores.try_extract_tensor::<f32>().map_err(inference)?;
        let classes = *shape.last().unwrap_or(&0) as usize;
        if classes != POWERSET.len() {
            return Err(unusable(format!(
                "the model scores {classes} classes, expected {}",
                POWERSET.len()
            )));
        }
        Ok(values.chunks_exact(classes).map(loudest).collect())
    }
}

/// The voices named by a frame's highest-scoring class.
fn loudest(frame: &[f32]) -> Activity {
    let winner = frame
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(class, _)| class);
    POWERSET[winner]
        .iter()
        .fold(Activity::SILENT, |active, &voice| active.speaking(voice))
}

fn inference(source: ort::Error) -> SegmentationError {
    SegmentationError::Inference {
        backend: NAME,
        source: BackendError::engine(source),
    }
}

fn unusable(reason: String) -> SegmentationError {
    SegmentationError::Inference {
        backend: NAME,
        source: BackendError::Unusable(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_classes_run_silence_then_singles_then_pairs() {
        assert_eq!(
            loudest(&[9.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            Activity::SILENT
        );
        assert_eq!(
            loudest(&[0.0, 9.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            Activity::SILENT.speaking(0)
        );
        assert_eq!(
            loudest(&[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 9.0]),
            Activity::SILENT.speaking(1).speaking(2)
        );
    }

    #[test]
    fn every_class_names_at_most_two_voices() {
        for class in POWERSET {
            assert!(class.len() <= 2);
            assert!(class.iter().all(|&voice| voice < LOCAL_SPEAKERS));
        }
    }
}
