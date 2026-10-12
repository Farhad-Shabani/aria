//! The token ids a decode produces, read back as text.
//!
//! Only this direction is needed: the model is given audio, never text, so the
//! vocabulary is loaded for lookup and the merge rules that would encode text
//! are ignored.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use aria_kit::backend::ModelError;

/// The character SentencePiece writes where a word begins.
const WORD_MARK: char = '\u{2581}';

/// A token whose text is one raw byte, written `<0x41>`. Bytes carry the
/// characters the vocabulary has no token for, and only mean something once
/// the run of them is complete.
const BYTE_PREFIX: &str = "<0x";
const BYTE_TOKEN_LENGTH: usize = 6;
const HEXADECIMAL: u32 = 16;

/// What one token id contributes to a transcript.
enum Piece {
    /// Text as it appears in the transcript, word marks already spaces.
    Text(String),
    /// One byte of a character spelled out across several tokens.
    Byte(u8),
    /// A marker that structures the sequence but is never shown.
    Special,
}

/// Every token the model can emit, in id order.
pub(super) struct Vocabulary {
    pieces: Vec<Piece>,
}

impl Vocabulary {
    pub(super) fn load(directory: &Path) -> Result<Self, ModelError> {
        let file = directory.join(TOKENIZER_FILE);
        let text = std::fs::read_to_string(&file).map_err(|source| ModelError::Unreadable {
            path: file.clone(),
            source,
        })?;
        let parsed: TokenizerFile =
            serde_json::from_str(&text).map_err(|source| ModelError::Malformed {
                path: file,
                source: Box::new(source),
            })?;
        Ok(Self::from_tokens(parsed.model.vocab, parsed.added_tokens))
    }

    /// The file maps token text to id; decoding needs the reverse. Laying
    /// the pieces out in id order once here saves a search per token, per
    /// transcript.
    fn from_tokens(vocab: HashMap<String, u32>, added: Vec<AddedToken>) -> Self {
        let special: HashSet<u32> = added
            .iter()
            .filter(|token| token.special)
            .map(|token| token.id)
            .collect();

        let size = vocab.values().copied().max().map_or(0, |last| last + 1);
        let mut pieces = Vec::new();
        pieces.resize_with(size as usize, || Piece::Special);
        for (token, id) in vocab {
            pieces[id as usize] = match special.contains(&id) {
                true => Piece::Special,
                false => piece(&token),
            };
        }
        Self { pieces }
    }

    /// The text of a decoded sequence. Ids the vocabulary does not name are
    /// dropped: the model can emit one, and losing a token beats failing a
    /// transcript the user just spoke.
    pub(super) fn decode(&self, tokens: &[u32]) -> String {
        let mut text = String::new();
        let mut bytes = Vec::new();
        let mut unnamed = 0;

        for &token in tokens {
            match self.pieces.get(token as usize) {
                Some(Piece::Byte(byte)) => bytes.push(*byte),
                Some(Piece::Text(piece)) => {
                    flush(&mut bytes, &mut text);
                    text.push_str(piece);
                }
                Some(Piece::Special) => flush(&mut bytes, &mut text),
                // Flushed like any other break in the run, so the bytes on
                // either side are not read as one character.
                None => {
                    flush(&mut bytes, &mut text);
                    unnamed += 1;
                }
            }
        }
        flush(&mut bytes, &mut text);

        if unnamed > 0 {
            tracing::warn!(tokens = unnamed, "dropped tokens outside the vocabulary");
        }
        // The vocabulary marks the start of every word, including the first.
        text.strip_prefix(' ').unwrap_or(&text).to_owned()
    }
}

/// Empties a run of raw bytes into the transcript. A run that is not valid
/// UTF-8 stands for characters that were not transcribed, one replacement each.
fn flush(bytes: &mut Vec<u8>, text: &mut String) {
    if bytes.is_empty() {
        return;
    }
    match std::str::from_utf8(bytes) {
        Ok(decoded) => text.push_str(decoded),
        Err(_) => text.extend(std::iter::repeat_n(
            char::REPLACEMENT_CHARACTER,
            bytes.len(),
        )),
    }
    bytes.clear();
}

fn piece(token: &str) -> Piece {
    match byte_value(token) {
        Some(byte) => Piece::Byte(byte),
        None => Piece::Text(token.replace(WORD_MARK, " ")),
    }
}

fn byte_value(token: &str) -> Option<u8> {
    if token.len() != BYTE_TOKEN_LENGTH {
        return None;
    }
    let digits = token.strip_prefix(BYTE_PREFIX)?.strip_suffix('>')?;
    u8::from_str_radix(digits, HEXADECIMAL).ok()
}

#[derive(Deserialize)]
struct TokenizerFile {
    model: TokenizerModel,
    added_tokens: Vec<AddedToken>,
}

#[derive(Deserialize)]
struct TokenizerModel {
    #[serde(deserialize_with = "within_bounds")]
    vocab: HashMap<String, u32>,
}

/// The pieces are laid out in id order, so the largest id decides how much
/// memory the vocabulary takes. A file claiming an id beyond any real
/// vocabulary is refused instead of sized into memory. Moonshine's is 32000.
const MAX_TOKEN_ID: u32 = 1 << 20;

fn within_bounds<'de, D>(deserializer: D) -> Result<HashMap<String, u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let vocab = HashMap::<String, u32>::deserialize(deserializer)?;
    match vocab.values().copied().max() {
        Some(highest) if highest > MAX_TOKEN_ID => Err(serde::de::Error::custom(format!(
            "token id {highest} is beyond any real vocabulary"
        ))),
        _ => Ok(vocab),
    }
}

#[derive(Deserialize)]
struct AddedToken {
    id: u32,
    special: bool,
}

pub(super) const TOKENIZER_FILE: &str = "tokenizer.json";

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids match the shipped vocabulary's specials so the cases read like real
    /// sequences: 0 unknown, 1 start, 2 end.
    fn vocabulary(tokens: &[(&str, u32)]) -> Vocabulary {
        let vocab = tokens
            .iter()
            .map(|&(token, id)| (token.to_owned(), id))
            .collect();
        let added = [0, 1, 2]
            .map(|id| AddedToken { id, special: true })
            .into_iter()
            .collect();
        Vocabulary::from_tokens(vocab, added)
    }

    #[test]
    fn word_marks_become_the_spaces_between_words() {
        let vocabulary = vocabulary(&[("▁the", 10), ("▁socket", 11), ("▁closes", 12)]);

        assert_eq!(vocabulary.decode(&[10, 11, 12]), "the socket closes");
    }

    #[test]
    fn the_leading_word_mark_is_not_a_leading_space() {
        let vocabulary = vocabulary(&[("▁the", 10), ("re", 11)]);

        assert_eq!(vocabulary.decode(&[10, 11]), "there");
    }

    #[test]
    fn special_markers_are_not_transcribed() {
        let vocabulary = vocabulary(&[("<s>", 1), ("</s>", 2), ("▁the", 10)]);

        assert_eq!(vocabulary.decode(&[1, 10, 2]), "the");
    }

    #[test]
    fn a_run_of_bytes_spells_one_character() {
        // "ñ" is U+00F1, written 0xC3 0xB1 across two tokens.
        let vocabulary = vocabulary(&[("<0xC3>", 20), ("<0xB1>", 21), ("▁se", 10), ("or", 11)]);

        assert_eq!(vocabulary.decode(&[10, 20, 21, 11]), "señor");
    }

    #[test]
    fn a_byte_run_that_is_not_a_character_marks_what_was_lost() {
        let vocabulary = vocabulary(&[("<0xC3>", 20), ("▁the", 10)]);

        assert_eq!(vocabulary.decode(&[10, 20]), "the\u{fffd}");
    }

    #[test]
    fn a_token_outside_the_vocabulary_is_dropped() {
        let vocabulary = vocabulary(&[("▁the", 10), ("▁socket", 11)]);

        assert_eq!(vocabulary.decode(&[10, 9_000, 11]), "the socket");
    }

    /// Two half-characters either side of a dropped token are not one
    /// character the speaker never said.
    #[test]
    fn a_dropped_token_breaks_the_byte_run_around_it() {
        let vocabulary = vocabulary(&[("<0xC3>", 20), ("<0xB1>", 21), ("▁the", 10)]);

        assert_eq!(
            vocabulary.decode(&[10, 20, 9_000, 21]),
            "the\u{fffd}\u{fffd}"
        );
    }

    /// `Vocabulary` holds no `Debug`, so the error is taken out by hand.
    fn load_error(directory: &Path) -> ModelError {
        match Vocabulary::load(directory) {
            Err(error) => error,
            Ok(_) => panic!("a vocabulary must not load from this directory"),
        }
    }

    #[test]
    fn a_tokenizer_that_is_not_json_is_reported_as_malformed() {
        let directory = tempfile::TempDir::new().expect("test directory should be created");
        std::fs::write(directory.path().join(TOKENIZER_FILE), b"not json")
            .expect("test tokenizer should be written");

        let error = load_error(directory.path());

        assert!(matches!(error, ModelError::Malformed { .. }));
    }

    #[test]
    fn a_byte_token_is_told_apart_from_text_that_looks_like_one() {
        assert_eq!(byte_value("<0x41>"), Some(0x41));
        assert_eq!(byte_value("<0xZZ>"), None);
        assert_eq!(byte_value("<0x41"), None);
        assert_eq!(byte_value("▁<0x41>"), None);
    }
}
