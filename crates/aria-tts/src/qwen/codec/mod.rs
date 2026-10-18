//! The speech-codec decoder: turns the talker's 16 codebooks per 12.5 Hz
//! frame into 24 kHz mono PCM.
//!
//! Port of `Qwen3TTSTokenizerV2Decoder` — SplitRVQ, a small causal transformer,
//! ConvNeXt upsampling, and a SnakeBeta convolution stack.

use candle_core::{DType, IndexOp, Tensor};
use candle_nn::{Module, VarBuilder};

use crate::qwen::config::CodecDecoderConfig;
use crate::qwen::nn::rms_norm;

mod pre_transformer;
mod vocoder;

use pre_transformer::TransformerLayer;
use vocoder::{CausalConv, CausalTransConv, ConvNeXtBlock, DecoderBlock, SnakeBeta};

const CODEBOOK_EPSILON: f32 = 1e-5;
/// Frames decoded per window in streaming decode; matches the reference's
/// `chunked_decode` defaults.
const WINDOW_FRAMES: usize = 300;
/// Left context frames each window re-decodes so its conv/attention state
/// matches a whole-sequence pass; the context audio is then discarded.
pub const LEFT_CONTEXT_FRAMES: usize = 25;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("qwen: codec expects {expected} codebooks per frame, got {got}")]
    CodebookCount { expected: usize, got: usize },
    #[error("qwen: codec decode failed")]
    Decode(#[from] candle_core::Error),
    #[error("qwen: codec load failed: {0}")]
    Load(String),
}

type Result<T> = std::result::Result<T, CodecError>;

/// The full decoder graph. Codes come in as `[num_quantizers, frames]`; audio
/// leaves as `[1, samples]` at 24 kHz.
pub struct SpeechCodecDecoder {
    first_embedding: Tensor,
    first_usage: Tensor,
    first_output_proj: Tensor, // [512, 256]
    rest_embeddings: Vec<Tensor>,
    rest_usages: Vec<Tensor>,
    rest_output_proj: Tensor,
    pre_conv: CausalConv,
    input_proj: candle_nn::Linear,
    layers: Vec<TransformerLayer>,
    final_norm: Tensor,
    output_proj: candle_nn::Linear,
    upsample_stages: Vec<(CausalTransConv, ConvNeXtBlock)>,
    head_conv: CausalConv,
    blocks: Vec<DecoderBlock>,
    final_snake: SnakeBeta,
    final_conv: CausalConv,
    num_quantizers: usize,
    samples_per_frame: usize,
    sliding_window: usize,
    rope_theta: f64,
    rms_eps: f64,
}

impl SpeechCodecDecoder {
    /// Loads from a VarBuilder rooted at the tokenizer repo's `decoder.*` keys.
    pub fn load(vb: VarBuilder<'_>, config: &CodecDecoderConfig) -> Result<Self> {
        let hidden = config.hidden_size as usize;
        let latent = config.latent_dim as usize;
        let codebook_dim = config.codebook_dim as usize;
        let quantizer_dim = codebook_dim / 2;
        let codebook_size = config.codebook_size as usize;
        let num_quantizers = config.num_quantizers as usize;
        if quantizer_dim * 2 != codebook_dim {
            return Err(CodecError::Load("codebook_dim must be even".to_owned()));
        }

        let load_codebook = |vb: VarBuilder<'_>| -> Result<(Tensor, Tensor)> {
            let cb = vb.pp("_codebook");
            Ok((
                cb.get((codebook_size, quantizer_dim), "embedding_sum")?,
                cb.get(codebook_size, "cluster_usage")?,
            ))
        };
        let squeeze_proj = |vb: VarBuilder<'_>| -> Result<Tensor> {
            Ok(vb
                .get((codebook_dim, quantizer_dim, 1), "output_proj.weight")?
                .squeeze(2)?)
        };

        let (first_embedding, first_usage) =
            load_codebook(vb.pp("quantizer.rvq_first.vq.layers.0"))?;
        let first_output_proj = squeeze_proj(vb.pp("quantizer.rvq_first"))?;
        let mut rest_embeddings = Vec::with_capacity(num_quantizers - 1);
        let mut rest_usages = Vec::with_capacity(num_quantizers - 1);
        for i in 0..num_quantizers - 1 {
            let (embedding, usage) =
                load_codebook(vb.pp(format!("quantizer.rvq_rest.vq.layers.{i}")))?;
            rest_embeddings.push(embedding);
            rest_usages.push(usage);
        }
        let rest_output_proj = squeeze_proj(vb.pp("quantizer.rvq_rest"))?;

        let pre_conv = CausalConv::load(codebook_dim, latent, 3, 1, 1, 1, vb.pp("pre_conv.conv"))?;

        let transformer = vb.pp("pre_transformer");
        let input_proj = candle_nn::linear(latent, hidden, transformer.pp("input_proj"))?;
        let layers = (0..config.num_hidden_layers)
            .map(|i| TransformerLayer::load(config, transformer.pp(format!("layers.{i}"))))
            .collect::<Result<Vec<_>>>()?;
        let final_norm = transformer.get(hidden, "norm.weight")?;
        let output_proj = candle_nn::linear(hidden, latent, transformer.pp("output_proj"))?;

        let upsample_stages = config
            .upsampling_ratios
            .iter()
            .enumerate()
            .map(|(stage, ratio)| {
                let stage_vb = vb.pp(format!("upsample.{stage}"));
                Ok((
                    CausalTransConv::load(
                        latent,
                        latent,
                        *ratio as usize,
                        *ratio as usize,
                        stage_vb.pp("0.conv"),
                    )?,
                    ConvNeXtBlock::load(latent, stage_vb.pp("1"))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        let decoder_dim = config.decoder_dim as usize;
        let head_conv = CausalConv::load(latent, decoder_dim, 7, 1, 1, 1, vb.pp("decoder.0.conv"))?;
        let blocks = config
            .upsample_rates
            .iter()
            .enumerate()
            .map(|(idx, rate)| {
                DecoderBlock::load(
                    idx,
                    decoder_dim,
                    *rate as usize,
                    vb.pp(format!("decoder.{}", idx + 1)),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let final_dim = decoder_dim >> config.upsample_rates.len();
        let final_snake = SnakeBeta::load(
            final_dim,
            vb.pp(format!("decoder.{}", config.upsample_rates.len() + 1)),
        )?;
        let final_conv = CausalConv::load(
            final_dim,
            1,
            7,
            1,
            1,
            1,
            vb.pp(format!("decoder.{}", config.upsample_rates.len() + 2))
                .pp("conv"),
        )?;

        Ok(Self {
            first_embedding,
            first_usage,
            first_output_proj,
            rest_embeddings,
            rest_usages,
            rest_output_proj,
            pre_conv,
            input_proj,
            layers,
            final_norm,
            output_proj,
            upsample_stages,
            head_conv,
            blocks,
            final_snake,
            final_conv,
            num_quantizers,
            samples_per_frame: {
                let rates: usize = config.upsample_rates.iter().product::<u32>() as _;
                let ratios: usize = config.upsampling_ratios.iter().product::<u32>() as _;
                rates * ratios
            },
            sliding_window: config.sliding_window as usize,
            rope_theta: config.rope_theta,
            rms_eps: config.rms_norm_eps,
        })
    }

    /// Audio samples produced per code frame (24 kHz / 12.5 Hz).
    pub fn samples_per_frame(&self) -> usize {
        self.samples_per_frame
    }

    fn codebook_lookup(
        &self,
        embedding: &Tensor,
        usage: &Tensor,
        codes: &Tensor,
    ) -> Result<Tensor> {
        // Rows normalize to the live codebook: embedding_sum / max(usage, eps).
        let usage = usage.clamp(CODEBOOK_EPSILON, f32::MAX)?.unsqueeze(1)?;
        let table = embedding.broadcast_div(&usage)?;
        Ok(table.index_select(codes, 0)?)
    }

    fn project_latent(&self, embeddings: &Tensor, proj: &Tensor) -> Result<Tensor> {
        Ok(embeddings.matmul(&proj.t()?)?)
    }

    /// Decodes codes into mono samples, clamped to [-1, 1].
    pub fn decode(&self, codes: &Tensor) -> Result<Tensor> {
        let (book_count, _) = codes.dims2()?;
        if book_count != self.num_quantizers {
            return Err(CodecError::CodebookCount {
                expected: self.num_quantizers,
                got: book_count,
            });
        }

        let semantic = codes.i(0)?;
        let latent = self.project_latent(
            &self.codebook_lookup(&self.first_embedding, &self.first_usage, &semantic)?,
            &self.first_output_proj,
        )?;
        let mut total = latent;
        for book in 1..self.num_quantizers {
            let acoustic = self.codebook_lookup(
                &self.rest_embeddings[book - 1],
                &self.rest_usages[book - 1],
                &codes.i(book)?,
            )?;
            total = total.add(&self.project_latent(&acoustic, &self.rest_output_proj)?)?;
        }

        // The quantizer path yields [T, C]; convs run over [B, C, T].
        let h = self
            .pre_conv
            .forward(&total.transpose(0, 1)?.unsqueeze(0)?)?; // [1, latent, T]
        let h = h.transpose(1, 2)?.contiguous()?;
        let h = self.input_proj.forward(&h)?;
        let mut h = h;
        for layer in &self.layers {
            h = layer.forward(&h, self.sliding_window, self.rope_theta, self.rms_eps)?;
        }
        let h = rms_norm(&h.to_dtype(DType::F32)?, &self.final_norm, self.rms_eps)?;
        let h = self.output_proj.forward(&h)?;
        let mut h = h.transpose(1, 2)?.contiguous()?;

        for (transconv, convnext) in &self.upsample_stages {
            let upsampled = transconv.forward(&h)?;
            h = convnext.forward(&upsampled)?;
        }
        let mut wav = self.head_conv.forward(&h)?;
        for block in &self.blocks {
            wav = block.forward(&wav)?;
        }
        let wav = self.final_snake.forward(&wav)?;
        let wav = self.final_conv.forward(&wav)?;
        Ok(wav.clamp(-1.0, 1.0)?)
    }

    /// Whole-sequence decode split into windows with left context, matching
    /// the reference's `chunked_decode`: each window re-decodes its context
    /// frames so boundaries stay continuous, then discards that region.
    pub fn decode_chunked(&self, codes: &Tensor) -> Result<Tensor> {
        let pieces = self.decoded_windows(codes)?.collect::<Result<Vec<_>>>()?;
        Ok(Tensor::cat(&pieces, 2)?)
    }

    /// Decodes one bounded window at a time so playback and cancellation can
    /// advance between expensive codec passes.
    pub fn decoded_windows<'a>(
        &'a self,
        codes: &'a Tensor,
    ) -> Result<impl Iterator<Item = Result<Tensor>> + 'a> {
        let (book_count, frames) = codes.dims2()?;
        if book_count != self.num_quantizers {
            return Err(CodecError::CodebookCount {
                expected: self.num_quantizers,
                got: book_count,
            });
        }
        let upsample = self.samples_per_frame();
        let mut start = 0;
        Ok(std::iter::from_fn(move || {
            if start >= frames {
                return None;
            }
            let end = (start + WINDOW_FRAMES).min(frames);
            let context = LEFT_CONTEXT_FRAMES.min(start);
            let decoded = codes
                .i((.., start - context..end))
                .map_err(CodecError::from)
                .and_then(|window| self.decode(&window))
                .and_then(|wav| {
                    wav.i((.., .., context * upsample..))
                        .map_err(CodecError::from)
                });
            start = end;
            Some(decoded)
        }))
    }
}
