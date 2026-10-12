//! The voice styles beside the model: one npz archive, one entry per voice.
//!
//! An entry is a `[MAX_TOKENS, 1, STYLE_WIDTH]` f32 table: the style vector
//! for an utterance is the row at its token count, so longer utterances get
//! the style the model learned for longer inputs.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use super::{MAX_TOKENS, STYLE_WIDTH};

/// The voice Kokoro speaks with when nobody asks for one.
const DEFAULT_VOICE: &str = "af_heart";

/// A ceiling on how many voices one archive may hold. Kokoro ships about
/// fifty; the cap bounds what a compressed archive can make this allocate.
const MAX_VOICES: usize = 256;

pub(super) struct VoiceStyles {
    /// Ordered, so the default fallback is deterministic.
    styles: BTreeMap<String, Vec<f32>>,
}

impl VoiceStyles {
    pub(super) fn read(path: &Path) -> Result<Self, VoicesError> {
        let file = File::open(path).map_err(VoicesError::Open)?;
        let mut archive = npyz::npz::NpzArchive::new(BufReader::new(file))
            .map_err(|source| VoicesError::Archive(Box::new(source)))?;

        let names: Vec<String> = archive.array_names().map(str::to_owned).collect();
        if names.len() > MAX_VOICES {
            return Err(VoicesError::TooManyVoices {
                actual: names.len(),
                maximum: MAX_VOICES,
            });
        }
        let mut styles = BTreeMap::new();
        for name in names {
            let array = archive
                .by_name(&name)
                .map_err(|source| VoicesError::Archive(Box::new(source)))?
                .ok_or_else(|| VoicesError::MissingEntry {
                    voice: name.clone(),
                })?;
            let shape = array.shape().to_vec();
            if shape != [MAX_TOKENS as u64, 1, STYLE_WIDTH as u64] {
                return Err(VoicesError::WrongShape { voice: name, shape });
            }
            let table: Vec<f32> = array
                .into_vec()
                .map_err(|source| VoicesError::Archive(Box::new(source)))?;
            styles.insert(name, table);
        }
        if styles.is_empty() {
            return Err(VoicesError::Empty);
        }
        Ok(Self { styles })
    }

    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.styles.keys().map(String::as_str)
    }

    /// The style row for `voice` at `tokens` tokens. `None` names a voice the
    /// archive does not hold.
    pub(super) fn style(&self, voice: Option<&str>, tokens: usize) -> Option<&[f32]> {
        let table = match voice {
            Some(name) => self.styles.get(name)?,
            None => self
                .styles
                .get(DEFAULT_VOICE)
                .or_else(|| self.styles.values().next())
                .expect("read() refuses an empty archive"),
        };
        let row = tokens.min(MAX_TOKENS - 1);
        Some(&table[row * STYLE_WIDTH..(row + 1) * STYLE_WIDTH])
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum VoicesError {
    #[error("failed to open the voices file")]
    Open(#[source] std::io::Error),

    #[error("the voices file is not a readable npz archive")]
    Archive(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("the voices archive lost its entry for {voice}")]
    MissingEntry { voice: String },

    #[error("voice {voice} has shape {shape:?}; expected [510, 1, 256]")]
    WrongShape { voice: String, shape: Vec<u64> },

    #[error("the voices archive holds {actual} voices; maximum is {maximum}")]
    TooManyVoices { actual: usize, maximum: usize },

    #[error("the voices archive holds no voices")]
    Empty,
}
