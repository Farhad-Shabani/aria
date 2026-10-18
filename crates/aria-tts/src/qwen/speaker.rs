//! The x-vector speaker encoder (ECAPA-TDNN): mel frames in, a 1024-dim
//! speaker embedding out. Used by Base models to clone a voice from a
//! reference clip without any preset speaker table.

use candle_core::{Device, Tensor};
use candle_nn::{Module, VarBuilder};

#[derive(Debug, thiserror::Error)]
#[error("qwen: speaker encoder failed")]
pub struct SpeakerError(#[from] candle_core::Error);

pub type Result<T> = std::result::Result<T, SpeakerError>;

fn relu(x: &Tensor) -> Result<Tensor> {
    Ok(x.relu()?)
}

const MEL_DIM: usize = 128;
const ENC_DIM: usize = 1024;
/// Channel widths of the initial TDNN, the three SE-Res2Net blocks, and the
/// multi-layer aggregation.
const CHANNELS: [usize; 5] = [512, 512, 512, 512, 1536];
const KERNELS: [usize; 5] = [5, 3, 3, 3, 1];
const DILATIONS: [usize; 5] = [1, 2, 3, 4, 1];
const RES2NET_SCALE: usize = 8;
const SE_CHANNELS: usize = 128;
const ATTENTION_CHANNELS: usize = 128;
const STATS_EPSILON: f32 = 1e-12;

/// Reflect-pads `[B, C, T]` along time without repeating the edge samples,
/// matching `padding="same", padding_mode="reflect"`.
fn reflect_pad(x: &Tensor, pad: usize) -> Result<Tensor> {
    if pad == 0 {
        return Ok(x.clone());
    }
    let len = x.dim(2)?;
    let device = x.device();
    // Left: samples[pad], ..., samples[1]; right: samples[len-2], ..., reversed.
    let left_indices: Vec<u32> = (1..=pad).rev().map(|i| i as u32).collect();
    let right_indices: Vec<u32> = (0..pad).rev().map(|i| (len - 2 - i) as u32).collect();
    let pick = |indices: &[u32]| -> Result<Tensor> {
        let index = Tensor::from_vec(indices.to_vec(), (indices.len(),), device)?;
        Ok(x.index_select(&index, 2)?)
    };
    let left = pick(&left_indices)?;
    let right = pick(&right_indices)?;
    Ok(Tensor::cat(&[left, x.clone(), right], 2)?)
}

/// A same-output Conv1d over reflect padding.
struct SameConv {
    conv: candle_nn::Conv1d,
}

impl SameConv {
    fn load(
        in_channels: usize,
        out_channels: usize,
        kernel: usize,
        dilation: usize,
        vb: VarBuilder<'_>,
    ) -> Result<Self> {
        let cfg = candle_nn::Conv1dConfig {
            dilation,
            ..Default::default()
        };
        Ok(Self {
            conv: candle_nn::conv1d(in_channels, out_channels, kernel, cfg, vb)?,
        })
    }

    /// x: `[B, C, T]` → `[B, out, T]`
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let kernel = self.conv.weight().dim(2)?;
        let dilation = self.conv.config().dilation;
        let pad = dilation * (kernel - 1) / 2;
        let padded = reflect_pad(x, pad)?;
        Ok(self.conv.forward(&padded)?)
    }
}

fn tdnn(
    in_channels: usize,
    out_channels: usize,
    kernel: usize,
    dilation: usize,
    vb: VarBuilder<'_>,
) -> Result<SameConv> {
    SameConv::load(in_channels, out_channels, kernel, dilation, vb.pp("conv"))
}

/// Squeeze-and-excitation recalibration over the time axis.
struct SeBlock {
    conv1: candle_nn::Conv1d, // 1x1
    conv2: candle_nn::Conv1d, // 1x1
}

impl SeBlock {
    fn load(in_channels: usize, vb: VarBuilder<'_>) -> Result<Self> {
        Ok(Self {
            conv1: candle_nn::conv1d(
                in_channels,
                SE_CHANNELS,
                1,
                candle_nn::Conv1dConfig::default(),
                vb.pp("conv1"),
            )?,
            conv2: candle_nn::conv1d(
                SE_CHANNELS,
                in_channels,
                1,
                candle_nn::Conv1dConfig::default(),
                vb.pp("conv2"),
            )?,
        })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let mean = x
            .mean(candle_core::D::Minus1)?
            .unsqueeze(candle_core::D::Minus1)?; // [B, C, 1]
        let h = self.conv1.forward(&mean)?.relu()?;
        let scale = candle_nn::ops::sigmoid(&self.conv2.forward(&h)?)?;
        Ok(x.broadcast_mul(&scale)?)
    }
}

struct Res2NetBlock {
    blocks: Vec<SameConv>,
}

impl Res2NetBlock {
    fn load(channels: usize, kernel: usize, dilation: usize, vb: VarBuilder<'_>) -> Result<Self> {
        let part = channels / RES2NET_SCALE;
        let mut blocks = Vec::with_capacity(RES2NET_SCALE - 1);
        for i in 0..RES2NET_SCALE - 1 {
            blocks.push(SameConv::load(
                part,
                part,
                kernel,
                dilation,
                vb.pp(format!("blocks.{i}.conv")),
            )?);
        }
        Ok(Self { blocks })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let channels = x.dim(1)?;
        let part = channels / RES2NET_SCALE;
        let mut parts: Vec<Tensor> = Vec::with_capacity(RES2NET_SCALE);
        parts.push(x.narrow(1, 0, part)?);
        let mut running: Option<Tensor> = None;
        for (block, conv) in self.blocks.iter().enumerate() {
            let part_tensor = x.narrow(1, (block + 1) * part, part)?;
            let input = match &running {
                Some(prev) => part_tensor.add(prev)?,
                None => part_tensor,
            };
            let out = relu(&conv.forward(&input)?)?;
            running = Some(out.clone());
            parts.push(out);
        }
        Ok(Tensor::cat(&parts, 1)?)
    }
}

struct SeRes2NetBlock {
    tdnn1: SameConv,
    res2net_block: Res2NetBlock,
    tdnn2: SameConv,
    se_block: SeBlock,
}

impl SeRes2NetBlock {
    fn load(
        in_channels: usize,
        out_channels: usize,
        kernel: usize,
        dilation: usize,
        vb: VarBuilder<'_>,
    ) -> Result<Self> {
        Ok(Self {
            tdnn1: tdnn(in_channels, out_channels, 1, 1, vb.pp("tdnn1"))?,
            res2net_block: Res2NetBlock::load(
                out_channels,
                kernel,
                dilation,
                vb.pp("res2net_block"),
            )?,
            tdnn2: tdnn(out_channels, out_channels, 1, 1, vb.pp("tdnn2"))?,
            se_block: SeBlock::load(out_channels, vb.pp("se_block"))?,
        })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let h = relu(&self.tdnn1.forward(x)?)?;
        let h = self.res2net_block.forward(&h)?;
        let h = relu(&self.tdnn2.forward(&h)?)?;
        let h = self.se_block.forward(&h)?;
        Ok(x.add(&h)?)
    }
}

/// Attentive statistics pooling: attention weights from the concatenated
/// statistics context, then weighted mean and standard deviation.
struct AttentiveStatisticsPooling {
    tdnn: SameConv,
    conv: candle_nn::Conv1d, // 1x1
}

impl AttentiveStatisticsPooling {
    fn load(channels: usize, attention_channels: usize, vb: VarBuilder<'_>) -> Result<Self> {
        Ok(Self {
            tdnn: SameConv::load(
                channels * 3,
                attention_channels,
                1,
                1,
                vb.pp("tdnn").pp("conv"),
            )?,
            conv: candle_nn::conv1d(
                attention_channels,
                channels,
                1,
                candle_nn::Conv1dConfig::default(),
                vb.pp("conv"),
            )?,
        })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let len = x.dim(2)?;
        let mean0 = x
            .mean(candle_core::D::Minus1)?
            .unsqueeze(candle_core::D::Minus1)?;
        let variance = x
            .broadcast_sub(&mean0)?
            .sqr()?
            .mean(candle_core::D::Minus1)?
            .unsqueeze(candle_core::D::Minus1)?;
        let std0 = variance.sqrt()?;
        // The statistics ride every frame as the attention context.
        let channels = mean0.dim(1)?;
        let mean0 = mean0.broadcast_as((x.dim(0)?, channels, len))?;
        let std0 = std0.broadcast_as((x.dim(0)?, channels, len))?;
        let context = Tensor::cat(&[x.clone(), mean0, std0], 1)?;

        let attention = relu(&self.tdnn.forward(&context)?)?.tanh()?;
        let attention =
            candle_nn::ops::softmax(&self.conv.forward(&attention)?, 2)?.to_dtype(x.dtype())?;

        let mean = attention
            .broadcast_mul(x)?
            .sum(candle_core::D::Minus1)?
            .unsqueeze(candle_core::D::Minus1)?;
        let spread = x
            .broadcast_sub(&mean)?
            .sqr()?
            .broadcast_mul(&attention)?
            .sum(candle_core::D::Minus1)?;
        let std = spread.clamp(STATS_EPSILON as f64, f64::MAX)?.sqrt()?;
        // Back to [B, C, 1] so the final 1x1 conv sees a time axis.
        let std = std.unsqueeze(candle_core::D::Minus1)?;
        Ok(Tensor::cat(&[mean, std], 1)?)
    }
}

pub struct SpeakerEncoder {
    first: SameConv,
    seres_blocks: Vec<SeRes2NetBlock>,
    mfa: SameConv,
    asp: AttentiveStatisticsPooling,
    fc: SameConv,
}

impl SpeakerEncoder {
    /// Loads from a VarBuilder rooted at the model's `speaker_encoder.*` keys.
    pub fn load(vb: VarBuilder<'_>, _device: &Device) -> Result<Self> {
        let first = tdnn(
            MEL_DIM,
            CHANNELS[0],
            KERNELS[0],
            DILATIONS[0],
            vb.pp("blocks.0"),
        )?;
        let mut seres_blocks = Vec::with_capacity(CHANNELS.len() - 2);
        for i in 1..CHANNELS.len() - 1 {
            seres_blocks.push(SeRes2NetBlock::load(
                CHANNELS[i - 1],
                CHANNELS[i],
                KERNELS[i],
                DILATIONS[i],
                vb.pp(format!("blocks.{i}")),
            )?);
        }
        let aggregated_channels: usize = CHANNELS[1..CHANNELS.len() - 1].iter().sum();
        let mfa = tdnn(
            aggregated_channels,
            CHANNELS[CHANNELS.len() - 1],
            KERNELS[KERNELS.len() - 1],
            DILATIONS[DILATIONS.len() - 1],
            vb.pp("mfa"),
        )?;
        let asp = AttentiveStatisticsPooling::load(
            CHANNELS[CHANNELS.len() - 1],
            ATTENTION_CHANNELS,
            vb.pp("asp"),
        )?;
        let fc = SameConv::load(CHANNELS[CHANNELS.len() - 1] * 2, ENC_DIM, 1, 1, vb.pp("fc"))?;
        Ok(Self {
            first,
            seres_blocks,
            mfa,
            asp,
            fc,
        })
    }

    /// Embeds one utterance. `mel` is channel-major: `[1, mel_dim, T]`.
    pub fn embed(&mut self, mel: &Tensor) -> Result<Tensor> {
        let mut h = mel.clone();
        h = relu(&self.first.forward(&h)?)?;

        // Multi-layer feature aggregation skips the initial TDNN output.
        let mut aggregated: Vec<Tensor> = Vec::with_capacity(self.seres_blocks.len());
        for block in &self.seres_blocks {
            h = block.forward(&h)?;
            aggregated.push(h.clone());
        }
        let h = Tensor::cat(&aggregated, 1)?;
        let h = relu(&self.mfa.forward(&h)?)?;
        let pooled = self.asp.forward(&h)?;
        Ok(self.fc.forward(&pooled)?.squeeze(2)?)
    }
}
