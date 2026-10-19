//! Voice-activity inference over canonical audio frames: which stretches
//! carry a human voice rather than music, noise, or silence.

#[cfg(feature = "silero")]
mod silero;

#[cfg(feature = "silero")]
pub use silero::{SileroVad, Speech};

pub const SPEECH_THRESHOLD: f32 = 0.5;

pub trait VoiceActivityDetector {
    /// The exact number of canonical samples `probability` accepts per call.
    const FRAME_SAMPLES: usize;

    /// Clears state carried across recordings; stateless detectors keep the no-op.
    fn reset(&mut self) {}

    fn probability(&mut self, samples: &[i16]) -> Result<f32, VoiceActivityError>;
}

impl<V: VoiceActivityDetector> VoiceActivityDetector for &mut V {
    const FRAME_SAMPLES: usize = V::FRAME_SAMPLES;

    fn reset(&mut self) {
        (**self).reset();
    }

    fn probability(&mut self, samples: &[i16]) -> Result<f32, VoiceActivityError> {
        (**self).probability(samples)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VoiceActivityError {
    #[error("failed to load the bundled voice-activity model")]
    LoadModel(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("voice-activity frame contains {actual_samples} samples; expected {expected_samples}")]
    InvalidFrameLength {
        actual_samples: usize,
        expected_samples: usize,
    },

    #[error("voice-activity inference failed")]
    Inference(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("voice-activity model returned invalid probability {0}")]
    InvalidProbability(f32),
}
