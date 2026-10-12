//! Locates model files and bounds their sizes. Parsing and tensor allocation
//! belong to the engine.

use std::path::PathBuf;

use aria_kit::backend::BackendError;
use aria_kit::synthesis::{SynthesisError, validate_model_file};

/// Files a talker model directory must carry.
pub(super) const TALKER_WEIGHTS: &str = "model.safetensors";
pub(super) const TALKER_CONFIG: &str = "config.json";

/// The speech-tokenizer repo mirrors this layout under its own directory.
pub(super) const TOKENIZER_WEIGHTS: &str = "model.safetensors";
pub(super) const TOKENIZER_CONFIG: &str = "config.json";

const CHAT_VOCAB: &str = "vocab.json";
const CHAT_MERGES: &str = "merges.txt";
const CHAT_CONFIG: &str = "tokenizer_config.json";

pub(super) const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CHAT_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_VOCAB_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MERGES_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TALKER_WEIGHTS_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_CODEC_WEIGHTS_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The two model repos an engine instance reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The talker model repo (config.json + weights + Qwen2 tokenizer files).
    pub model_dir: PathBuf,
    /// The speech-tokenizer repo (codec weights + its config.json).
    pub speech_tokenizer_dir: PathBuf,
}

/// Every artifact a load touches, resolved and bounded.
pub(super) struct ModelFiles {
    pub talker_weights: PathBuf,
    pub talker_config: PathBuf,
    pub chat_vocab: PathBuf,
    pub chat_merges: PathBuf,
    pub chat_config: PathBuf,
    pub codec_weights: PathBuf,
    pub codec_config: PathBuf,
}

impl ModelFiles {
    pub(super) fn check(config: &Config) -> Result<Self, SynthesisError> {
        Ok(Self {
            talker_weights: checked_file(
                config.model_dir.join(TALKER_WEIGHTS),
                TALKER_WEIGHTS,
                MAX_TALKER_WEIGHTS_BYTES,
            )?,
            talker_config: checked_file(
                config.model_dir.join(TALKER_CONFIG),
                TALKER_CONFIG,
                MAX_CONFIG_BYTES,
            )?,
            chat_vocab: checked_file(
                config.model_dir.join(CHAT_VOCAB),
                CHAT_VOCAB,
                MAX_VOCAB_BYTES,
            )?,
            chat_merges: checked_file(
                config.model_dir.join(CHAT_MERGES),
                CHAT_MERGES,
                MAX_MERGES_BYTES,
            )?,
            chat_config: checked_file(
                config.model_dir.join(CHAT_CONFIG),
                CHAT_CONFIG,
                MAX_CHAT_CONFIG_BYTES,
            )?,
            codec_weights: checked_file(
                config.speech_tokenizer_dir.join(TOKENIZER_WEIGHTS),
                TOKENIZER_WEIGHTS,
                MAX_CODEC_WEIGHTS_BYTES,
            )?,
            codec_config: checked_file(
                config.speech_tokenizer_dir.join(TOKENIZER_CONFIG),
                TOKENIZER_CONFIG,
                MAX_CONFIG_BYTES,
            )?,
        })
    }
}

fn checked_file(
    path: PathBuf,
    label: &'static str,
    maximum_bytes: u64,
) -> Result<PathBuf, SynthesisError> {
    let path = validate_model_file(path, crate::qwen::NAME)?;
    let size = std::fs::metadata(&path)
        .map_err(|error| load_failed(label, error))?
        .len();
    if size > maximum_bytes {
        return Err(load_failed(
            label,
            Detail(format!("file exceeds {maximum_bytes} bytes")),
        ));
    }
    Ok(path)
}

/// Reads a bounded UTF-8 artifact, rejecting anything past `maximum_bytes`.
pub(super) fn read_bounded_text(
    path: &std::path::Path,
    label: &'static str,
    maximum_bytes: u64,
) -> Result<String, SynthesisError> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|error| load_failed(label, error))?;
    let mut text = String::new();
    file.take(maximum_bytes + 1)
        .read_to_string(&mut text)
        .map_err(|error| load_failed(label, error))?;
    if text.len() as u64 > maximum_bytes {
        return Err(load_failed(
            label,
            Detail(format!("file exceeds {maximum_bytes} bytes")),
        ));
    }
    Ok(text)
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct Detail(pub(super) String);

pub(super) fn load_failed(
    model: impl Into<String>,
    error: impl std::error::Error + Send + Sync + 'static,
) -> SynthesisError {
    SynthesisError::Load {
        backend: crate::qwen::NAME,
        model: model.into(),
        source: BackendError::engine(error),
    }
}
