//! The words that name a symbol.

/// The symbol `word` names, as "underscore" names `_`.
pub(super) fn symbol(word: &str) -> Option<char> {
    match word.to_lowercase().as_str() {
        "underscore" => Some('_'),
        "hyphen" => Some('-'),
        "dot" => Some('.'),
        _ => None,
    }
}
