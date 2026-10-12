//! CustomVoice generation: assembles the streaming-mode prompt the reference
//! builds, then runs the two-stage frame loop — talker picks the semantic
//! code, code predictor expands it to 15 acoustic codes — until EOS.

use std::collections::HashSet;

use candle_core::{IndexOp, Tensor};

use aria_kit::cancel::CancelToken;

use crate::qwen::prompt::VoicePrompt;

use crate::qwen::code_predictor::CodePredictor;
use crate::qwen::config::ModelConfig;
use crate::qwen::sampling::{Sampler, SamplingError};
use crate::qwen::talker::Talker;

#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    #[error("qwen: unsupported language {language:?}")]
    UnsupportedLanguage { language: String },
    #[error("qwen: generation was cancelled")]
    Cancelled,
    #[error("qwen: generation failed: {0}")]
    Forward(#[from] candle_core::Error),
    #[error(transparent)]
    Sampling(#[from] SamplingError),
    #[error("qwen: generation failed: {0}")]
    Transformer(#[from] crate::qwen::transformer::TransformerError),
}

pub type Result<T> = std::result::Result<T, GenerateError>;

/// Generation knobs; defaults mirror the reference `generate_config`.
#[derive(Debug, Clone)]
pub struct GenerationParams {
    pub seed: u64,
    /// Picks the argmax at every step instead of sampling.
    pub greedy: bool,
    pub temperature: f32,
    pub top_k: usize,
    pub top_p: f32,
    pub repetition_penalty: f32,
    pub max_frames: usize,
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            seed: 42,
            greedy: false,
            temperature: 0.9,
            top_k: 50,
            top_p: 1.0,
            repetition_penalty: 1.05,
            max_frames: 2048,
        }
    }
}

/// Drives the two models and holds sampling state across one utterance.
pub struct CustomVoiceGenerator<'a> {
    talker: &'a mut Talker,
    predictor: &'a mut CodePredictor,
    config: &'a ModelConfig,
    params: GenerationParams,
    talker_sampler: Sampler,
    sub_sampler: Sampler,
    greedy: bool,
}

impl<'a> CustomVoiceGenerator<'a> {
    pub fn new(
        talker: &'a mut Talker,
        predictor: &'a mut CodePredictor,
        config: &'a ModelConfig,
        params: GenerationParams,
    ) -> Self {
        let greedy = params.greedy;
        let talker_sampler = Sampler::new(
            params.temperature,
            params.top_k,
            params.top_p,
            params.repetition_penalty,
            params.seed,
        );
        let sub_sampler = Sampler::new(
            params.temperature,
            params.top_k,
            params.top_p,
            1.0,
            params.seed ^ 0x9E37,
        );
        Self {
            talker,
            predictor,
            config,
            params,
            talker_sampler,
            sub_sampler,
            greedy,
        }
    }

    /// One greedy/sampled pick from a logit row with the reference's
    /// preprocessing (penalty over semantic history + suppressed tail).
    fn pick_semantic(
        &mut self,
        logits: &[f32],
        history: &HashSet<u32>,
        banned: &[u32],
        eos: u32,
        vocab: u32,
    ) -> Result<u32> {
        let mut processed = logits.to_vec();
        let pick = if self.greedy {
            self.talker_sampler.argmax(
                &mut processed,
                history,
                banned,
                Some((vocab - 1024)..vocab),
                &[eos],
            )?
        } else {
            self.talker_sampler.sample(
                &mut processed,
                history,
                banned,
                Some((vocab - 1024)..vocab),
                &[eos],
            )?
        };
        Ok(pick as u32)
    }

    /// Generates `[num_code_groups, frames]` codes for one utterance.
    pub fn generate(&mut self, prompt: VoicePrompt, cancel: &CancelToken) -> Result<Vec<Vec<u32>>> {
        let eos = self.config.talker_config.codec_eos_token_id;
        let vocab = self.config.talker_config.vocab_size;
        let groups = self.config.talker_config.num_code_groups as usize;

        self.talker.reset_cache();
        let mut codes: Vec<Vec<u32>> = Vec::new();
        let mut semantic_history: HashSet<u32> = HashSet::new();
        let mut x = prompt.prefill.clone();
        let mut offset = 0usize;

        for frame in 0..self.params.max_frames {
            if cancel.is_cancelled() {
                return Err(GenerateError::Cancelled);
            }
            let (logits, last_hidden) = self.talker.forward(&x, offset)?;
            offset += x.dim(1)?;

            // Sample the semantic code from the newest position.
            let last_logits = logits.i((logits.dim(0)? - 1.., ..))?;
            let logits_vec = last_logits.squeeze(0)?.to_vec1::<f32>()?;
            let banned: &[u32] = if codes.len() < 2 { &[eos] } else { &[] };
            let c0 = self.pick_semantic(&logits_vec, &semantic_history, banned, eos, vocab)?;
            if c0 == eos {
                break;
            }
            semantic_history.insert(c0);

            // Expand to the remaining code groups with the sub-talker. The
            // semantic id embeds through the TALKER's table; only the
            // acoustic groups 1..15 use the predictor's own tables.
            self.predictor.reset_cache();
            let past = last_hidden;
            let e0 = self.talker.embed_codec(c0)?;
            let mut hidden = self
                .predictor
                .forward(&Tensor::cat(&[past, e0], 0)?.unsqueeze(0)?, 0)?;
            let mut frame_codes = vec![c0];
            for step in 0..groups - 1 {
                if cancel.is_cancelled() {
                    return Err(GenerateError::Cancelled);
                }
                let last = hidden.i((hidden.dim(0)? - 1.., ..))?;
                let step_logits = self.predictor.logits(&last, step)?;
                let step_vec = step_logits.squeeze(0)?.to_vec1::<f32>()?;
                let pick = if self.greedy {
                    self.sub_sampler.argmax(
                        &mut step_vec.clone(),
                        &HashSet::new(),
                        &[],
                        None,
                        &[],
                    )?
                } else {
                    self.sub_sampler.sample(
                        &mut step_vec.clone(),
                        &HashSet::new(),
                        &[],
                        None,
                        &[],
                    )?
                };
                frame_codes.push(pick as u32);
                if step == groups - 2 {
                    break;
                }
                let emb = self.predictor.embed(step, pick as u32)?;
                hidden = self.predictor.forward(&emb.unsqueeze(0)?, step + 2)?;
            }

            // Next-step input: summed group embeddings plus the text tail;
            // once trailing text is exhausted every frame carries tts_pad.
            let mut next = self.talker.embed_codec(frame_codes[0])?;
            for (step, &code) in frame_codes.iter().enumerate().skip(1) {
                next = next.add(&self.predictor.embed(step - 1, code)?)?;
            }
            let marker = prompt.trailing.get(frame).unwrap_or(&prompt.pad_row);
            let next = next.add(marker)?;
            x = next.unsqueeze(0)?;
            codes.push(frame_codes);
        }

        Ok(codes)
    }
}
