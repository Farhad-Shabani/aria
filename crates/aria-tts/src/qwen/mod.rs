//! Pure-Rust Qwen3-TTS synthesis.
//!
//! The talker, code predictor, and speech-codec decoder run in-process on
//! Candle, so an utterance never leaves the process and needs no Python or
//! MLX. Each sentence-sized chunk decodes to PCM as soon as it is generated,
//! so playback starts before the utterance is finished.
//!
//! ```no_run
//! use aria_tts::{qwen::Qwen, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let speech = Synthesis::new(Qwen::load("model", "speech-tokenizer")?);
//!
//! let mut session = speech.start(&SpeechText::new("Ready when you are.")?)?;
//! let mut pcm = [0.0; 1024];
//! while session.read(&mut pcm)? != 0 {}
//! # Ok(())
//! # }
//! ```

pub mod code_predictor;
pub mod codec;
pub mod config;
pub mod engine;
pub mod generate;
pub mod mel;
pub mod model_files;
mod nn;
pub mod prompt;
pub mod sampling;
pub mod speaker;
pub mod talker;
pub mod text;
pub mod transformer;

pub use code_predictor::CodePredictor;
pub use codec::{CodecError, SpeechCodecDecoder};
pub use engine::Qwen;
pub use speaker::SpeakerEncoder;
pub use talker::Talker;

pub use config::{
    CodePredictorConfig, CodecDecoderConfig, ConfigError, ModelConfig, ModelType, RopeScaling,
    SpeakerDialect, SpeechTokenizerConfig, TalkerConfig,
};
pub use text::{ChatTokenizer, TextError};

pub const NAME: &str = "qwen";
