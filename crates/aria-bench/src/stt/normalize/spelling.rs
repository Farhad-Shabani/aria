//! British to American spelling, so `colour` and `color` are not an error.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Taken verbatim from `whisper/normalizers/english.json` (MIT), itself
/// derived from the tysto.com UK/US spelling list.
const SPELLINGS: &str = include_str!("british-american-spelling.json");

static AMERICAN: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(SPELLINGS).expect("the embedded spelling map should parse")
});

/// Rewrites every word that has an American spelling, leaving the rest alone.
pub(super) fn americanize(text: &str) -> String {
    let mut words = Vec::new();
    for word in text.split_whitespace() {
        words.push(AMERICAN.get(word).map_or(word, String::as_str));
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn british_spellings_become_american() {
        assert_eq!(
            americanize("the colour of aluminium"),
            "the color of aluminum"
        );
    }
}
