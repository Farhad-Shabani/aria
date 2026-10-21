//! In-process speech synthesis through the Kokoro ONNX model.
//!
//! Text becomes phonemes through misaki's G2P, which spells out words its
//! lexicon lacks. Phonemes become token ids, and the model turns tokens
//! plus a voice's style vector into 24 kHz audio. Each sentence-sized chunk
//! is written to the caller as soon as it is synthesized, so playback starts
//! before the utterance is finished.
//!
//! ```no_run
//! use aria_tts::{kokoro::Kokoro, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let speech = Synthesis::new(Kokoro::load("model", "voices")?);
//!
//! // A session yields PCM while later chunks are still being synthesized.
//! let mut session = speech.start(&SpeechText::new("Ready when you are.")?)?;
//! let mut pcm = [0.0; 1024];
//! while session.read(&mut pcm)? != 0 {}
//!
//! // Speaking aloud needs `aria-kit` with its `playback` feature.
//! let cancel = CancelToken::new();
//! speech.speak_with(
//!     &SpeechText::new("A longer announcement.")?,
//!     Some(&Voice::new("af_bella")?),
//!     SpeakingRate::standard(),
//!     &cancel,
//! )?;
//! # Ok(())
//! # }
//! ```

mod vocabulary;
mod voices;

use std::path::PathBuf;
use std::sync::Mutex;

use misaki_rs::{G2P, Language};
use ort::session::Session as OrtSession;
use ort::value::Tensor;

use aria_kit::backend::{BackendError, ModelDownload, Unpack};
use aria_kit::cancel::CancelToken;
use aria_kit::synthesis::{
    Engine, SpeakingRate, SpeechText, SpeechWriter, SynthesisError, Synthesizer, Voice, chunks,
    validate_model_file,
};

use voices::VoiceStyles;

/// Names this engine under `tts.backend` and in its own errors.
pub const NAME: &str = "kokoro";

/// Kokoro v1.0: the graph, then the voice styles it reads.
pub const MODEL: &[ModelDownload] = &[
    ModelDownload {
        file_name: "kokoro.onnx",
        url: "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/kokoro-v1.0.onnx",
        bytes: 325_532_387,
        unpack: Unpack::Plain,
    },
    ModelDownload {
        file_name: "voices.bin",
        url: "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/voices-v1.0.bin",
        bytes: 28_214_398,
        unpack: Unpack::Plain,
    },
];

/// The rate the model was trained to write.
const SAMPLE_RATE: u32 = 24_000;

/// The model's token context, and the number of style rows each voice holds.
const MAX_TOKENS: usize = 510;

/// The width of one style vector.
const STYLE_WIDTH: usize = 256;

const TOKENS_INPUT: &str = "tokens";
const STYLE_INPUT: &str = "style";
const SPEED_INPUT: &str = "speed";
const AUDIO_OUTPUT: &str = "audio";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub model: PathBuf,
    pub voices: PathBuf,
}

pub struct Kokoro {
    /// The session mutates as it runs and misaki tags parts of speech
    /// statefully, so they are shared one chunk at a time.
    synth: Mutex<Synth>,
    voices: VoiceStyles,
}

struct Synth {
    session: OrtSession,
    g2p: G2P,
}

impl Kokoro {
    pub fn load(
        model: impl Into<PathBuf>,
        voices: impl Into<PathBuf>,
    ) -> Result<Self, SynthesisError> {
        Self::load_from(Config {
            model: model.into(),
            voices: voices.into(),
        })
    }
}

impl Engine for Kokoro {
    const NAME: &'static str = NAME;
    const MODEL: &'static [ModelDownload] = MODEL;

    type Config = Config;

    fn check_from(config: &Self::Config) -> Result<PathBuf, SynthesisError> {
        validate_model_file(config.voices.clone(), NAME)?;
        validate_model_file(config.model.clone(), NAME)
    }

    fn load_from(config: Self::Config) -> Result<Self, SynthesisError> {
        let model = Self::check_from(&config)?;
        let voices = VoiceStyles::read(&config.voices).map_err(|source| SynthesisError::Load {
            backend: NAME,
            model: config.voices.display().to_string(),
            source: BackendError::engine(source),
        })?;
        let session = OrtSession::builder()
            .and_then(|mut builder| builder.commit_from_file(&model))
            .map_err(|source| SynthesisError::Load {
                backend: NAME,
                model: model.display().to_string(),
                source: BackendError::engine(source),
            })?;
        // US English matches the voices Kokoro ships.
        let g2p = G2P::new(Language::EnglishUS);
        Ok(Self {
            synth: Mutex::new(Synth { session, g2p }),
            voices,
        })
    }
}

impl Synthesizer for Kokoro {
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn synthesize_with(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        mut out: SpeechWriter,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError> {
        let voice_name = voice.map(Voice::as_str);
        // Checked before the first chunk, so an unknown voice costs no
        // inference.
        if let Some(name) = voice_name
            && !self.voices.names().any(|known| known == name)
        {
            let unsupported = || SynthesisError::UnsupportedVoice {
                backend: NAME,
                voice: name.to_owned(),
            };
            out.fail(unsupported());
            return Err(unsupported());
        }

        for piece in chunks(text.as_str()) {
            if cancel.is_cancelled() {
                out.fail(SynthesisError::Cancelled);
                return Err(SynthesisError::Cancelled);
            }
            // Held for one chunk's inference only: the write below blocks
            // until a slow listener catches up, and other utterances wait on
            // this lock.
            let synthesized = self
                .synth
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .synthesize(&piece, voice_name, rate, &self.voices);
            let samples = match synthesized {
                Ok(samples) => samples,
                Err(error) => {
                    // The listener learns the cause rather than a bare
                    // vanished source. A boxed engine error cannot be cloned,
                    // so it hears the same words in the same category.
                    out.fail(unusable(&error.to_string()));
                    return Err(error);
                }
            };
            if out.write(&samples).is_err() {
                // The listener is gone; stopping is what it asked for.
                return Err(SynthesisError::Cancelled);
            }
        }
        out.finish();
        Ok(())
    }
}

impl Synth {
    /// The audio for one chunk of text, in as many model passes as its
    /// phonemes need.
    fn synthesize(
        &mut self,
        text: &str,
        voice: Option<&str>,
        rate: SpeakingRate,
        voices: &VoiceStyles,
    ) -> Result<Vec<f32>, SynthesisError> {
        // The phonemizer names the word it could not read, and the word is
        // the user's speech. Only the category crosses this boundary.
        let (phonemes, _tokens) = self
            .g2p
            .g2p(text)
            .map_err(|_word_bearing| unusable("the text could not be turned into phonemes"))?;
        let mut audio = Vec::new();
        for tokens in passes(&phonemes) {
            audio.extend(self.run(tokens, voice, rate, voices)?);
        }
        Ok(audio)
    }

    /// One model pass over tokens that fit its context.
    fn run(
        &mut self,
        mut tokens: Vec<i64>,
        voice: Option<&str>,
        rate: SpeakingRate,
        voices: &VoiceStyles,
    ) -> Result<Vec<f32>, SynthesisError> {
        let style = voices
            .style(voice, tokens.len())
            .expect("the voice name was checked before synthesis began")
            .to_vec();

        // The model expects one zero token on each side, as the kokoro-onnx
        // tokenizer adds.
        let mut padded = Vec::with_capacity(tokens.len() + 2);
        padded.push(0);
        padded.append(&mut tokens);
        padded.push(0);

        let token_count = padded.len() as i64;
        let inputs = ort::inputs![
            TOKENS_INPUT => tensor(vec![1, token_count], padded)?,
            STYLE_INPUT => tensor(vec![1, STYLE_WIDTH as i64], style)?,
            SPEED_INPUT => tensor(vec![1], vec![rate.get()])?,
        ];
        let outputs = self.session.run(inputs).map_err(inference)?;
        let audio = outputs
            .get(AUDIO_OUTPUT)
            .ok_or_else(|| unusable("the model returned no audio"))?;
        let (_shape, samples) = audio.try_extract_tensor::<f32>().map_err(inference)?;
        Ok(samples.to_vec())
    }
}

/// Token ids for `phonemes`, cut at word gaps into passes that fit the
/// model's context. Spelled-out words can make one chunk outgrow one pass.
fn passes(phonemes: &str) -> Vec<Vec<i64>> {
    let limit = MAX_TOKENS - 2;
    let mut passes = Vec::new();
    let mut current = Vec::new();
    for word in phonemes.split_inclusive(' ') {
        let word = vocabulary::tokenize(word);
        if !current.is_empty() && current.len() + word.len() > limit {
            passes.push(std::mem::take(&mut current));
        }
        current.extend(word);
    }
    passes.push(current);
    // A single word past the limit has no gap to cut at.
    passes
        .iter()
        .flat_map(|pass| pass.chunks(limit))
        .map(<[i64]>::to_vec)
        .collect()
}

fn tensor<T: ort::value::PrimitiveTensorElementType + std::fmt::Debug + Clone + 'static>(
    shape: Vec<i64>,
    data: Vec<T>,
) -> Result<Tensor<T>, SynthesisError> {
    Tensor::from_array((shape, data)).map_err(inference)
}

fn inference(source: ort::Error) -> SynthesisError {
    SynthesisError::Inference {
        backend: NAME,
        source: BackendError::engine(source),
    }
}

fn unusable(detail: &str) -> SynthesisError {
    SynthesisError::Inference {
        backend: NAME,
        source: BackendError::Unusable(detail.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use misaki_rs::{G2P, Language};

    use super::{MAX_TOKENS, passes, vocabulary};

    #[test]
    fn long_phonemes_split_at_words_without_losing_tokens() {
        let phonemes = "hˈi ".repeat(200);
        let passes = passes(&phonemes);
        assert!(passes.len() > 1, "800 tokens should not fit one pass");
        assert!(passes.iter().all(|pass| pass.len() <= MAX_TOKENS - 2));
        assert_eq!(passes.concat(), vocabulary::tokenize(&phonemes));
    }

    #[test]
    fn words_outside_the_lexicon_are_spelled_out() {
        let g2p = G2P::new(Language::EnglishUS);
        let phonemize = |text: &str| g2p.g2p(text).expect("phonemized").0;
        let spelled: Vec<String> = "zorbquix"
            .chars()
            .map(|c| phonemize(&c.to_string()))
            .collect();
        let sounds = |phonemes: String| {
            phonemes
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(sounds(phonemize("zorbquix")), sounds(spelled.join(" ")));
    }
}
