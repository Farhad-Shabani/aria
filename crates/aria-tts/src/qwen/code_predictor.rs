//! The code predictor ("sub-talker"): expands the talker's semantic frame id
//! into the remaining 15 acoustic codebooks with a small autoregressive
//! decoder — one embedding table and output head per code group.

use candle_core::Tensor;

use candle_nn::VarBuilder;

use crate::qwen::config::CodePredictorConfig;
use crate::qwen::nn::rms_norm;
use crate::qwen::transformer::{DecoderLayer, LayerCache, Result};

pub struct CodePredictor {
    embeddings: Vec<Tensor>, // per group 1..15: [2048, hidden]
    heads: Vec<Tensor>,      // per group: [2048, hidden], used transposed
    layers: Vec<DecoderLayer>,
    norm: Tensor,
    rope_theta: f64,
    rms_eps: f64,
    caches: Vec<LayerCache>,
}

impl CodePredictor {
    pub fn load(vb: VarBuilder<'_>, config: &CodePredictorConfig) -> Result<Self> {
        let model = vb.pp("model");
        let hidden = config.hidden_size as usize;
        let groups = (config.num_code_groups - 1) as usize;

        let layers = (0..config.num_hidden_layers)
            .map(|i| {
                DecoderLayer::load_with(
                    hidden,
                    config.num_attention_heads as usize,
                    config.num_key_value_heads as usize,
                    config.head_dim as usize,
                    config.intermediate_size as usize,
                    config.rms_norm_eps,
                    model.pp(format!("layers.{i}")),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let caches = (0..config.num_hidden_layers)
            .map(|_| {
                LayerCache::new(
                    config.num_key_value_heads as usize,
                    config.head_dim as usize,
                    256,
                    1024,
                )
            })
            .collect();

        let mut embeddings = Vec::with_capacity(groups);
        let mut heads = Vec::with_capacity(groups);
        for group in 0..groups {
            embeddings.push(model.get(
                (config.vocab_size as usize, hidden),
                &format!("codec_embedding.{group}.weight"),
            )?);
            heads.push(vb.get(
                (config.vocab_size as usize, hidden),
                &format!("lm_head.{group}.weight"),
            )?);
        }

        Ok(Self {
            embeddings,
            heads,
            layers,
            norm: model.get(hidden, "norm.weight")?,
            rope_theta: config.rope_theta,
            rms_eps: config.rms_norm_eps,
            caches,
        })
    }

    pub fn reset_cache(&mut self) {
        for cache in &mut self.caches {
            cache.clear();
        }
    }

    /// Embeds code `code` from group `step` (0-indexed over groups 1..15):
    /// `[1, hidden]`.
    pub fn embed(&self, step: usize, code: u32) -> Result<Tensor> {
        let device = self.embeddings[step].device();
        let ids = Tensor::from_vec(vec![code], (1,), device)?;
        Ok(self.embeddings[step].index_select(&ids, 0)?)
    }

    /// Runs the decoder over `[1, T, hidden]` at absolute position `offset`,
    /// returning hidden states `[T, hidden]`; the caller applies the head for
    /// the current generation step.
    pub fn forward(&mut self, x: &Tensor, offset: usize) -> Result<Tensor> {
        let mut h = x.clone();
        for (layer, cache) in self.layers.iter_mut().zip(&mut self.caches) {
            h = layer.forward(&h, offset, self.rope_theta, cache)?;
        }
        Ok(rms_norm(&h, &self.norm, self.rms_eps)?.squeeze(0)?)
    }

    /// Logits for one generation step: hidden `[T, hidden]` through the head
    /// of group `step`.
    pub fn logits(&self, hidden: &Tensor, step: usize) -> Result<Tensor> {
        Ok(hidden.matmul(&self.heads[step].t()?)?)
    }
}
