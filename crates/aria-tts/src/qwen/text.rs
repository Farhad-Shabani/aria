//! Chat-format prompt text for the talker: the Qwen2 tokenizer shipped in the
//! model repo plus the exact wrappers the reference implementation applies
//! before encoding.
//!
//! The repo carries slow-tokenizer files only (`vocab.json` + `merges.txt`);
//! control tokens live in `tokenizer_config.json`'s `added_tokens_decoder`
//! outside the BPE vocabulary. This module rebuilds the fast tokenizer from
//! those three files and verifies every special id after registration — a gap
//! in the added-token range would otherwise silently shift every later id.

use std::collections::BTreeMap;
use std::path::Path;

use tokenizers::models::bpe::{BPE, BpeBuilder};
use tokenizers::pre_tokenizers::byte_level::ByteLevel;
use tokenizers::{AddedToken, DecoderWrapper, PostProcessorWrapper, Tokenizer};

use crate::qwen::{Error, Result};

/// Wraps assistant text the way the reference feeds its processor: an open
/// second assistant turn tells the talker speech is expected next.
pub fn assistant_text(text: &str) -> String {
    format!("<|im_start|>assistant\n{text}<|im_end|>\n<|im_start|>assistant\n")
}

/// Reference-transcript wrapper for in-context voice cloning.
pub fn ref_text(text: &str) -> String {
    format!("<|im_start|>assistant\n{text}<|im_end|>\n")
}

/// Style-instruction wrapper (1.7B models only; 0.6B ignores instructions).
pub fn instruct_text(instruct: &str) -> String {
    format!("<|im_start|>user\n{instruct}<|im_end|>\n")
}

/// Encodes chat-wrapped prompts into Qwen2 token ids.
pub struct ChatTokenizer {
    tokenizer: Tokenizer,
}

impl ChatTokenizer {
    /// Builds the fast tokenizer from a model repo directory holding
    /// `vocab.json`, `merges.txt`, and `tokenizer_config.json`.
    pub fn load(repo_dir: impl AsRef<Path>) -> Result<Self> {
        let dir = repo_dir.as_ref();
        Self::load_files(
            dir.join("vocab.json"),
            dir.join("merges.txt"),
            dir.join("tokenizer_config.json"),
        )
    }

    pub(super) fn load_files(
        vocab: impl AsRef<Path>,
        merges: impl AsRef<Path>,
        config: impl AsRef<Path>,
    ) -> Result<Self> {
        let config = std::fs::read(config).map_err(|source| Error::Read {
            file: "tokenizer_config.json",
            source,
        })?;

        let (vocab, merges) = BPE::read_file(
            &vocab.as_ref().to_string_lossy(),
            &merges.as_ref().to_string_lossy(),
        )
        .map_err(Error::Tokenizer)?;
        // Qwen2 is byte-level BPE over the GPT-2 regex split: no prefix space,
        // no normalization, offsets trimmed on decode.
        let bpe = BpeBuilder::new()
            .vocab_and_merges(vocab, merges)
            .build()
            .map_err(Error::Tokenizer)?;
        let mut tokenizer = Tokenizer::new(bpe);
        tokenizer.with_pre_tokenizer(Some(ByteLevel::new(false, true, true)));
        let decoder: DecoderWrapper = ByteLevel::default().into();
        tokenizer.with_decoder(Some(decoder));
        let post: PostProcessorWrapper =
            tokenizers::processors::byte_level::ByteLevel::default().into();
        tokenizer.with_post_processor(Some(post));

        let specials = Self::special_ids(&config)?;
        tokenizer
            .add_special_tokens(
                specials
                    .values()
                    .map(|content| AddedToken::from(content.as_str(), true)),
            )
            .map_err(Error::Tokenizer)?;
        for (id, content) in &specials {
            if tokenizer.token_to_id(content) != Some(*id) {
                return Err(Error::SpecialIdMismatch {
                    token: content.clone(),
                    expected: *id,
                    got: tokenizer.token_to_id(content).unwrap_or_default(),
                });
            }
        }

        Ok(Self { tokenizer })
    }

    /// `added_tokens_decoder` keyed by numeric id, ascending. Registration
    /// order matters: added ids are assigned sequentially above the BPE
    /// vocabulary, so ascending order reproduces the configured ids exactly.
    fn special_ids(config: &[u8]) -> Result<BTreeMap<u32, String>> {
        let value: serde_json::Value =
            serde_json::from_slice(config).map_err(|source| Error::Json {
                file: "tokenizer_config.json",
                source,
            })?;
        let decoder = value
            .get("added_tokens_decoder")
            .and_then(|d| d.as_object())
            .ok_or(Error::MissingAddedTokens)?;
        decoder
            .iter()
            .map(|(id, entry)| {
                let id = id.parse::<u32>().map_err(|source| Error::AddedTokenId {
                    id: id.clone(),
                    source,
                })?;
                let content = entry
                    .get("content")
                    .and_then(|c| c.as_str())
                    .ok_or(Error::AddedTokenContent { id })?
                    .to_owned();
                Ok((id, content))
            })
            .collect()
    }

    /// Token ids for one wrapped prompt. Specials stay off at encode time: the
    /// wrappers already spell out every control token, and they are matched by
    /// the added-vocabulary before the BPE ever runs.
    pub fn encode(&self, wrapped: &str) -> Result<Vec<u32>> {
        let encoding = self
            .tokenizer
            .encode(wrapped, false)
            .map_err(Error::Encode)?;
        let ids = encoding.get_ids().to_vec();
        if ids.is_empty() {
            return Err(Error::EmptyPrompt);
        }
        Ok(ids)
    }
}
