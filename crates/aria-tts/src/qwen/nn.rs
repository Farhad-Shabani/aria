//! Tensor operations shared by the crate's transformer implementations:
//! RMSNorm and rotary position embeddings.

use candle_core::{Device, IndexOp, Tensor};

/// RMSNorm over the last dimension with a learned per-channel weight.
pub(super) fn rms_norm(x: &Tensor, weight: &Tensor, eps: f64) -> candle_core::Result<Tensor> {
    let variance = x
        .sqr()?
        .mean(candle_core::D::Minus1)?
        .unsqueeze(candle_core::D::Minus1)?;
    let normed = x.broadcast_div(&(variance + eps)?.sqrt()?.broadcast_as(x.shape())?)?;
    normed.broadcast_mul(weight)
}

/// The cos/sin tables `[len, head_dim]` for absolute positions
/// `offset..offset + len`, duplicated halves matching `rotate_half`.
pub(super) fn rope_angles(
    head_dim: usize,
    offset: usize,
    len: usize,
    theta: f64,
    device: &Device,
) -> candle_core::Result<(Tensor, Tensor)> {
    let half = head_dim / 2;
    let inv_freq: Vec<f32> = (0..half)
        .map(|i| theta.powf(-(2.0 * i as f64) / head_dim as f64) as f32)
        .collect();
    let inv_freq = Tensor::new(inv_freq.as_slice(), device)?;
    let positions: Vec<f32> = (offset..offset + len).map(|i| i as f32).collect();
    let positions = Tensor::new(positions.as_slice(), device)?;
    let angles = Tensor::cat(
        &[
            inv_freq
                .reshape((half, 1))?
                .broadcast_matmul(&positions.reshape((1, len))?)?,
            inv_freq
                .reshape((half, 1))?
                .broadcast_matmul(&positions.reshape((1, len))?)?,
        ],
        0,
    )?; // [head_dim, len]
    Ok((angles.cos()?.t()?, angles.sin()?.t()?))
}

/// Rotates the last dimension of `t` by half: `[-t₂, t₁]`.
pub(super) fn rotate_half(t: &Tensor, half: usize) -> candle_core::Result<Tensor> {
    let a = t.i((.., .., .., ..half))?;
    let b = t.i((.., .., .., half..))?;
    Tensor::cat(&[b.neg()?, a], 3)
}

/// Standard RoPE application: `t·cos + rotate_half(t)·sin`, broadcasting the
/// `[len, head_dim]` tables over any leading batch/head dimensions.
pub(super) fn apply_rope(t: &Tensor, cos: &Tensor, sin: &Tensor) -> candle_core::Result<Tensor> {
    let cos = cos.broadcast_as(t.shape())?;
    let sin = sin.broadcast_as(t.shape())?;
    t.broadcast_mul(&cos)?
        .broadcast_add(&rotate_half(t, t.dim(3)? / 2)?.broadcast_mul(&sin)?)
}
