use std::borrow::Cow;

use crate::cancel::CancelToken;

use super::symbol_words::symbol;
use super::vocabulary::Word;
use super::{Layer, TranscriptionError, Vocabulary, check_cancelled, check_size};

/// Joins the words around "underscore", "hyphen", or "dot", as in
/// "user underscore id" → `user_id`.
#[derive(Debug, Clone, Copy, Default)]
pub struct JoinSymbols;

impl Layer for JoinSymbols {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        let corrected = Vocabulary::default().correct_with(
            text,
            |text, words| claim(text, words, None),
            &|bytes| {
                check_cancelled(cancel)?;
                check_size(bytes)
            },
        )?;
        if let Some(corrected) = corrected {
            *text = corrected;
        }
        Ok(())
    }
}

/// How many of the words `words` ends on more speech may still change: a
/// symbol word that may join what follows. A hyphen keeps the word before
/// it too: once that word is typed, the hyphen can't join it to a number.
pub(super) fn open_tail(text: &str, words: &[Word<'_>]) -> usize {
    let last = words.last().filter(|word| word.claim.is_none());
    match last.and_then(|word| symbol(word.typed(text))) {
        Some('-') => words.len().min(2),
        Some(_) => 1,
        None => 0,
    }
}

/// Whether a symbol between them joins `left` and `right`, whatever
/// punctuation sits outside the pair.
pub(super) fn joins(left: &str, right: &str) -> bool {
    let left = left.trim_start_matches(|c: char| c.is_ascii_punctuation());
    let right = right.trim_end_matches(|c: char| c.is_ascii_punctuation());
    is_joinable(left) && is_joinable(right)
}

/// Claims each run of words that spoken symbols join as one word, as in
/// "user underscore id" → `user_id`. A symbol opening `text` joins `before`,
/// the last word already typed, so its run types from the symbol on.
pub(super) fn claim<'a>(text: &str, words: Vec<Word<'a>>, before: Option<&str>) -> Vec<Word<'a>> {
    let links = links(text, &words, before);
    let mut joined = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let first = index;
        let mut typed = String::new();
        if let (0, Some(symbol)) = (index, links[0]) {
            typed.push(symbol);
            index = 1;
        }
        let mut last = index;
        while links.get(last + 1).is_some_and(Option::is_some) {
            last += 2;
        }
        if typed.is_empty() && last == index {
            joined.push(words[index].clone());
            index += 1;
            continue;
        }
        typed.push_str(words[index].typed(text));
        for link in (index + 1..last).step_by(2) {
            typed.extend(links[link]);
            typed.push_str(words[link + 1].typed(text));
        }
        joined.push(Word {
            span: words[first].span.start..words[last].span.end,
            claim: Some(Cow::Owned(typed)),
        });
        index = last + 1;
    }
    joined
}

/// The symbol each word becomes when it links its two neighbors.
fn links(text: &str, words: &[Word<'_>], before: Option<&str>) -> Vec<Option<char>> {
    let mut links = vec![None; words.len()];
    for index in 0..words.len().saturating_sub(1) {
        if words[index].claim.is_some() {
            continue;
        }
        let Some(character) = symbol(words[index].typed(text)) else {
            continue;
        };
        let left = match index {
            0 => before,
            _ => Some(words[index - 1].typed(text)),
        };
        let Some(left) = left else {
            continue;
        };
        let right = words[index + 1].typed(text);
        let gap_start = index.checked_sub(1).map_or(0, |left| words[left].span.end);
        let line_break = text[gap_start..words[index + 1].span.start].contains('\n');
        if line_break || !joins(left, right) {
            continue;
        }
        // Typed text can't change, and a caret keeps "-5" apart from it.
        let opens_number =
            index == 0 && character == '-' && right.starts_with(|c: char| c.is_ascii_digit());
        if opens_number {
            continue;
        }
        links[index] = Some(character);
    }
    links
}

fn is_joinable(word: &str) -> bool {
    word.starts_with(char::is_alphanumeric)
        && word.ends_with(char::is_alphanumeric)
        && word
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && symbol(word).is_none()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::vocabulary::PersonalRules;
    use super::*;

    fn applied(heard: &str) -> String {
        let mut text = heard.to_owned();
        JoinSymbols.apply(&mut text, &CancelToken::never()).unwrap();
        text
    }

    #[rstest]
    #[case::underscore_between_words("user underscore id", "user_id")]
    #[case::underscore_chain("max underscore retry underscore count", "max_retry_count")]
    #[case::hyphen_between_words("user hyphen id", "user-id")]
    #[case::dot_in_a_name("docs dot local", "docs.local")]
    #[case::joined_word_as_neighbor("aria-kit dot rs", "aria-kit.rs")]
    #[case::keeps_each_words_casing("User Underscore ID", "User_ID")]
    #[case::symbol_word_in_capitals("user UNDERSCORE id", "user_id")]
    #[case::brackets_around_the_join("(user underscore id)", "(user_id)")]
    #[case::extra_spaces_around_the_symbol("user   underscore  id", "user_id")]
    #[case::spacing_elsewhere_is_kept("say  user underscore id  now", "say  user_id  now")]
    #[case::non_ascii_word("café underscore menu", "café_menu")]
    #[case::small_word_neighbor("to underscore string", "to_string")]
    // Accepted risk: a symbol word in ordinary prose still joins.
    #[case::symbol_word_in_prose("add a hyphen here", "add a-here")]
    fn joins(#[case] heard: &str, #[case] typed: &str) {
        assert_eq!(applied(heard), typed);
    }

    #[rstest]
    #[case::empty("")]
    #[case::symbol_word_first("dot com")]
    #[case::symbol_word_last("user underscore")]
    #[case::dash_is_a_word("aria dash kit dash test")]
    #[case::point_is_a_word("about 2 point 5 seconds")]
    #[case::comma_before_symbol("user, underscore id")]
    #[case::comma_after_symbol("user underscore, id")]
    #[case::bracket_before_right_word("user underscore (id)")]
    #[case::repeated_symbol_word("x underscore underscore y")]
    #[case::apostrophe_in_word("user's underscore id")]
    #[case::symbol_inside_longer_word("hyphenated underscored dotted")]
    #[case::line_break_between_words("user\nunderscore id")]
    fn leaves_unchanged(#[case] heard: &str) {
        assert_eq!(applied(heard), heard);
    }

    #[rstest]
    #[case::vocabulary_keeps_its_spelling("open cargo dot toml", "open Cargo.toml")]
    #[case::match_keeps_declared_casing("kit dash pro hyphen test", "Kit-Pro-test")]
    #[case::symbols_beside_a_match("aria-stt and user underscore id", "aria-stt and user_id")]
    fn joins_through_vocabulary(#[case] heard: &str, #[case] typed: &str) {
        let vocabulary = Vocabulary::new(["aria-stt", "Cargo.toml", "Kit-Pro"]).unwrap();
        let corrected = vocabulary.correct_spoken(heard, "");
        assert_eq!(corrected.as_deref().unwrap_or(heard), typed);
    }

    #[rstest]
    #[case::underscore("call user", "underscore id", Some("_id"))]
    #[case::chain("max", "underscore retry underscore count", Some("_retry_count"))]
    #[case::dot_after_a_digit("2", "dot 5", Some(".5"))]
    #[case::hyphen_before_a_digit("3", "hyphen 5", None)]
    #[case::nothing_typed("", "underscore id", None)]
    #[case::punctuation_typed("user,", "underscore id", None)]
    fn opens_onto_the_word_typed_before(
        #[case] before: &str,
        #[case] heard: &str,
        #[case] typed: Option<&str>,
    ) {
        assert_eq!(
            Vocabulary::default()
                .correct_spoken(heard, before)
                .as_deref(),
            typed
        );
    }

    #[test]
    fn personal_rules_get_what_no_join_claimed() {
        let personal = PersonalRules::new(vec![], vec![("teh".into(), "the".into())]);
        let vocabulary = Vocabulary::compile(&[], personal).unwrap();
        assert_eq!(
            vocabulary
                .correct_spoken("teh user underscore id", "")
                .as_deref(),
            Some("the user_id")
        );
    }

    #[test]
    fn term_correction_never_joins() {
        let vocabulary = Vocabulary::new(["aria-stt"]).unwrap();
        assert_eq!(vocabulary.correct("user underscore id"), None);
    }

    #[test]
    fn a_cancelled_layer_stops_with_the_text_unchanged() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut text = "user underscore id".to_owned();
        assert!(JoinSymbols.apply(&mut text, &cancel).is_err());
        assert_eq!(text, "user underscore id");
    }
}
