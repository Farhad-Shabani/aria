//! Reduces text to letters, digits, and the few symbols numbers need.

use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategory, GeneralCategoryGroup, UnicodeGeneralCategory};

/// Non-ASCII letters that NFKD leaves whole, so they need spelling out by hand.
const UNDECOMPOSED: &[(char, &str)] = &[
    ('œ', "oe"),
    ('ø', "o"),
    ('æ', "ae"),
    ('ß', "ss"),
    ('đ', "d"),
    ('ð', "d"),
    ('þ', "th"),
    ('ł', "l"),
];

/// Drops diacritics and turns every mark, symbol, and punctuation character
/// into a space, except the ones a number can be written with.
///
/// The kept set is what lets `$20` and `3.5%` survive to the number
/// normalizer, which reads those symbols as part of the value.
pub(super) fn strip(text: &str, keep: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    for character in text.nfkd() {
        if keep.contains(character) {
            stripped.push(character);
        } else if let Some(spelled) = spell_out(character) {
            stripped.push_str(spelled);
        } else if character.general_category() == GeneralCategory::NonspacingMark {
            // A diacritic separated from its letter by NFKD. Dropping it
            // rather than spacing it keeps the letter's word intact.
        } else if is_mark_symbol_or_punctuation(character) {
            stripped.push(' ');
        } else {
            stripped.push(character);
        }
    }
    stripped
}

fn spell_out(character: char) -> Option<&'static str> {
    let lowercase = character.to_lowercase().next()?;
    UNDECOMPOSED
        .iter()
        .find(|(undecomposed, _spelled)| *undecomposed == lowercase)
        .map(|(_undecomposed, spelled)| *spelled)
}

fn is_mark_symbol_or_punctuation(character: char) -> bool {
    matches!(
        character.general_category_group(),
        GeneralCategoryGroup::Mark
            | GeneralCategoryGroup::Symbol
            | GeneralCategoryGroup::Punctuation
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NUMERIC: &str = ".%$¢€£";

    #[test]
    fn punctuation_becomes_a_space() {
        assert_eq!(strip("well, yes!", NUMERIC), "well  yes ");
    }

    #[test]
    fn kept_symbols_survive() {
        assert_eq!(strip("$20 and 5%", NUMERIC), "$20 and 5%");
    }

    #[test]
    fn diacritics_are_dropped_without_splitting_the_word() {
        assert_eq!(strip("café", NUMERIC), "cafe");
    }

    #[test]
    fn undecomposed_letters_are_spelled_out() {
        assert_eq!(strip("æther", NUMERIC), "aether");
    }
}
