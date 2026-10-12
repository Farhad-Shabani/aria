//! The talker: a Qwen3 decoder over mixed text/codec embeddings that predicts
//! the first (semantic) codebook id per 12.5 Hz frame, plus the text-side
//! embeddings and projection the prompt is built from.

use candle_core::{IndexOp, Tensor};
use candle_nn::Module;
use candle_nn::VarBuilder;

use crate::qwen::config::TalkerConfig;
use crate::qwen::nn::rms_norm;
use crate::qwen::transformer::{DecoderLayer, LayerCache, Result};

pub struct Talker {
    text_embedding: Tensor, // [text_vocab, 2048]
    text_fc1: candle_nn::Linear,
    text_fc2: candle_nn::Linear,
    codec_embedding: Tensor, // [codec_vocab, hidden]
    layers: Vec<DecoderLayer>,
    norm: Tensor,
    codec_head: Tensor, // [codec_vocab, hidden], used transposed
    rope_theta: f64,
    rms_eps: f64,
    caches: Vec<LayerCache>,
}

impl Talker {
    pub fn load(vb: VarBuilder<'_>, config: &TalkerConfig) -> Result<Self> {
        let model = vb.pp("model");
        let hidden = config.hidden_size as usize;
        let text_hidden = config.text_hidden_size as usize;

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
                    512,
                    2048,
                )
            })
            .collect();

        Ok(Self {
            text_embedding: model.get(
                (config.text_vocab_size as usize, text_hidden),
                "text_embedding.weight",
            )?,
            text_fc1: candle_nn::linear(
                text_hidden,
                text_hidden,
                vb.pp("text_projection.linear_fc1"),
            )?,
            text_fc2: candle_nn::linear(text_hidden, hidden, vb.pp("text_projection.linear_fc2"))?,
            codec_embedding: model.get(
                (config.vocab_size as usize, hidden),
                "codec_embedding.weight",
            )?,
            norm: model.get(hidden, "norm.weight")?,
            codec_head: vb.get((config.vocab_size as usize, hidden), "codec_head.weight")?,
            layers,
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

    /// Projects wrapped text token ids into talker space: `[T] -> [T, hidden]`.
    pub fn embed_text(&self, ids: &[u32]) -> Result<Tensor> {
        let device = self.text_embedding.device();
        let ids = Tensor::from_vec(ids.to_vec(), (ids.len(),), device)?;
        let embedded = self.text_embedding.index_select(&ids, 0)?; // [T, text_hidden]
        let projected = candle_nn::ops::silu(&self.text_fc1.forward(&embedded)?)?;
        Ok(self.text_fc2.forward(&projected)?)
    }

    /// Embeds one codec id in the talker's codebook table: `[1, hidden]`.
    pub fn embed_codec(&self, id: u32) -> Result<Tensor> {
        let device = self.codec_embedding.device();
        let ids = Tensor::from_vec(vec![id], (1,), device)?;
        Ok(self.codec_embedding.index_select(&ids, 0)?)
    }

    /// Runs the decoder over `[1, T, hidden]` starting at absolute position
    /// `offset`, returning `(logits [T, codec_vocab], last hidden [1, hidden])`.
    pub fn forward(&mut self, x: &Tensor, offset: usize) -> Result<(Tensor, Tensor)> {
        let mut h = x.clone();
        for (layer, cache) in self.layers.iter_mut().zip(&mut self.caches) {
            h = layer.forward(&h, offset, self.rope_theta, cache)?;
        }
        let h = rms_norm(&h, &self.norm, self.rms_eps)?;
        let logits = h.broadcast_matmul(&self.codec_head.t()?)?; // [1, T, vocab]
        let logits = logits.squeeze(0)?;
        let last = h.i((.., h.dim(1)? - 1.., ..))?.squeeze(0)?;
        Ok((logits, last))
    }
}
