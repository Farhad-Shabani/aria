//! The codec's waveform stack: SnakeBeta activations, zero-padded causal
//! convolutions, transposed-convolution upsampling, and the ConvNeXt-flavored
//! residual blocks that turn latents into audio.

use candle_core::{IndexOp, Tensor};
use candle_nn::{Module, VarBuilder};

use super::{CodecError, Result};

/// Reshapes a channel vector to broadcast over `[batch, channels, time]`.
fn channel_bias(t: &Tensor) -> Result<Tensor> {
    Ok(t.reshape((1, t.elem_count(), 1))?)
}

/// `x + (1/beta) * sin(alpha * x)^2` with learned `exp`-scaled alpha/beta.
pub(super) struct SnakeBeta {
    alpha: Tensor,
    beta: Tensor,
}

impl SnakeBeta {
    pub(super) fn load(dim: usize, vb: VarBuilder<'_>) -> Result<Self> {
        Ok(Self {
            alpha: channel_bias(&vb.get(dim, "alpha")?)?,
            beta: channel_bias(&vb.get(dim, "beta")?)?,
        })
    }

    pub(super) fn forward(&self, x: &Tensor) -> Result<Tensor> {
        // Parameters are stored pre-exp; the activation exponentiates them.
        let alpha = self.alpha.exp()?;
        let beta = self.beta.exp()?;
        let sin_squared = x.broadcast_mul(&alpha)?.sin()?.sqr()?;
        Ok(x.broadcast_add(&sin_squared.broadcast_div(&beta)?)?)
    }
}

/// Zero-padded left-causal Conv1d producing exactly `ceil(len/stride)` frames.
pub(super) struct CausalConv {
    conv: candle_nn::Conv1d,
    /// Samples prepended so no future audio leaks in: `(k-1)*dilation + 1 - stride`.
    pad_left: usize,
}

impl CausalConv {
    pub(super) fn load(
        in_channels: usize,
        out_channels: usize,
        kernel: usize,
        stride: usize,
        dilation: usize,
        groups: usize,
        vb: VarBuilder<'_>,
    ) -> Result<Self> {
        let effective = (kernel - 1) * dilation + 1;
        let cfg = candle_nn::Conv1dConfig {
            stride,
            dilation,
            groups,
            ..Default::default()
        };
        if effective < stride {
            return Err(CodecError::Load(format!(
                "kernel {kernel} dilation {dilation} cannot support stride {stride}"
            )));
        }
        Ok(Self {
            conv: candle_nn::conv1d(in_channels, out_channels, kernel, cfg, vb)?,
            pad_left: effective - stride,
        })
    }

    pub(super) fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let len = x.dim(2)?;
        let stride = self.conv.config().stride;
        let frames = len.div_ceil(stride);
        let extra = frames * stride - len;
        let padded = x.pad_with_zeros(2, self.pad_left, extra)?;
        Ok(self.conv.forward(&padded)?)
    }
}

/// Transposed convolution upsampling by integer `stride`, trimming the
/// `kernel - stride` tail samples the reference drops.
pub(super) struct CausalTransConv {
    conv: candle_nn::ConvTranspose1d,
    trim_right: usize,
}

impl CausalTransConv {
    pub(super) fn load(
        in_channels: usize,
        out_channels: usize,
        kernel: usize,
        stride: usize,
        vb: VarBuilder<'_>,
    ) -> Result<Self> {
        let cfg = candle_nn::ConvTranspose1dConfig {
            stride,
            ..Default::default()
        };
        Ok(Self {
            conv: candle_nn::conv_transpose1d(in_channels, out_channels, kernel, cfg, vb)?,
            trim_right: kernel - stride,
        })
    }

    pub(super) fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let out = self.conv.forward(x)?;
        let len = out.dim(2)?;
        Ok(out.i((.., .., ..len - self.trim_right))?)
    }
}

/// Depthwise-conv → LayerNorm → MLP block with a learned residual gain.
pub(super) struct ConvNeXtBlock {
    dwconv: CausalConv,
    norm_weight: Tensor,
    norm_bias: Tensor,
    pwconv1: candle_nn::Linear,
    pwconv2: candle_nn::Linear,
    gamma: Tensor,
}

impl ConvNeXtBlock {
    const LN_EPS: f64 = 1e-6;

    pub(super) fn load(dim: usize, vb: VarBuilder<'_>) -> Result<Self> {
        Ok(Self {
            dwconv: CausalConv::load(dim, dim, 7, 1, 1, dim, vb.pp("dwconv.conv"))?,
            norm_weight: vb.get(dim, "norm.weight")?,
            norm_bias: vb.get(dim, "norm.bias")?,
            pwconv1: candle_nn::linear(dim, 4 * dim, vb.pp("pwconv1"))?,
            pwconv2: candle_nn::linear(4 * dim, dim, vb.pp("pwconv2"))?,
            gamma: channel_bias(&vb.get(dim, "gamma")?)?.contiguous()?,
        })
    }

    pub(super) fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let h = self.dwconv.forward(x)?;
        // The norm and pointwise mix run over the channel axis.
        let h = h.transpose(1, 2)?.contiguous()?;
        let h = candle_nn::ops::layer_norm(
            &h,
            &self.norm_weight,
            &self.norm_bias,
            Self::LN_EPS as f32,
        )?;
        let h = self.pwconv1.forward(&h)?;
        let h = self.pwconv2.forward(&h.gelu_erf()?)?;
        let h = h.transpose(1, 2)?.contiguous()?;
        Ok(x.add(&h.broadcast_mul(&self.gamma)?)?)
    }
}

pub(super) struct DecoderBlock {
    snake: SnakeBeta,
    transconv: CausalTransConv,
    residual_units: Vec<(SnakeBeta, CausalConv, SnakeBeta, CausalConv)>,
}

impl DecoderBlock {
    pub(super) fn load(
        layer_idx: usize,
        decoder_dim: usize,
        rate: usize,
        vb: VarBuilder<'_>,
    ) -> Result<Self> {
        let in_dim = decoder_dim / (1 << layer_idx);
        let out_dim = decoder_dim / (1 << (layer_idx + 1));
        let snake = SnakeBeta::load(in_dim, vb.pp("block.0"))?;
        let transconv =
            CausalTransConv::load(in_dim, out_dim, 2 * rate, rate, vb.pp("block.1.conv"))?;
        let mut residual_units = Vec::with_capacity(3);
        for (unit, dilation) in [(2usize, 1usize), (3, 3), (4, 9)] {
            let unit_vb = vb.pp(format!("block.{unit}"));
            residual_units.push((
                SnakeBeta::load(out_dim, unit_vb.pp("act1"))?,
                CausalConv::load(
                    out_dim,
                    out_dim,
                    7,
                    1,
                    dilation,
                    1,
                    unit_vb.pp("conv1.conv"),
                )?,
                SnakeBeta::load(out_dim, unit_vb.pp("act2"))?,
                CausalConv::load(out_dim, out_dim, 1, 1, 1, 1, unit_vb.pp("conv2.conv"))?,
            ));
        }
        Ok(Self {
            snake,
            transconv,
            residual_units,
        })
    }

    pub(super) fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let mut h = self.snake.forward(x)?;
        h = self.transconv.forward(&h)?;
        for (act1, conv1, act2, conv2) in &self.residual_units {
            let y = conv2.forward(&act2.forward(&conv1.forward(&act1.forward(&h)?)?)?)?;
            h = h.add(&y)?;
        }
        Ok(h)
    }
}
