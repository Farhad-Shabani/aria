//! The words that name a symbol.

const SYMBOLS: [(&str, char); 5] = [
    ("underscore", '_'),
    ("hyphen", '-'),
    ("dot", '.'),
    ("slash", '/'),
    ("backslash", '\\'),
];

/// The symbol `word` names, as "underscore" names `_`.
pub(super) fn symbol(word: &str) -> Option<char> {
    SYMBOLS
        .iter()
        .find(|(name, _)| word.eq_ignore_ascii_case(name))
        .map(|(_, symbol)| *symbol)
}

/// The word that names `character`, as "underscore" names `_`.
pub(super) fn word(character: char) -> Option<&'static str> {
    SYMBOLS
        .iter()
        .find(|(_, symbol)| *symbol == character)
        .map(|(name, _)| *name)
}

/// Whether a spoken symbol types `character`.
pub(super) fn is_symbol(character: char) -> bool {
    word(character).is_some()
}
