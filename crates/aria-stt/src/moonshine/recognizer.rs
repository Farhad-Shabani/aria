//! The two ONNX graphs behind a moonshine transcript.
//!
//! The encoder reads a window of audio once. The decoder then writes one token
//! at a time, and is handed back the keys and values it has already computed so
//! each step costs one position rather than the whole sequence again.

use std::path::Path;

use ort::session::{Session, SessionInputValue, SessionOutputs};
use ort::value::{DynValue, Tensor, TensorElementType, ValueType};

use aria_kit::backend::BackendError;
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{TranscriptionError, check_cancelled};

use super::BACKEND;

pub(super) const ENCODER_FILE: &str = "encoder_model.onnx";
pub(super) const DECODER_FILE: &str = "decoder_model_merged.onnx";

const AUDIO_INPUT: &str = "input_values";
const AUDIO_STATE: &str = "last_hidden_state";
const TOKEN_INPUT: &str = "input_ids";
const AUDIO_STATE_INPUT: &str = "encoder_hidden_states";
const REUSE_CACHE_INPUT: &str = "use_cache_branch";
const SCORES_OUTPUT: &str = "logits";

/// What the decoder calls a cache entry it is handed, and the same entry when
/// it hands it back.
const CACHE_INPUT_PREFIX: &str = "past_key_values.";
const CACHE_OUTPUT_PREFIX: &str = "present.";

/// Marks the entries that attend to the audio rather than to the tokens
/// decoded so far.
const CROSS_ATTENTION: &str = ".encoder.";

const START_TOKEN: u32 = 1;
const END_TOKEN: u32 = 2;

/// Positions the decoder was trained to write. Read speech reaches it at about
/// fifty seconds, and what the model writes after it repeats rather than
/// transcribes, so windows are cut to stay well inside it.
const MAX_TOKENS: usize = 194;

/// Tokens a second of speech is worth. Read speech measures near four; the
/// headroom is for speech denser than that.
const TOKENS_PER_SECOND: usize = 8;

/// A window given more positions than its speech needs repeats itself.
fn token_budget(samples: usize) -> usize {
    let rate = aria_kit::audio::CANONICAL_SAMPLE_RATE as usize;
    (samples * TOKENS_PER_SECOND / rate).clamp(1, MAX_TOKENS)
}

/// Samples the encoder needs before it has a frame to hand the decoder. Shorter
/// than this it refuses the shape rather than returning nothing: measured, 55 ms
/// of audio fails and 64 ms decodes.
pub(super) const MIN_SAMPLES: usize = 1024;

pub(super) struct Recognizer {
    encoder: Session,
    decoder: Session,
    cache: CacheLayout,
}

impl Recognizer {
    pub(super) fn load(directory: &Path) -> Result<Self, TranscriptionError> {
        let encoder = open(directory, ENCODER_FILE)?;
        let decoder = open(directory, DECODER_FILE)?;
        declares(&encoder, ENCODER_FILE, &[AUDIO_INPUT], &[AUDIO_STATE])?;
        declares(
            &decoder,
            DECODER_FILE,
            &[TOKEN_INPUT, AUDIO_STATE_INPUT, REUSE_CACHE_INPUT],
            &[SCORES_OUTPUT],
        )?;
        let cache = CacheLayout::read(&decoder)?;
        cache.hands_back(&decoder)?;
        Ok(Self {
            encoder,
            decoder,
            cache,
        })
    }

    /// The tokens spoken in one window of audio.
    pub(super) fn transcribe(
        &mut self,
        samples: &[f32],
        cancel: &CancelToken,
    ) -> Result<Vec<u32>, TranscriptionError> {
        let audio = self.encode(samples)?;
        self.decode(&audio, samples.len(), cancel)
    }

    fn encode(&mut self, samples: &[f32]) -> Result<DynValue, TranscriptionError> {
        let recording = tensor(vec![1, samples.len() as i64], samples.to_vec())?;
        let mut outputs = self
            .encoder
            .run(ort::inputs![AUDIO_INPUT => recording])
            .map_err(inference)?;
        outputs
            .remove(AUDIO_STATE)
            .ok_or_else(|| unusable("the encoder returned no audio state"))
    }

    /// Writes tokens until the model marks the end of the sentence, the caller
    /// cancels, or the model runs out of positions.
    fn decode(
        &mut self,
        audio: &DynValue,
        samples: usize,
        cancel: &CancelToken,
    ) -> Result<Vec<u32>, TranscriptionError> {
        let mut cached = self.cache.empty()?;
        let mut tokens = Vec::new();
        let mut token = START_TOKEN;

        for step in 0..token_budget(samples) {
            check_cancelled(cancel)?;

            let mut inputs = self.cache.hand_in(&cached);
            let written = tensor(vec![1, 1], vec![i64::from(token)])?;
            let reuse_cache = tensor(vec![1], vec![step > 0])?;
            inputs.push((TOKEN_INPUT.into(), written.into()));
            inputs.push((AUDIO_STATE_INPUT.into(), audio.into()));
            inputs.push((REUSE_CACHE_INPUT.into(), reuse_cache.into()));

            let mut outputs = self.decoder.run(inputs).map_err(inference)?;
            token = most_likely(&outputs)?;
            if token == END_TOKEN {
                return Ok(tokens);
            }
            tokens.push(token);
            self.cache.take_back(&mut cached, &mut outputs, step == 0)?;
        }

        // Discard a window that exhausts its token budget: a repetition loop
        // could otherwise carry into the next window.
        tracing::warn!(
            samples,
            "a window filled the decoder without finishing; dropping what it wrote"
        );
        Ok(Vec::new())
    }
}

/// Every cache entry the decoder declares, in the order it declares them.
struct CacheLayout {
    entries: Vec<CacheEntry>,
}

struct CacheEntry {
    handed_in: String,
    handed_back: String,
    /// The shape this entry takes before anything has been decoded.
    empty: Vec<i64>,
    /// Cross-attention is computed once from the audio and then never changes;
    /// self-attention grows by one position every step.
    cross_attention: bool,
}

impl CacheLayout {
    /// Read from the graph rather than written down, so the same code serves
    /// every size of the model. Every input named as a cache entry must be one
    /// this code can fill, or the decode would be handed a partial cache.
    fn read(decoder: &Session) -> Result<Self, TranscriptionError> {
        let named = decoder
            .inputs()
            .iter()
            .filter(|outlet| outlet.name().starts_with(CACHE_INPUT_PREFIX));
        let mut entries = Vec::new();
        for outlet in named {
            let entry = CacheEntry::read(outlet.name(), outlet.dtype()).ok_or_else(|| {
                unusable_model(
                    DECODER_FILE,
                    &format!("{} is not an attention cache of floats", outlet.name()),
                )
            })?;
            entries.push(entry);
        }
        match entries.is_empty() {
            true => Err(unusable_model(DECODER_FILE, "it takes no attention cache")),
            false => Ok(Self { entries }),
        }
    }

    /// Every entry the decoder is handed must come back, or a decode would
    /// stop partway through its first step.
    fn hands_back(&self, decoder: &Session) -> Result<(), TranscriptionError> {
        for entry in &self.entries {
            let returned = decoder
                .outputs()
                .iter()
                .any(|outlet| outlet.name() == entry.handed_back);
            if !returned {
                return Err(unusable_model(
                    DECODER_FILE,
                    &format!("it does not return {}", entry.handed_back),
                ));
            }
        }
        Ok(())
    }

    fn empty(&self) -> Result<Vec<DynValue>, TranscriptionError> {
        self.entries
            .iter()
            .map(|entry| tensor(entry.empty.clone(), Vec::<f32>::new()).map(Tensor::into_dyn))
            .collect()
    }

    fn hand_in<'v>(&self, cached: &'v [DynValue]) -> Vec<(String, SessionInputValue<'v>)> {
        self.entries
            .iter()
            .zip(cached)
            .map(|(entry, value)| (entry.handed_in.clone(), value.into()))
            .collect()
    }

    /// Takes back what the step computed. Cross-attention is taken only on the
    /// first step: afterwards the decoder is handing back what it was handed,
    /// and taking it again would cost a copy per step.
    fn take_back(
        &self,
        cached: &mut [DynValue],
        outputs: &mut SessionOutputs<'_>,
        first_step: bool,
    ) -> Result<(), TranscriptionError> {
        for (entry, value) in self.entries.iter().zip(cached) {
            if entry.cross_attention && !first_step {
                continue;
            }
            *value = outputs
                .remove(&entry.handed_back)
                .ok_or_else(|| unusable("the decoder kept back part of its attention cache"))?;
        }
        Ok(())
    }
}

impl CacheEntry {
    /// The empty cache handed in on the first step is built as `f32`, so an
    /// entry declared as anything else is not an entry this code can fill.
    fn read(name: &str, declared: &ValueType) -> Option<Self> {
        let suffix = name.strip_prefix(CACHE_INPUT_PREFIX)?;
        let ValueType::Tensor {
            ty: TensorElementType::Float32,
            shape,
            ..
        } = declared
        else {
            return None;
        };
        Some(Self {
            handed_in: name.to_owned(),
            handed_back: format!("{CACHE_OUTPUT_PREFIX}{suffix}"),
            empty: empty_shape(shape),
            cross_attention: name.contains(CROSS_ATTENTION),
        })
    }
}

/// A declared shape with nothing decoded yet: one recording, no positions. The
/// graph leaves those two axes open and fixes the rest.
fn empty_shape(declared: &[i64]) -> Vec<i64> {
    declared
        .iter()
        .enumerate()
        .map(|(axis, &dimension)| match (dimension.is_negative(), axis) {
            (false, _) => dimension,
            (true, 0) => 1,
            (true, _) => 0,
        })
        .collect()
}

fn most_likely(outputs: &SessionOutputs<'_>) -> Result<u32, TranscriptionError> {
    let scored = outputs
        .get(SCORES_OUTPUT)
        .ok_or_else(|| unusable("the decoder returned no scores"))?;
    let (shape, scores) = scored.try_extract_tensor::<f32>().map_err(inference)?;
    best_token(shape, scores).ok_or_else(|| unusable("the decoder scored no tokens"))
}

/// Selects the highest-scoring token at the last position; the lowest id wins ties.
fn best_token(shape: &[i64], scores: &[f32]) -> Option<u32> {
    let vocabulary = usize::try_from(*shape.last()?).ok().filter(|it| *it > 0)?;
    let last = scores.len().checked_sub(vocabulary)?;
    let best = scores[last..]
        .iter()
        .enumerate()
        .reduce(|best, scored| match scored.1 > best.1 {
            true => scored,
            false => best,
        })?
        .0;
    u32::try_from(best).ok()
}

fn open(directory: &Path, file: &'static str) -> Result<Session, TranscriptionError> {
    let path = directory.join(file);
    Session::builder()
        .and_then(|mut builder| builder.commit_from_file(&path))
        .map_err(|source| TranscriptionError::Load {
            backend: BACKEND,
            model: path.display().to_string(),
            source: BackendError::engine(source),
        })
}

/// Refuses a graph that is not the one this backend drives, at startup rather
/// than on the first press.
fn declares(
    session: &Session,
    file: &'static str,
    inputs: &[&str],
    outputs: &[&str],
) -> Result<(), TranscriptionError> {
    let declared = |outlets: &[ort::value::Outlet], wanted: &str| {
        outlets.iter().any(|outlet| outlet.name() == wanted)
    };
    for wanted in inputs {
        if !declared(session.inputs(), wanted) {
            return Err(unusable_model(file, &format!("it does not take {wanted}")));
        }
    }
    for wanted in outputs {
        if !declared(session.outputs(), wanted) {
            return Err(unusable_model(
                file,
                &format!("it does not return {wanted}"),
            ));
        }
    }
    Ok(())
}

fn tensor<T: ort::value::PrimitiveTensorElementType + std::fmt::Debug + Clone + 'static>(
    shape: Vec<i64>,
    data: Vec<T>,
) -> Result<Tensor<T>, TranscriptionError> {
    Tensor::from_array((shape, data)).map_err(inference)
}

fn inference(source: ort::Error) -> TranscriptionError {
    TranscriptionError::Inference {
        backend: BACKEND,
        source: BackendError::engine(source),
    }
}

fn unusable(detail: &str) -> TranscriptionError {
    TranscriptionError::Inference {
        backend: BACKEND,
        source: BackendError::Unusable(detail.to_owned()),
    }
}

fn unusable_model(file: &str, detail: &str) -> TranscriptionError {
    TranscriptionError::Load {
        backend: BACKEND,
        model: file.to_owned(),
        source: BackendError::Unusable(detail.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_axis_becomes_one_recording_and_no_positions() {
        assert_eq!(empty_shape(&[-1, 8, -1, 52]), [1, 8, 0, 52]);
    }

    #[test]
    fn a_fully_declared_shape_is_left_alone() {
        assert_eq!(empty_shape(&[1, 8, 4, 52]), [1, 8, 4, 52]);
    }

    #[test]
    fn attention_over_the_audio_is_told_from_attention_over_the_tokens() {
        assert!(entry("past_key_values.3.encoder.key").cross_attention);
        assert!(!entry("past_key_values.3.decoder.key").cross_attention);
    }

    #[test]
    fn an_entry_knows_what_the_decoder_hands_back() {
        assert_eq!(
            entry("past_key_values.7.decoder.value").handed_back,
            "present.7.decoder.value"
        );
    }

    #[test]
    fn an_input_that_is_not_a_cache_entry_is_skipped() {
        assert!(CacheEntry::read(TOKEN_INPUT, &shape(&[1, 1])).is_none());
    }

    #[test]
    fn the_highest_scoring_token_of_the_last_position_wins() {
        let scores = [0.1, 0.9, 0.2, /* last position */ 0.4, 0.1, 0.8];

        assert_eq!(best_token(&[1, 2, 3], &scores), Some(2));
    }

    #[test]
    fn the_lowest_id_wins_a_tie() {
        let scores = [0.5, 0.5, 0.5];

        assert_eq!(best_token(&[1, 1, 3], &scores), Some(0));
    }

    #[test]
    fn scores_that_do_not_fill_one_position_are_refused() {
        assert_eq!(best_token(&[1, 1, 4], &[0.1, 0.2]), None);
        assert_eq!(best_token(&[1, 1, 0], &[]), None);
        assert_eq!(best_token(&[], &[0.1]), None);
    }

    fn entry(name: &str) -> CacheEntry {
        CacheEntry::read(name, &shape(&[-1, 8, -1, 52])).expect("the name declares a cache entry")
    }

    fn shape(dimensions: &[i64]) -> ValueType {
        ValueType::Tensor {
            ty: ort::value::TensorElementType::Float32,
            shape: ort::value::Shape::new(dimensions.iter().copied()),
            dimension_symbols: dimensions.iter().map(|_| String::new()).collect(),
        }
    }
}
