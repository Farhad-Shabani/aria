//! Your own word list: one word per line, `#` comments. Personal rules
//! turn what a recognizer writes for these words into the words, and a
//! pair memory learns must name one of them.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const FILE: &str = "lexicon.txt";

const MAX_BYTES: u64 = 1024 * 1024;
const MAX_WORDS: usize = 100_000;
const MAX_WORD_BYTES: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum DictionaryError {
    #[error("the dictionary could not be read")]
    Io(#[from] std::io::Error),
    #[error("the dictionary exceeds its input limit")]
    TooLarge,
}

pub struct Dictionary {
    path: PathBuf,
}

impl Dictionary {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The words, or `None` when there is no file. Never follows a link.
    pub fn words(&self) -> Result<Option<Vec<String>>, DictionaryError> {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the dictionary must be a regular file",
            )
            .into());
        }
        let mut text = String::new();
        file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_BYTES {
            return Err(DictionaryError::TooLarge);
        }
        let words = parse(&text);
        if words.len() > MAX_WORDS
            || words
                .iter()
                .any(|word| word.len() > MAX_WORD_BYTES || word.chars().any(char::is_control))
        {
            return Err(DictionaryError::TooLarge);
        }
        Ok(Some(words))
    }
}

/// The words of the file at `path`, for what only lists them. Missing or
/// unreadable reads as empty.
pub fn load_words(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|text| parse(&text))
        .unwrap_or_default()
}

fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_skip_comments_and_blanks_and_a_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FILE);
        std::fs::write(&path, "# mine\n\n  db2  \nvex16\n").expect("writes");
        assert_eq!(load_words(&path), ["db2", "vex16"]);
        assert!(load_words(&dir.path().join("none.txt")).is_empty());
        let missing = Dictionary::new(dir.path().join("none.txt"));
        assert!(missing.words().expect("reads").is_none());
    }

    #[test]
    fn invalid_words_fail_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let dictionary = Dictionary::new(path.clone());
        std::fs::write(&path, [255]).unwrap();
        assert!(matches!(dictionary.words(), Err(DictionaryError::Io(_))));
        std::fs::write(&path, "a".repeat(257)).unwrap();
        assert!(matches!(dictionary.words(), Err(DictionaryError::TooLarge)));
    }
}
