//! In-process transcription through the Nemotron streaming ASR ONNX
//! bindings.
//!
//! The encoder streams by design: it takes 560 ms of audio at a time against
//! a cache of what came before, so a step costs the same however long a
//! press runs, and the words it puts out are final. No agreement between
//! decodes is needed, and none runs.
//!
//! ```no_run
//! use aria_stt::{nemotron::Nemotron, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let transcription = Transcription::streaming(Nemotron::load("model")?)
//!     .with(RemoveHesitations)
//!     .with(Vocabulary::new(["aria-stt", "Cargo.toml"])?);
//! let mut session = Session::microphone(&transcription, &Language::Automatic)?;
//!
//! // Words arrive as you speak and are never revised.
//! while let Some(words) = session.next_finalized()? {
//!     print!("{}", words.as_str());
//! }
//!
//! let transcript = session.finish()?;
//! # Ok(())
//! # }
//! ```
//!
//! For audio you already hold, [`crate::Transcription::transcribe_file`] decodes a
//! WAV in one call through the same chunked path.

use std::path::PathBuf;

use parakeet_rs::{ExecutionConfig, NemotronHandle, TokenInfo};

use aria_kit::backend::{BackendError, ModelDownload, Unpack};
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{
    AudioInputKind, ChunkStream, Emitted, Engine, Hypothesis, Language, Stream, StreamingEngine,
    TimedWord, Transcriber, TranscriptionAudio, TranscriptionError, check_cancelled, english_only,
    validate_model_directory, word_timings,
};

/// Names this engine under `transcription.backend` and in its own errors.
pub const NAME: &str = "nemotron";

const BACKEND: &str = NAME;
const ENCODER_FILE: &str = "encoder.onnx";
const DECODER_FILE: &str = "decoder_joint.onnx";
const TOKENIZER_FILE: &str = "tokenizer.model";

/// Streaming English 0.6B. A load does not require `encoder.onnx.data` by
/// name: ONNX Runtime opens it because this export's encoder graph points
/// there.
pub const MODEL: &[ModelDownload] = &[
    ModelDownload {
        file_name: ENCODER_FILE,
        url: "https://huggingface.co/altunenes/parakeet-rs/resolve/4d2a8bc71f5c896ec40faa59732e6716295edaf2/nemotron-speech-streaming-en-0.6b/encoder.onnx",
        bytes: 42_159_995,
        sha256: "5c5110ca2e961c3ff5edc2b0ff49f29888b5213287624f7865c60f7384ac02f0",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: "encoder.onnx.data",
        url: "https://huggingface.co/altunenes/parakeet-rs/resolve/4d2a8bc71f5c896ec40faa59732e6716295edaf2/nemotron-speech-streaming-en-0.6b/encoder.onnx.data",
        bytes: 2_436_567_040,
        sha256: "44f65771e1570546f61106b3d0c604a60b398d061476fda8042bb05432601bd4",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: DECODER_FILE,
        url: "https://huggingface.co/altunenes/parakeet-rs/resolve/4d2a8bc71f5c896ec40faa59732e6716295edaf2/nemotron-speech-streaming-en-0.6b/decoder_joint.onnx",
        bytes: 35_779_240,
        sha256: "8bcfde85fa9039a70caeb90204273f837923d63a706c186bd33e2ada25a91700",
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: TOKENIZER_FILE,
        url: "https://huggingface.co/altunenes/parakeet-rs/resolve/4d2a8bc71f5c896ec40faa59732e6716295edaf2/nemotron-speech-streaming-en-0.6b/tokenizer.model",
        bytes: 251_056,
        sha256: "07d4e5a63840a53ab2d4d106d2874768143fb3fbdd47938b3910d2da05bfb0a9",
        unpack: Unpack::Plain,
    },
];

/// Audio per encoder step: 560 ms. The export fixes it, and a load refuses
/// a model that decodes any other width.
pub const CHUNK_SAMPLES: usize = 8_960;
/// One encoder output frame: 80 ms.
const FRAME_SECONDS: f32 = 0.08;

pub struct Nemotron {
    handle: NemotronHandle,
}

#[derive(Clone)]
pub struct NemotronConfig {
    pub model_directory: PathBuf,
}

impl NemotronConfig {
    pub fn new(model_directory: impl Into<PathBuf>) -> Self {
        Self {
            model_directory: model_directory.into(),
        }
    }
}

impl Nemotron {
    pub fn load(directory: impl Into<PathBuf>) -> Result<Self, TranscriptionError> {
        Self::load_from(NemotronConfig::new(directory), &CancelToken::never())
    }

    /// A decoder of its own over the shared sessions.
    fn stream(&self) -> NemotronStream {
        NemotronStream {
            inner: parakeet_rs::Nemotron::from_shared(&self.handle),
        }
    }
}

impl Engine for Nemotron {
    const NAME: &'static str = NAME;
    const MODEL: &'static [ModelDownload] = MODEL;
    const INPUT: AudioInputKind = AudioInputKind::Samples;

    type Config = NemotronConfig;

    fn check_from(config: &NemotronConfig) -> Result<PathBuf, TranscriptionError> {
        validate_model_directory(
            config.model_directory.clone(),
            BACKEND,
            &[ENCODER_FILE, DECODER_FILE, TOKENIZER_FILE],
        )
    }

    fn load_from(config: NemotronConfig, cancel: &CancelToken) -> Result<Self, TranscriptionError> {
        check_cancelled(cancel)?;
        let directory = Self::check_from(&config)?;
        let load_error = |source: BackendError| TranscriptionError::Load {
            backend: BACKEND,
            model: directory.display().to_string(),
            source,
        };
        let handle = NemotronHandle::from_pretrained(&directory, Some(execution()))
            .map_err(|source| load_error(BackendError::engine(source)))?;
        if handle.chunk_samples() != CHUNK_SAMPLES {
            return Err(load_error(BackendError::Unusable(format!(
                "the export decodes {} samples per chunk; this engine drives {CHUNK_SAMPLES}",
                handle.chunk_samples()
            ))));
        }
        check_cancelled(cancel)?;
        Ok(Self { handle })
    }
}

impl StreamingEngine for Nemotron {
    const STREAM: Stream = Stream::Native;
}

impl Transcriber for Nemotron {
    fn validate_language(
        &self,
        language: &Language,
        _cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        english_only(BACKEND, language)
    }

    /// The whole input through the chunked path, so a bounded decode says
    /// what a live one would have.
    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        _language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        check_cancelled(cancel)?;
        let samples = audio.require_samples(BACKEND)?;
        let mut stream = self.stream();
        let mut tokens = Vec::new();
        for chunk in samples.chunks(CHUNK_SAMPLES) {
            check_cancelled(cancel)?;
            let mut chunk = chunk.to_vec();
            chunk.resize(CHUNK_SAMPLES, 0.0);
            tokens.extend(stream.feed_tokens(&chunk)?);
        }
        for _ in 0..FLUSH_CHUNKS {
            tokens.extend(stream.feed_tokens(&[0.0; CHUNK_SAMPLES])?);
        }
        Ok(hypothesis(pieces(&tokens)))
    }

    fn open_stream(
        &self,
        _language: &Language,
        cancel: &CancelToken,
    ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
        check_cancelled(cancel)?;
        Ok(Some(Box::new(self.stream())))
    }
}

/// Silence fed after the audio, in chunks: the right context that lets the
/// model release the words it holds back.
const FLUSH_CHUNKS: usize = 1;

/// Use available CPU parallelism instead of the bindings' four-thread default.
fn execution() -> ExecutionConfig {
    let threads = std::thread::available_parallelism().map_or(4, usize::from);
    ExecutionConfig::new().with_intra_threads(threads)
}

/// One live decode: the bindings' own stream state over the shared model.
struct NemotronStream {
    inner: parakeet_rs::Nemotron,
}

impl NemotronStream {
    fn feed_tokens(&mut self, chunk: &[f32]) -> Result<Vec<TokenInfo>, TranscriptionError> {
        self.inner
            .transcribe_chunk_with_tokens(chunk)
            .map_err(|source| TranscriptionError::Inference {
                backend: BACKEND,
                source: BackendError::engine(source),
            })
    }
}

impl ChunkStream for NemotronStream {
    fn chunk_samples(&self) -> usize {
        CHUNK_SAMPLES
    }

    fn feed(&mut self, chunk: &[f32]) -> Result<Emitted, TranscriptionError> {
        let tokens = self.feed_tokens(chunk)?;
        Ok(Emitted {
            words: hypothesis(pieces(&tokens)),
            joins_previous: continues_a_word(pieces(&tokens)),
        })
    }
}

/// Each token as its text and the encoder frame that emitted it.
fn pieces(tokens: &[TokenInfo]) -> impl Iterator<Item = (&str, usize)> + Clone {
    tokens
        .iter()
        .map(|token| (token.text.as_str(), token.local_frame))
}

/// Whether the first piece carries on a word rather than opening one: an
/// opening piece starts with the space its sentencepiece marker became.
fn continues_a_word<'a>(mut pieces: impl Iterator<Item = (&'a str, usize)>) -> bool {
    pieces
        .next()
        .is_some_and(|(text, _)| !text.starts_with(char::is_whitespace))
}

/// The pieces' text, with each word timed by the frame that emitted its
/// first piece. A piece opens a word with the space its sentencepiece
/// marker became.
fn hypothesis<'a>(pieces: impl Iterator<Item = (&'a str, usize)> + Clone) -> Hypothesis {
    let text: String = pieces.clone().map(|(text, _)| text).collect();
    let words = word_timings(pieces.map(|(text, frame)| {
        let start = frame as f32 * FRAME_SECONDS;
        let timing = TimedWord {
            start,
            end: start + FRAME_SECONDS,
        };
        (text, timing)
    }));
    Hypothesis::with_words(text.trim(), words)
}

#[cfg(test)]
mod tests {
    use aria_kit::backend::ModelError;

    use super::*;

    #[test]
    fn tokens_become_words_with_frame_timings() {
        let heard = hypothesis([(" He", 1), (" hop", 2), ("ed", 3), (" there", 5)].into_iter());

        assert_eq!(heard.as_str(), "He hoped there");
        let frame = |count: usize| count as f32 * FRAME_SECONDS;
        let starts: Vec<f32> = heard.words().iter().map(|word| word.start).collect();
        assert_eq!(starts, [frame(1), frame(2), frame(5)]);
        let ends: Vec<f32> = heard.words().iter().map(|word| word.end).collect();
        assert_eq!(
            ends,
            [
                frame(1) + FRAME_SECONDS,
                frame(3) + FRAME_SECONDS,
                frame(5) + FRAME_SECONDS
            ]
        );
    }

    #[test]
    fn a_chunk_opening_mid_word_says_so() {
        assert!(continues_a_word([("on", 0), (" for", 1)].into_iter()));
        assert!(!continues_a_word([(" pris", 6)].into_iter()));
        assert!(!continues_a_word(std::iter::empty()));
    }

    #[test]
    fn no_tokens_is_silence() {
        let heard = hypothesis(std::iter::empty());
        assert_eq!(heard.as_str(), "");
        assert!(heard.words().is_empty());
    }

    #[test]
    fn a_cancelled_load_stops_before_model_validation() {
        let cancel = CancelToken::new();
        cancel.cancel();

        let outcome = Nemotron::load_from(NemotronConfig::new("/nonexistent/model"), &cancel);

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
    }

    #[test]
    fn a_missing_file_names_itself() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(directory.path().join("encoder.onnx"), b"x").expect("written");

        let outcome = Nemotron::check_from(&NemotronConfig::new(directory.path()));

        assert!(matches!(
            outcome,
            Err(TranscriptionError::Model {
                source: ModelError::Missing {
                    file: "decoder_joint.onnx",
                    ..
                },
                ..
            })
        ));
    }
}
