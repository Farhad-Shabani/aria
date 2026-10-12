//! Greedy decoding of one recording, thirty seconds at a time.

use candle_core::{Device, IndexOp, Tensor};
use candle_transformers::models::whisper::{self as whisper, Config, model::Whisper};
use tokenizers::Tokenizer;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{Language, TranscriptionError, check_cancelled, english_only};

use super::{BACKEND, EngineError, inference_failed};

/// How many tokens one window may produce. Whisper's own text context is 448
/// including the prompt; a greedy decode that has not stopped by here is
/// looping.
const MAX_WINDOW_TOKENS: usize = 224;

const ENGLISH_TOKEN: &str = "<|en|>";

/// An English-only checkpoint stops one token short of the multilingual
/// vocabulary. Both vocabularies carry the language tokens, so the size is
/// what says which prompt the model was trained on. Prepending a language and
/// a task token to a model that never saw them wrecks the transcript.
const ENGLISH_ONLY_VOCABULARY: usize = 51_864;

/// The tokens that precede a transcript, and the one that ends it.
pub(super) struct Prompt {
    leading: Vec<u32>,
    end_of_text: u32,
    /// Tokens the checkpoint marks as never valid output.
    suppressed: Vec<u32>,
}

impl Prompt {
    pub(super) fn new(
        tokenizer: &Tokenizer,
        config: &Config,
        language: &Language,
    ) -> Result<Self, TranscriptionError> {
        let control = |name: &str| {
            tokenizer
                .token_to_id(name)
                .ok_or_else(|| missing_control_token(name))
        };

        let mut leading = vec![control(whisper::SOT_TOKEN)?];
        if config.vocab_size > ENGLISH_ONLY_VOCABULARY {
            leading.push(language_token(tokenizer, language)?);
            leading.push(control(whisper::TRANSCRIBE_TOKEN)?);
        } else {
            english_only(BACKEND, language)?;
        }
        leading.push(control(whisper::NO_TIMESTAMPS_TOKEN)?);

        Ok(Self {
            leading,
            end_of_text: control(whisper::EOT_TOKEN)?,
            suppressed: config.suppress_tokens.clone(),
        })
    }
}

/// An undeclared language is taken as English; detection is a decode of its
/// own. A language the vocabulary has no token for is one this model does not
/// speak.
fn language_token(tokenizer: &Tokenizer, language: &Language) -> Result<u32, TranscriptionError> {
    let Language::Specific(code) = language else {
        return tokenizer
            .token_to_id(ENGLISH_TOKEN)
            .ok_or_else(|| missing_control_token(ENGLISH_TOKEN));
    };
    let primary = code.as_str().split('-').next().unwrap_or_default();
    tokenizer
        .token_to_id(&format!("<|{primary}|>"))
        .ok_or_else(|| TranscriptionError::UnsupportedLanguage {
            backend: BACKEND,
            language: primary.to_owned(),
        })
}

fn missing_control_token(name: &str) -> TranscriptionError {
    inference_failed(EngineError(format!("the vocabulary has no {name} token")))
}

/// Transcribes every window of `mel` that holds audio, and joins what they
/// heard.
///
/// `spoken` counts the frames the recording itself produced. The spectrogram
/// runs past them — Whisper reads fixed windows, so the last one is padded to
/// length — and a window starting beyond the audio would transcribe silence.
pub(super) fn all_windows(
    model: &mut Whisper,
    tokenizer: &Tokenizer,
    prompt: &Prompt,
    mel: &[f32],
    spoken: usize,
    cancel: &CancelToken,
) -> Result<String, TranscriptionError> {
    let bands = model.config.num_mel_bins;
    let mut text = String::new();

    for start in (0..spoken).step_by(whisper::N_FRAMES) {
        check_cancelled(cancel)?;
        let heard = one_window(model, tokenizer, prompt, mel, bands, start)?;
        let heard = heard.trim();
        if heard.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(heard);
    }
    Ok(text)
}

fn one_window(
    model: &mut Whisper,
    tokenizer: &Tokenizer,
    prompt: &Prompt,
    mel: &[f32],
    bands: usize,
    start: usize,
) -> Result<String, TranscriptionError> {
    model.reset_kv_cache();
    let window = window_tensor(mel, bands, start)?;
    let features = model
        .encoder
        .forward(&window, true)
        .map_err(inference_failed)?;

    let mut tokens = prompt.leading.clone();
    for step in 0..MAX_WINDOW_TOKENS {
        let input = Tensor::new(tokens.as_slice(), &Device::Cpu)
            .and_then(|tokens| tokens.unsqueeze(0))
            .map_err(inference_failed)?;
        let mut logits = model
            .decoder
            .forward(&input, &features, step == 0)
            .and_then(|decoded| {
                let last = decoded.dim(1)? - 1;
                model.decoder.final_linear(&decoded.i((.., last..))?)
            })
            .and_then(|logits| logits.i(0)?.i(0)?.to_vec1::<f32>())
            .map_err(inference_failed)?;
        for &suppressed in &prompt.suppressed {
            if let Some(logit) = logits.get_mut(suppressed as usize) {
                *logit = f32::NEG_INFINITY;
            }
        }
        let next = most_likely(&logits).ok_or_else(|| {
            missing_control_token("any token the decoder could pick from an empty vocabulary")
        })?;
        if next == prompt.end_of_text {
            break;
        }
        tokens.push(next);
    }

    tokenizer
        .decode(&tokens, true)
        .map_err(|error| inference_failed(EngineError(error.to_string())))
}

/// One 30-second window of mel frames, zero-padded when the recording ends
/// mid-window. Whisper only reads inputs of that one length.
fn window_tensor(mel: &[f32], bands: usize, start: usize) -> Result<Tensor, TranscriptionError> {
    let frames = mel.len() / bands;
    let mut window = vec![0f32; bands * whisper::N_FRAMES];
    let taken = whisper::N_FRAMES.min(frames - start);
    for band in 0..bands {
        let source = band * frames + start;
        let target = band * whisper::N_FRAMES;
        window[target..target + taken].copy_from_slice(&mel[source..source + taken]);
    }
    Tensor::from_vec(window, (1, bands, whisper::N_FRAMES), &Device::Cpu).map_err(inference_failed)
}

fn most_likely(logits: &[f32]) -> Option<u32> {
    logits
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(token, _)| token as u32)
}
