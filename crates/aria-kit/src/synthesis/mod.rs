//! The text-to-speech contract engine crates implement.

pub mod chunk;
// Playback needs an output device, which only a native target has.
#[cfg(all(feature = "playback", not(target_family = "wasm")))]
mod playback;
#[cfg(not(target_family = "wasm"))]
mod session;
#[cfg(all(feature = "playback", not(target_family = "wasm")))]
mod speak;
mod stream;

use std::path::PathBuf;

use crate::backend::{BackendError, ModelError};
use crate::cancel::CancelToken;
pub use chunk::chunks;
#[cfg(all(feature = "playback", not(target_family = "wasm")))]
pub use playback::SpeakerOutput;
#[cfg(not(target_family = "wasm"))]
pub use session::{Session, Synthesis};
pub use stream::{SpeechStream, SpeechWriter, StreamClosed, TryRead, stream};

/// One import for code that speaks through an engine. Chunking, the raw
/// stream pair, and model validation stay outside it. [`Synthesis`] and
/// [`Session`] exist only off wasm.
pub mod prelude {
    #[cfg(not(target_family = "wasm"))]
    pub use super::{Session, Synthesis};
    pub use super::{SpeakingRate, SpeechText, SynthesisError, Voice};
}

const MAX_SPEECH_TEXT_BYTES: usize = 8 * 1024;
const MAX_VOICE_BYTES: usize = 64;

const MIN_SPEAKING_RATE: f32 = 0.25;
const MAX_SPEAKING_RATE: f32 = 4.0;

/// The contract every text-to-speech engine implements.
///
/// An implementation must:
/// - write PCM into `out` as it is produced, so playback starts before the
///   whole utterance is synthesized;
/// - poll `cancel` at its natural pause points and answer
///   [`SynthesisError::Cancelled`];
/// - treat a closed writer as the listener being gone and stop, answering
///   [`SynthesisError::Cancelled`];
/// - close the writer on every exit — [`SpeechWriter::finish`] on success,
///   [`SpeechWriter::fail`] with the cause on failure;
/// - wrap its own failures in [`BackendError::engine`] under
///   [`SynthesisError::Load`] or [`SynthesisError::Inference`];
/// - refuse a voice it does not have as [`SynthesisError::UnsupportedVoice`].
pub trait Synthesizer: Send + Sync + 'static {
    /// The rate of the PCM this engine writes, fixed by its model.
    fn sample_rate(&self) -> u32;

    /// Synthesizes speech into `out`, chunk by chunk. `voice` of `None` means
    /// the engine's configured default.
    fn synthesize_with(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        out: SpeechWriter,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError>;

    /// Whether this engine delivers sound itself rather than writing PCM: a
    /// platform voice program synthesizes and plays in one stroke.
    /// `Synthesis::speak` skips its own playback for such an engine.
    fn plays_audio_itself(&self) -> bool {
        false
    }
}

/// How an engine is constructed from its own configuration. `NAME` names the
/// engine in logs and errors; `Config` carries whatever it needs to load.
pub trait Engine: Synthesizer + Sized {
    const NAME: &'static str;

    type Config;

    /// Resolves what a load would read, without reading it or starting a
    /// child.
    fn check_from(config: &Self::Config) -> Result<PathBuf, SynthesisError>;

    fn load_from(config: Self::Config) -> Result<Self, SynthesisError>;
}

/// A shared engine synthesizes like the engine itself.
impl<T: Synthesizer + ?Sized> Synthesizer for std::sync::Arc<T> {
    fn sample_rate(&self) -> u32 {
        (**self).sample_rate()
    }

    fn synthesize_with(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        out: SpeechWriter,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError> {
        (**self).synthesize_with(text, voice, rate, out, cancel)
    }
}

impl<T: Synthesizer + ?Sized> Synthesizer for Box<T> {
    fn sample_rate(&self) -> u32 {
        (**self).sample_rate()
    }

    fn synthesize_with(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        out: SpeechWriter,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError> {
        (**self).synthesize_with(text, voice, rate, out, cancel)
    }
}

/// [`crate::backend::validate_model_file`], failing as a [`SynthesisError`].
#[cfg(not(target_family = "wasm"))]
pub fn validate_model_file(
    path: PathBuf,
    backend: &'static str,
) -> Result<PathBuf, SynthesisError> {
    crate::backend::validate_model_file(path)
        .map_err(|source| SynthesisError::Model { backend, source })
}

/// Text to speak. Personal content: it must never land in logs, filenames, or
/// command lines.
#[derive(Clone, PartialEq, Eq)]
pub struct SpeechText(String);

impl std::fmt::Debug for SpeechText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "SpeechText({} bytes)", self.0.len())
    }
}

impl SpeechText {
    pub fn new(text: impl Into<String>) -> Result<Self, SpeechTextError> {
        let text = text.into();
        let text = text.trim();
        if text.is_empty() {
            return Err(SpeechTextError::Empty);
        }
        if text.len() > MAX_SPEECH_TEXT_BYTES {
            return Err(SpeechTextError::TooLong {
                actual_bytes: text.len(),
                maximum_bytes: MAX_SPEECH_TEXT_BYTES,
            });
        }
        Ok(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpeechTextError {
    #[error("speech text must not be empty")]
    Empty,

    #[error("speech text contains {actual_bytes} bytes; maximum is {maximum_bytes} bytes")]
    TooLong {
        actual_bytes: usize,
        maximum_bytes: usize,
    },
}

/// An engine voice name. Which names exist is each engine's own affair; this
/// only rejects what no engine could mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voice(String);

impl Voice {
    pub fn new(name: impl Into<String>) -> Result<Self, VoiceError> {
        let name = name.into();
        let name = name.trim();
        if name.is_empty() {
            return Err(VoiceError::Empty);
        }
        if name.len() > MAX_VOICE_BYTES {
            return Err(VoiceError::TooLong {
                actual_bytes: name.len(),
                maximum_bytes: MAX_VOICE_BYTES,
            });
        }
        if name.chars().any(char::is_control) {
            return Err(VoiceError::Malformed(name.to_owned()));
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("voice name must not be empty")]
    Empty,

    #[error("voice name contains {actual_bytes} bytes; maximum is {maximum_bytes} bytes")]
    TooLong {
        actual_bytes: usize,
        maximum_bytes: usize,
    },

    #[error("voice name contains control characters: {0:?}")]
    Malformed(String),
}

/// A speaking-rate multiplier: 1.0 is the engine's natural pace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeakingRate(f32);

impl SpeakingRate {
    pub fn new(rate: f32) -> Result<Self, SpeakingRateError> {
        if !rate.is_finite() || !(MIN_SPEAKING_RATE..=MAX_SPEAKING_RATE).contains(&rate) {
            return Err(SpeakingRateError::OutOfRange {
                actual: rate,
                minimum: MIN_SPEAKING_RATE,
                maximum: MAX_SPEAKING_RATE,
            });
        }
        Ok(Self(rate))
    }

    pub fn get(self) -> f32 {
        self.0
    }

    pub const fn standard() -> Self {
        Self(1.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpeakingRateError {
    #[error("speaking rate must be between {minimum} and {maximum}; got {actual}")]
    OutOfRange {
        actual: f32,
        minimum: f32,
        maximum: f32,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum SynthesisError {
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

    #[error("the {backend} speech program {program} cannot be used")]
    Program {
        backend: &'static str,
        program: String,
        #[source]
        source: BackendError,
    },

    #[error("{backend} has no voice named {voice:?}")]
    UnsupportedVoice {
        backend: &'static str,
        voice: String,
    },

    #[error("the {backend} backend is not available on {}", std::env::consts::OS)]
    UnsupportedBackend { backend: &'static str },

    #[error("the {backend} synthesis worker stopped unexpectedly")]
    WorkerPanicked { backend: &'static str },

    #[error("the {backend} backend plays audio directly and cannot provide PCM")]
    PcmUnavailable { backend: &'static str },

    #[error("the PCM read buffer must not be empty")]
    EmptyReadBuffer,

    #[error("synthesis was cancelled")]
    Cancelled,

    #[error("the synthesizer stopped before the speech ended")]
    SourceGone,

    #[error("the {backend} synthesizer ran past its time ceiling")]
    TimedOut { backend: &'static str },

    #[error("the speech already failed and its cause was reported")]
    AlreadyFailed,

    #[cfg(not(target_family = "wasm"))]
    #[error("speech playback failed")]
    Playback {
        #[source]
        source: PlaybackError,
    },
}

#[cfg(not(target_family = "wasm"))]
#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("no audio output device is available")]
    NoOutputDevice,

    #[error("the output device offers no usable configuration")]
    NoUsableConfig(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("the output device wants {0} samples, which this build cannot feed")]
    UnsupportedFormat(String),

    #[error("the output stream could not be started")]
    Stream(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("the output device stopped mid-utterance")]
    DeviceLost,

    #[error("playback was cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_text_is_trimmed_and_bounded() {
        let text = SpeechText::new("  hello  ").expect("plain text is accepted");
        assert_eq!(text.as_str(), "hello");

        assert!(matches!(
            SpeechText::new("   "),
            Err(SpeechTextError::Empty)
        ));
        assert!(matches!(
            SpeechText::new("x".repeat(MAX_SPEECH_TEXT_BYTES + 1)),
            Err(SpeechTextError::TooLong { .. })
        ));
    }

    #[test]
    fn voice_names_reject_what_no_engine_could_mean() {
        assert_eq!(
            Voice::new("af_heart")
                .expect("a plain name is accepted")
                .as_str(),
            "af_heart"
        );

        assert!(matches!(Voice::new(""), Err(VoiceError::Empty)));
        assert!(matches!(
            Voice::new("x".repeat(MAX_VOICE_BYTES + 1)),
            Err(VoiceError::TooLong { .. })
        ));
        assert!(matches!(
            Voice::new("bad\nname"),
            Err(VoiceError::Malformed(_))
        ));
    }

    #[test]
    fn speaking_rate_is_bounded_and_finite() {
        assert_eq!(SpeakingRate::standard().get(), 1.0);
        assert!(SpeakingRate::new(2.0).is_ok());

        assert!(SpeakingRate::new(0.0).is_err());
        assert!(SpeakingRate::new(5.0).is_err());
        assert!(SpeakingRate::new(f32::NAN).is_err());
    }
}
