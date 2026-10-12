use std::collections::VecDeque;

pub use crate::{Config, ConfigError};

use crate::frontend::{
    AudioScale, Classifier, ClassifierSignature, Embedding, Error, FEATURE_LEN, FEATURE_WIDTH,
    FEATURE_WINDOW_FRAMES, MEL_BINS, MEL_WINDOW_FRAMES, Mel, ModelFiles,
};
use aria_kit::wake_word::{WakeWordDetector, WakeWordMatch};

pub const NAME: &str = "openwakeword";

const SIGNATURE: ClassifierSignature = ClassifierSignature {
    input: "onnx::Flatten_0",
    input_shape: &[1, 16, 96],
    output: "39",
    output_shape: &[1, 1],
};

const SCALE: AudioScale = AudioScale::Int16;

const FRAME_SAMPLES: usize = 1_280;
/// Samples of history the mel stage needs before the current frame.
const MEL_CONTEXT_SAMPLES: usize = 480;

pub struct OpenWakeWord {
    mel: Mel,
    embedding: Embedding,
    classifier: Classifier,
    threshold: f32,
    pending_samples: VecDeque<i16>,
    raw_context: VecDeque<i16>,
    mel_frames: VecDeque<[f32; MEL_BINS]>,
    feature_frames: VecDeque<[f32; FEATURE_WIDTH]>,
    baseline_feature: [f32; FEATURE_WIDTH],
}

impl OpenWakeWord {
    /// Proves the classifier is this backend's head, so `aria doctor` catches a
    /// swapped model before the daemon refuses to start.
    pub fn check_from(config: &Config) -> Result<ModelFiles, Error> {
        let files = ModelFiles::beside(config.classifier())?;
        Classifier::load(&files.classifier, SIGNATURE)?;
        Ok(files)
    }

    pub fn load_from(config: &Config) -> Result<Self, Error> {
        let files = Self::check_from(config)?;
        let mel = Mel::load(&files.melspectrogram, SCALE)?;
        let mut embedding = Embedding::load(&files.embedding)?;
        let classifier = Classifier::load(&files.classifier, SIGNATURE)?;

        let baseline_feature = embedding.feature(&[[1.0; MEL_BINS]; MEL_WINDOW_FRAMES])?;

        let mut detector = Self {
            mel,
            embedding,
            classifier,
            threshold: config.threshold(),
            pending_samples: VecDeque::with_capacity(FRAME_SAMPLES),
            raw_context: VecDeque::with_capacity(FRAME_SAMPLES + MEL_CONTEXT_SAMPLES),
            mel_frames: VecDeque::with_capacity(MEL_WINDOW_FRAMES),
            feature_frames: VecDeque::with_capacity(FEATURE_WINDOW_FRAMES),
            baseline_feature,
        };
        detector.reset();
        Ok(detector)
    }

    fn detect_inner(&mut self, canonical_samples: &[i16]) -> Result<WakeWordMatch, Error> {
        let mut matched = WakeWordMatch::NoMatch;
        for sample in canonical_samples {
            self.pending_samples.push_back(*sample);
            if self.pending_samples.len() != FRAME_SAMPLES {
                continue;
            }

            let frame: Vec<i16> = self.pending_samples.drain(..).collect();
            if self.process_frame(&frame)? >= self.threshold {
                matched = WakeWordMatch::Detected;
            }
        }
        Ok(matched)
    }

    fn process_frame(&mut self, frame: &[i16]) -> Result<f32, Error> {
        self.raw_context.extend(frame.iter().copied());
        while self.raw_context.len() > FRAME_SAMPLES + MEL_CONTEXT_SAMPLES {
            self.raw_context.pop_front();
        }

        let context: Vec<i16> = self.raw_context.iter().copied().collect();
        for frame in self.mel.frames(&context)? {
            self.mel_frames.push_back(frame);
            if self.mel_frames.len() > MEL_WINDOW_FRAMES {
                self.mel_frames.pop_front();
            }
        }

        let feature = self.embedding.feature(self.mel_frames.make_contiguous())?;
        self.feature_frames.push_back(feature);
        if self.feature_frames.len() > FEATURE_WINDOW_FRAMES {
            self.feature_frames.pop_front();
        }

        let window: Vec<f32> = self.feature_frames.iter().flatten().copied().collect();
        let window: [f32; FEATURE_LEN] = window.try_into().map_err(|_| Error::Incompatible {
            role: "embedding",
            detail: "built an incomplete classifier window".to_owned(),
        })?;
        self.classifier.score(&window)
    }
}

impl WakeWordDetector for OpenWakeWord {
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
        self.pending_samples.clear();
        self.raw_context.clear();
        self.mel_frames.clear();
        self.mel_frames.resize(MEL_WINDOW_FRAMES, [1.0; MEL_BINS]);
        self.feature_frames.clear();
        self.feature_frames
            .resize(FEATURE_WINDOW_FRAMES, self.baseline_feature);
    }
}
