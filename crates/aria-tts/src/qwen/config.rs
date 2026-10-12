//! Typed views of the Hugging Face `config.json` files a Qwen3-TTS engine loads:
//! the talker model repo and the `Qwen3-TTS-Tokenizer-12Hz` speech-tokenizer repo.
//!
//! Unknown fields are ignored because Hugging Face configs include metadata
//! that this engine does not use.

use std::collections::HashMap;

const CODE_GROUPS: u32 = 16;
const OUTPUT_SAMPLE_RATE: u32 = 24_000;
const SAMPLES_PER_FRAME: u32 = 1_920;

const MAX_HIDDEN_SIZE: u32 = 8_192;
const MAX_INTERMEDIATE_SIZE: u32 = 65_536;
const MAX_LAYERS: u32 = 128;
const MAX_HEADS: u32 = 256;
const MAX_HEAD_DIM: u32 = 512;
const MAX_VOCAB_SIZE: u32 = 2_000_000;
const MAX_POSITION_EMBEDDINGS: u32 = 1_000_000;
const MAX_UPSAMPLE_STAGES: usize = 16;
const MAX_UPSAMPLE_RATE: u32 = 64;

/// Supported model conditioning modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelType {
    CustomVoice,
    Base,
}

impl ModelType {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "custom_voice" => Some(Self::CustomVoice),
            "base" => Some(Self::Base),
            _ => None,
        }
    }
}

/// The talker repo's `config.json`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_model_type")]
    pub model_type: String,
    pub tts_model_type: String,
    pub im_start_token_id: u32,
    pub im_end_token_id: u32,
    pub tts_pad_token_id: u32,
    pub tts_bos_token_id: u32,
    pub tts_eos_token_id: u32,
    pub talker_config: TalkerConfig,
}

fn default_model_type() -> String {
    "qwen3_tts".to_owned()
}

/// Text ids are Qwen2 vocabulary entries; codec ids live in the talker's own
/// small codebook vocabulary.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TalkerConfig {
    pub hidden_size: u32,
    pub intermediate_size: u32,
    pub num_hidden_layers: u32,
    pub num_attention_heads: u32,
    pub num_key_value_heads: u32,
    pub head_dim: u32,
    pub rms_norm_eps: f64,
    pub rope_theta: f64,
    pub vocab_size: u32,
    pub text_vocab_size: u32,
    pub text_hidden_size: u32,
    pub num_code_groups: u32,
    pub rope_scaling: RopeScaling,
    #[serde(default)]
    pub attention_bias: bool,
    pub codec_bos_id: u32,
    pub codec_eos_token_id: u32,
    pub codec_think_id: u32,
    pub codec_nothink_id: u32,
    pub codec_think_bos_id: u32,
    pub codec_think_eos_id: u32,
    pub codec_pad_id: u32,
    pub spk_id: HashMap<String, u32>,
    /// Speakers either speak plain (`false`) or force one dialect language.
    pub spk_is_dialect: HashMap<String, SpeakerDialect>,
    pub codec_language_id: HashMap<String, u32>,
    pub code_predictor_config: CodePredictorConfig,
}

/// MRoPE channel split over query/key dims. The three sections partition
/// `head_dim / 2` rotation frequencies; interleaved scatters them stride-3.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RopeScaling {
    pub interleaved: bool,
    pub mrope_section: [u32; 3],
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum SpeakerDialect {
    Plain(bool),
    Dialect(String),
}

impl SpeakerDialect {
    /// The language a speaker forces, if any.
    pub fn forced_dialect(&self) -> Option<&str> {
        match self {
            Self::Dialect(name) => Some(name),
            Self::Plain(_) => None,
        }
    }
}

/// The sub-talker that expands each semantic frame into the remaining
/// 15 acoustic codebooks.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CodePredictorConfig {
    pub hidden_size: u32,
    pub intermediate_size: u32,
    pub num_hidden_layers: u32,
    pub num_attention_heads: u32,
    pub num_key_value_heads: u32,
    pub head_dim: u32,
    pub rms_norm_eps: f64,
    pub rope_theta: f64,
    pub vocab_size: u32,
    pub num_code_groups: u32,
}

/// The decoder fields from the speech-tokenizer repo's `config.json`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SpeechTokenizerConfig {
    pub output_sample_rate: u32,
    pub decode_upsample_rate: u32,
    pub decoder_config: CodecDecoderConfig,
}

/// ConvNeXt/transformer hybrid that turns 16 codebooks per 12.5 Hz frame into
/// 24 kHz PCM.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CodecDecoderConfig {
    pub latent_dim: u32,
    pub codebook_dim: u32,
    pub codebook_size: u32,
    pub decoder_dim: u32,
    pub hidden_size: u32,
    pub intermediate_size: u32,
    pub num_hidden_layers: u32,
    pub num_attention_heads: u32,
    pub num_key_value_heads: u32,
    pub head_dim: u32,
    pub num_quantizers: u32,
    pub sliding_window: u32,
    pub rope_theta: f64,
    pub rms_norm_eps: f64,
    pub upsample_rates: Vec<u32>,
    pub upsampling_ratios: Vec<u32>,
    pub layer_scale_initial_scale: f64,
    pub max_position_embeddings: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("qwen: unsupported tts_model_type {value:?} (expected custom_voice or base)")]
    UnsupportedModelType { value: String },
    #[error("qwen: expected qwen3_tts model_type, found {value:?}")]
    UnexpectedModelType { value: String },
    #[error("qwen: invalid config.json: {0}")]
    Parse(String),
    #[error("qwen: expected 16 code groups, found {value}")]
    UnexpectedCodeGroups { value: u32 },
    #[error("qwen: {field} must be between 1 and {maximum}, found {value}")]
    InvalidDimension {
        field: &'static str,
        value: u32,
        maximum: u32,
    },
    #[error("qwen: {heads_field} must be divisible by {kv_heads_field}")]
    InvalidAttentionGroups {
        heads_field: &'static str,
        kv_heads_field: &'static str,
    },
    #[error("qwen: mrope_section {:?} does not partition head_dim {head_dim}", .sections)]
    InvalidMropeSections { sections: [u32; 3], head_dim: u32 },
    #[error("qwen: expected speech-tokenizer sample rate 24000, found {value}")]
    UnexpectedSampleRate { value: u32 },
    #[error("qwen: expected speech-tokenizer samples per frame 1920, found {value}")]
    UnexpectedSamplesPerFrame { value: u32 },
    #[error("qwen: speech-tokenizer upsampling factors are invalid")]
    InvalidUpsampling,
}

impl ModelConfig {
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let config: Self =
            serde_json::from_str(json).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub(super) fn model_type(&self) -> ModelType {
        ModelType::parse(&self.tts_model_type).expect("validated")
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.model_type != "qwen3_tts" {
            return Err(ConfigError::UnexpectedModelType {
                value: self.model_type.clone(),
            });
        }
        if ModelType::parse(&self.tts_model_type).is_none() {
            return Err(ConfigError::UnsupportedModelType {
                value: self.tts_model_type.clone(),
            });
        }
        let talker = &self.talker_config;
        validate_code_groups(
            talker.num_code_groups,
            talker.code_predictor_config.num_code_groups,
        )?;
        validate_dimension("talker.hidden_size", talker.hidden_size, MAX_HIDDEN_SIZE)?;
        validate_dimension(
            "talker.intermediate_size",
            talker.intermediate_size,
            MAX_INTERMEDIATE_SIZE,
        )?;
        validate_dimension(
            "talker.num_hidden_layers",
            talker.num_hidden_layers,
            MAX_LAYERS,
        )?;
        validate_attention(
            "talker.num_attention_heads",
            talker.num_attention_heads,
            "talker.num_key_value_heads",
            talker.num_key_value_heads,
            "talker.head_dim",
            talker.head_dim,
        )?;
        validate_dimension("talker.vocab_size", talker.vocab_size, MAX_VOCAB_SIZE)?;
        validate_dimension(
            "talker.text_vocab_size",
            talker.text_vocab_size,
            MAX_VOCAB_SIZE,
        )?;
        validate_dimension(
            "talker.text_hidden_size",
            talker.text_hidden_size,
            MAX_HIDDEN_SIZE,
        )?;
        let predictor = &talker.code_predictor_config;
        validate_dimension(
            "code_predictor.hidden_size",
            predictor.hidden_size,
            MAX_HIDDEN_SIZE,
        )?;
        validate_dimension(
            "code_predictor.intermediate_size",
            predictor.intermediate_size,
            MAX_INTERMEDIATE_SIZE,
        )?;
        validate_dimension(
            "code_predictor.num_hidden_layers",
            predictor.num_hidden_layers,
            MAX_LAYERS,
        )?;
        validate_attention(
            "code_predictor.num_attention_heads",
            predictor.num_attention_heads,
            "code_predictor.num_key_value_heads",
            predictor.num_key_value_heads,
            "code_predictor.head_dim",
            predictor.head_dim,
        )?;
        validate_dimension(
            "code_predictor.vocab_size",
            predictor.vocab_size,
            MAX_VOCAB_SIZE,
        )?;
        let sections = talker.rope_scaling.mrope_section;
        if !talker.head_dim.is_multiple_of(2) || sections.iter().sum::<u32>() != talker.head_dim / 2
        {
            return Err(ConfigError::InvalidMropeSections {
                sections,
                head_dim: talker.head_dim,
            });
        }
        Ok(())
    }
}

impl SpeechTokenizerConfig {
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let config: Self =
            serde_json::from_str(json).map_err(|error| ConfigError::Parse(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.output_sample_rate != OUTPUT_SAMPLE_RATE {
            return Err(ConfigError::UnexpectedSampleRate {
                value: self.output_sample_rate,
            });
        }
        if self.decode_upsample_rate != SAMPLES_PER_FRAME {
            return Err(ConfigError::UnexpectedSamplesPerFrame {
                value: self.decode_upsample_rate,
            });
        }
        let decoder = &self.decoder_config;
        if decoder.num_quantizers != CODE_GROUPS {
            return Err(ConfigError::UnexpectedCodeGroups {
                value: decoder.num_quantizers,
            });
        }
        validate_dimension("decoder.latent_dim", decoder.latent_dim, MAX_HIDDEN_SIZE)?;
        validate_dimension(
            "decoder.codebook_dim",
            decoder.codebook_dim,
            MAX_HIDDEN_SIZE,
        )?;
        validate_dimension(
            "decoder.codebook_size",
            decoder.codebook_size,
            MAX_VOCAB_SIZE,
        )?;
        validate_dimension("decoder.decoder_dim", decoder.decoder_dim, MAX_HIDDEN_SIZE)?;
        validate_dimension("decoder.hidden_size", decoder.hidden_size, MAX_HIDDEN_SIZE)?;
        validate_dimension(
            "decoder.intermediate_size",
            decoder.intermediate_size,
            MAX_INTERMEDIATE_SIZE,
        )?;
        validate_dimension(
            "decoder.num_hidden_layers",
            decoder.num_hidden_layers,
            MAX_LAYERS,
        )?;
        validate_attention(
            "decoder.num_attention_heads",
            decoder.num_attention_heads,
            "decoder.num_key_value_heads",
            decoder.num_key_value_heads,
            "decoder.head_dim",
            decoder.head_dim,
        )?;
        validate_dimension(
            "decoder.max_position_embeddings",
            decoder.max_position_embeddings,
            MAX_POSITION_EMBEDDINGS,
        )?;
        validate_upsampling(decoder, self.decode_upsample_rate)
    }
}

fn validate_dimension(field: &'static str, value: u32, maximum: u32) -> Result<(), ConfigError> {
    if value == 0 || value > maximum {
        return Err(ConfigError::InvalidDimension {
            field,
            value,
            maximum,
        });
    }
    Ok(())
}

fn validate_attention(
    heads_field: &'static str,
    heads: u32,
    kv_heads_field: &'static str,
    kv_heads: u32,
    head_dim_field: &'static str,
    head_dim: u32,
) -> Result<(), ConfigError> {
    validate_dimension(heads_field, heads, MAX_HEADS)?;
    validate_dimension(kv_heads_field, kv_heads, MAX_HEADS)?;
    validate_dimension(head_dim_field, head_dim, MAX_HEAD_DIM)?;
    if !heads.is_multiple_of(kv_heads) {
        return Err(ConfigError::InvalidAttentionGroups {
            heads_field,
            kv_heads_field,
        });
    }
    Ok(())
}

fn validate_upsampling(decoder: &CodecDecoderConfig, expected: u32) -> Result<(), ConfigError> {
    let factors = decoder
        .upsample_rates
        .iter()
        .chain(&decoder.upsampling_ratios);
    let mut count = 0usize;
    let mut product = 1u32;
    for &factor in factors {
        count += 1;
        if factor == 0 || factor > MAX_UPSAMPLE_RATE {
            return Err(ConfigError::InvalidUpsampling);
        }
        product = product
            .checked_mul(factor)
            .ok_or(ConfigError::InvalidUpsampling)?;
    }
    if count == 0 || count > MAX_UPSAMPLE_STAGES || product != expected {
        return Err(ConfigError::InvalidUpsampling);
    }
    Ok(())
}

fn validate_code_groups(talker: u32, predictor: u32) -> Result<(), ConfigError> {
    if talker != CODE_GROUPS {
        return Err(ConfigError::UnexpectedCodeGroups { value: talker });
    }
    if predictor != CODE_GROUPS {
        return Err(ConfigError::UnexpectedCodeGroups { value: predictor });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_component_requires_sixteen_code_groups() {
        assert!(matches!(
            validate_code_groups(8, CODE_GROUPS),
            Err(ConfigError::UnexpectedCodeGroups { value: 8 })
        ));
        assert!(matches!(
            validate_code_groups(CODE_GROUPS, 8),
            Err(ConfigError::UnexpectedCodeGroups { value: 8 })
        ));
    }

    #[test]
    fn attention_groups_must_divide_evenly() {
        assert!(matches!(
            validate_attention("heads", 10, "kv_heads", 4, "head_dim", 64),
            Err(ConfigError::InvalidAttentionGroups { .. })
        ));
    }
}
