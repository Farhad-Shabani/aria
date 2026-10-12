//! The codec's small causal transformer: windowed multi-head attention and
//! SwiGLU layers with learned residual scales.

use candle_core::{DType, Tensor};
use candle_nn::{Module, VarBuilder};

use crate::qwen::nn::{apply_rope, rms_norm as shared_rms_norm, rope_angles};

use crate::qwen::config::CodecDecoderConfig;

use super::{CodecError, Result};

/// Full multi-head attention under a causal sliding-window mask. Windows are
/// at most a few hundred frames, so eager attention is cheap.
pub(super) struct WindowAttention {
    q_proj: candle_nn::Linear,
    k_proj: candle_nn::Linear,
    v_proj: candle_nn::Linear,
    o_proj: candle_nn::Linear,
    heads: usize,
    head_dim: usize,
}

impl WindowAttention {
    pub(super) fn load(
        hidden: usize,
        heads: usize,
        head_dim: usize,
        vb: VarBuilder<'_>,
    ) -> Result<Self> {
        let projected = heads * head_dim;
        Ok(Self {
            q_proj: candle_nn::linear_no_bias(hidden, projected, vb.pp("q_proj"))?,
            k_proj: candle_nn::linear_no_bias(hidden, projected, vb.pp("k_proj"))?,
            v_proj: candle_nn::linear_no_bias(hidden, projected, vb.pp("v_proj"))?,
            o_proj: candle_nn::linear_no_bias(projected, hidden, vb.pp("o_proj"))?,
            heads,
            head_dim,
        })
    }

    pub(super) fn forward(&self, x: &Tensor, window: Option<usize>, theta: f64) -> Result<Tensor> {
        let (batch, len, _) = x.dims3()?;
        let projected = self.heads * self.head_dim;

        let project = |proj: &candle_nn::Linear| -> Result<Tensor> {
            Ok(proj
                .forward(x)?
                .reshape((batch, len, self.heads, self.head_dim))?
                .transpose(1, 2)?
                .contiguous()?)
        };
        // RoPE over [T, head_dim], broadcast across batch and heads.
        let (cos, sin) =
            rope_angles(self.head_dim, 0, len, theta, x.device()).map_err(CodecError::from)?;
        let q = apply_rope(&project(&self.q_proj)?, &cos, &sin).map_err(CodecError::from)?;
        let k = apply_rope(&project(&self.k_proj)?, &cos, &sin).map_err(CodecError::from)?;
        let v = project(&self.v_proj)?;
        let scores = q
            .matmul(&k.transpose(2, 3)?)?
            .affine((self.head_dim as f64).sqrt().recip(), 0.0)?;
        let reach = window.unwrap_or(len).min(len);
        let mask: Vec<f32> = (0..len)
            .flat_map(move |i| {
                (0..len).map(move |j| {
                    if j <= i && i - j < reach {
                        0.0
                    } else {
                        f32::NEG_INFINITY
                    }
                })
            })
            .collect();
        let mask =
            Tensor::from_vec(mask, (len, len), scores.device())?.broadcast_as(scores.shape())?;
        let weights = candle_nn::ops::softmax(&scores.broadcast_add(&mask)?, 3)?;
        let attended = weights
            .matmul(&v)?
            .transpose(1, 2)?
            .reshape((batch, len, projected))?;
        Ok(self.o_proj.forward(&attended)?)
    }
}

pub(super) struct TransformerLayer {
    attn: WindowAttention,
    gate: candle_nn::Linear,
    up: candle_nn::Linear,
    down: candle_nn::Linear,
    input_norm: Tensor,
    post_norm: Tensor,
    attn_scale: Tensor,
    mlp_scale: Tensor,
}

impl TransformerLayer {
    pub(super) fn load(config: &CodecDecoderConfig, vb: VarBuilder<'_>) -> Result<Self> {
        let hidden = config.hidden_size as usize;
        let intermediate = config.intermediate_size as usize;
        Ok(Self {
            attn: WindowAttention::load(
                hidden,
                config.num_attention_heads as usize,
                config.head_dim as usize,
                vb.pp("self_attn"),
            )?,
            gate: candle_nn::linear_no_bias(hidden, intermediate, vb.pp("mlp.gate_proj"))?,
            up: candle_nn::linear_no_bias(hidden, intermediate, vb.pp("mlp.up_proj"))?,
            down: candle_nn::linear_no_bias(intermediate, hidden, vb.pp("mlp.down_proj"))?,
            input_norm: vb.get(hidden, "input_layernorm.weight")?,
            post_norm: vb.get(hidden, "post_attention_layernorm.weight")?,
            attn_scale: vb.get(hidden, "self_attn_layer_scale.scale")?,
            mlp_scale: vb.get(hidden, "mlp_layer_scale.scale")?,
        })
    }

    pub(super) fn forward(
        &self,
        x: &Tensor,
        window: usize,
        theta: f64,
        rms_eps: f64,
    ) -> Result<Tensor> {
        let scaled = |h: &Tensor, gain: &Tensor| -> Result<Tensor> { Ok(h.broadcast_mul(gain)?) };

        let h = shared_rms_norm(&x.to_dtype(DType::F32)?, &self.input_norm, rms_eps)?;
        let attn_out = self.attn.forward(&h, Some(window), theta)?;
        let x = x.add(&scaled(&attn_out, &self.attn_scale)?)?;

        let h = shared_rms_norm(&x.to_dtype(DType::F32)?, &self.post_norm, rms_eps)?;
        let gated =
            candle_nn::ops::silu(&self.gate.forward(&h)?)?.broadcast_mul(&self.up.forward(&h)?)?;
        let mlp_out = self.down.forward(&gated)?;
        Ok(x.add(&scaled(&mlp_out, &self.mlp_scale)?)?)
    }
}
