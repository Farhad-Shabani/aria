//! The voice styles beside the model: one npz archive, one entry per voice.
//!
//! An entry is a `[MAX_TOKENS, 1, STYLE_WIDTH]` f32 table: the style vector
//! for an utterance is the row at its token count, so longer utterances get
//! the style the model learned for longer inputs.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
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
        Self::from_archive(BufReader::new(file))
    }

    /// Reads an npz archive: a zip with one `<voice>.npy` entry per voice.
    fn from_archive(reader: impl Read + Seek) -> Result<Self, VoicesError> {
        let mut archive = zip::ZipArchive::new(reader).map_err(archive_error)?;
        if archive.len() > MAX_VOICES {
            return Err(VoicesError::TooManyVoices {
                actual: archive.len(),
                maximum: MAX_VOICES,
            });
        }
        let mut styles = BTreeMap::new();
        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(archive_error)?;
            let name = entry.name();
            let name = name.strip_suffix(".npy").unwrap_or(name).to_owned();
            let array = npyz::NpyFile::new(entry).map_err(archive_error)?;
            let shape = array.shape().to_vec();
            if shape != [MAX_TOKENS as u64, 1, STYLE_WIDTH as u64] {
                return Err(VoicesError::WrongShape { voice: name, shape });
            }
            let table: Vec<f32> = array.into_vec().map_err(archive_error)?;
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

    #[error("voice {voice} has shape {shape:?}; expected [510, 1, 256]")]
    WrongShape { voice: String, shape: Vec<u64> },

    #[error("the voices archive holds {actual} voices; maximum is {maximum}")]
    TooManyVoices { actual: usize, maximum: usize },

    #[error("the voices archive holds no voices")]
    Empty,
}

fn archive_error(source: impl std::error::Error + Send + Sync + 'static) -> VoicesError {
    VoicesError::Archive(Box::new(source))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use npyz::WriterBuilder as _;

    use super::*;

    fn stored_archive() -> Cursor<Vec<u8>> {
        let mut npy = Vec::new();
        let mut writer = npyz::WriteOptions::new()
            .default_dtype()
            .shape(&[MAX_TOKENS as u64, 1, STYLE_WIDTH as u64])
            .writer(&mut npy)
            .begin_nd()
            .unwrap();
        writer
            .extend((0..MAX_TOKENS * STYLE_WIDTH).map(|index| index as f32))
            .unwrap();
        writer.finish().unwrap();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("af_heart.npy", options).unwrap();
        zip.write_all(&npy).unwrap();
        zip.finish().unwrap()
    }

    #[test]
    fn a_stored_archive_names_each_voice_by_its_entry() {
        let mut bytes = stored_archive();
        bytes.set_position(0);

        let voices = VoiceStyles::from_archive(bytes).unwrap();

        assert_eq!(voices.names().collect::<Vec<_>>(), ["af_heart"]);
        let row = voices.style(None, 1).unwrap();
        assert_eq!(row[0], STYLE_WIDTH as f32);
    }
}
