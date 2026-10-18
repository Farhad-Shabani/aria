//! Shared Qwen3-style decoder pieces used by both the talker and the code
//! predictor: GQA attention with head-dim RMSNorm, SwiGLU MLP, RMSNorm, and
//! standard RoPE over a KV cache.
//!
//! Positions are plain sequential offsets. The talker's MRoPE collapses to
//! this form here because synthesis never pads batches, so the temporal,
//! height, and width position ids are always equal.

use candle_core::{IndexOp, Tensor};
use candle_nn::Module;
use candle_nn::kv_cache::KvCache;

use crate::qwen::config::TalkerConfig;
use crate::qwen::nn::{apply_rope, rms_norm, rope_angles};

#[derive(Debug, thiserror::Error)]
pub enum TransformerError {
    #[error("qwen: transformer forward failed")]
    Forward(#[from] candle_core::Error),
}

pub type Result<T> = std::result::Result<T, TransformerError>;

/// Per-layer key/value state; sequence axis is dim 2 of `[B, H, S, D]`.
pub struct LayerCache {
    inner: KvCache,
}

impl LayerCache {
    pub fn new(kv_heads: usize, head_dim: usize, initial_seq: usize, grow_by: usize) -> Self {
        let _ = (kv_heads, head_dim, grow_by);
        Self {
            // Buffers allocate lazily and grow by `initial_seq` positions on overflow.
            inner: KvCache::new(2, initial_seq),
        }
    }

    pub fn clear(&mut self) {
        self.inner.reset();
    }
}

struct Attention {
    q_proj: candle_nn::Linear,
    k_proj: candle_nn::Linear,
    v_proj: candle_nn::Linear,
    o_proj: candle_nn::Linear,
    q_norm: Tensor,
    k_norm: Tensor,
    heads: usize,
    kv_heads: usize,
    head_dim: usize,
    norm_eps: f64,
}

impl Attention {
    #[allow(clippy::too_many_arguments)]
    fn load(
        hidden: usize,
        heads: usize,
        kv_heads: usize,
        head_dim: usize,
        rms_eps: f64,
        vb: candle_nn::VarBuilder<'_>,
    ) -> Result<Self> {
        Ok(Self {
            q_proj: candle_nn::linear_no_bias(hidden, heads * head_dim, vb.pp("q_proj"))?,
            k_proj: candle_nn::linear_no_bias(hidden, kv_heads * head_dim, vb.pp("k_proj"))?,
            v_proj: candle_nn::linear_no_bias(hidden, kv_heads * head_dim, vb.pp("v_proj"))?,
            o_proj: candle_nn::linear_no_bias(heads * head_dim, hidden, vb.pp("o_proj"))?,
            q_norm: vb.get(head_dim, "q_norm.weight")?,
            k_norm: vb.get(head_dim, "k_norm.weight")?,
            heads,
            kv_heads,
            head_dim,
            norm_eps: rms_eps,
        })
    }

    fn rope_cos_sin(&self, offset: usize, len: usize, theta: f64) -> Result<(Tensor, Tensor)> {
        rope_angles(self.head_dim, offset, len, theta, self.q_norm.device())
            .map_err(TransformerError::from)
    }

    /// `x`: `[1, T, hidden]` appended after `offset` earlier tokens.
    fn forward(
        &self,
        x: &Tensor,
        offset: usize,
        theta: f64,
        cache: &mut LayerCache,
    ) -> Result<Tensor> {
        let seq = x.dims3()?.1;

        let project =
            |proj: &candle_nn::Linear, groups: usize, norm: Option<&Tensor>| -> Result<Tensor> {
                let t = proj.forward(x)?.reshape((1, seq, groups, self.head_dim))?;
                let t = match norm {
                    Some(n) => rms_norm(&t, n, self.norm_eps)?,
                    None => t,
                };
                Ok(t.transpose(1, 2)?.contiguous()?)
            };
        let mut q = project(&self.q_proj, self.heads, Some(&self.q_norm))?;
        let mut k = project(&self.k_proj, self.kv_heads, Some(&self.k_norm))?;
        let v = project(&self.v_proj, self.kv_heads, None)?;

        let (cos, sin) = self.rope_cos_sin(offset, seq, theta)?;
        q = apply_rope(&q, &cos, &sin)?;
        k = apply_rope(&k, &cos, &sin)?;

        // GQA: duplicate each kv head across its query-head group.
        let group = self.heads / self.kv_heads;
        let (k_all, v_all) = cache.inner.append(&k, &v)?;
        let total = offset + seq;
        // Head-major interleave so query head h sees kv head h / group:
        // [B, KV, S, D] -> [B, KV, group, S, D] -> [B, KV*group, S, D].
        let expand = |t: &Tensor| -> Result<Tensor> {
            if group == 1 {
                return Ok(t.clone());
            }
            let (b, kv, s, d) = t.dims4()?;
            Ok(t.unsqueeze(2)?
                .expand((b, kv, group, s, d))?
                .reshape((b, kv * group, s, d))?)
        };
        let k_full = expand(&k_all.i((.., .., ..total, ..))?)?;
        let v_full = expand(&v_all.i((.., .., ..total, ..))?)?;

        let scale = (self.head_dim as f64).sqrt().recip();
        let scores = q.matmul(&k_full.transpose(2, 3)?)?.affine(scale, 0.0)?;
        let total = scores.dim(3)?;
        let mut mask = vec![0f32; seq * total];
        for i in 0..seq {
            let absolute = offset + i;
            for j in 0..total {
                if j > absolute {
                    mask[i * total + j] = f32::NEG_INFINITY;
                }
            }
        }
        let mask =
            Tensor::from_vec(mask, (seq, total), scores.device())?.broadcast_as(scores.shape())?;
        let weights = candle_nn::ops::softmax(&scores.broadcast_add(&mask)?, 3)?;
        let attended = weights.matmul(&v_full)?.transpose(1, 2)?.reshape((
            1,
            seq,
            self.heads * self.head_dim,
        ))?;
        Ok(self.o_proj.forward(&attended)?)
    }
}

struct Mlp {
    gate: candle_nn::Linear,
    up: candle_nn::Linear,
    down: candle_nn::Linear,
}

impl Mlp {
    fn load(hidden: usize, intermediate: usize, vb: candle_nn::VarBuilder<'_>) -> Result<Self> {
        Ok(Self {
            gate: candle_nn::linear_no_bias(hidden, intermediate, vb.pp("gate_proj"))?,
            up: candle_nn::linear_no_bias(hidden, intermediate, vb.pp("up_proj"))?,
            down: candle_nn::linear_no_bias(intermediate, hidden, vb.pp("down_proj"))?,
        })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let gated =
            candle_nn::ops::silu(&self.gate.forward(x)?)?.broadcast_mul(&self.up.forward(x)?)?;
        Ok(self.down.forward(&gated)?)
    }
}

/// One pre-norm decoder block, identical in shape between talker and predictor.
pub struct DecoderLayer {
    attn: Attention,
    mlp: Mlp,
    input_norm: Tensor,
    post_norm: Tensor,
    rms_eps: f64,
}

impl DecoderLayer {
    pub fn load(talker_config: &TalkerConfig, vb: candle_nn::VarBuilder<'_>) -> Result<Self> {
        Self::load_with(
            talker_config.hidden_size as usize,
            talker_config.num_attention_heads as usize,
            talker_config.num_key_value_heads as usize,
            talker_config.head_dim as usize,
            talker_config.intermediate_size as usize,
            talker_config.rms_norm_eps,
            vb,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn load_with(
        hidden: usize,
        heads: usize,
        kv_heads: usize,
        head_dim: usize,
        intermediate: usize,
        rms_eps: f64,
        vb: candle_nn::VarBuilder<'_>,
    ) -> Result<Self> {
        Ok(Self {
            attn: Attention::load(
                hidden,
                heads,
                kv_heads,
                head_dim,
                rms_eps,
                vb.pp("self_attn"),
            )?,
            mlp: Mlp::load(hidden, intermediate, vb.pp("mlp"))?,
            input_norm: vb.get(hidden, "input_layernorm.weight")?,
            post_norm: vb.get(hidden, "post_attention_layernorm.weight")?,
            rms_eps,
        })
    }

    pub fn forward(
        &self,
        x: &Tensor,
        offset: usize,
        theta: f64,
        cache: &mut LayerCache,
    ) -> Result<Tensor> {
        let h = rms_norm(x, &self.input_norm, self.rms_eps)?;
        let h = self.attn.forward(&h, offset, theta, cache)?;
        let x = x.add(&h)?;

        let h = rms_norm(&x, &self.post_norm, self.rms_eps)?;
        let h = self.mlp.forward(&h)?;
        Ok(x.add(&h)?)
    }
}
