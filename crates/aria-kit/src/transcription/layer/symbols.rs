use std::borrow::Cow;

use super::claim::{Claimed, Word};
use super::symbol_words::{is_symbol, symbol};

/// Claims each run of words that spoken symbols join as one word, as in
/// "user underscore id" → `user_id`. Two words that both have a joined
/// form, as numbers do, join in that form.
pub(super) fn claim<'a>(text: &str, claimed: &mut Claimed<'a>) {
    let words = claimed.take_words();
    let links = links(text, &words);
    let in_joined_form = |index: usize| {
        let pair = |link: usize| {
            links[link].is_some()
                && words[link - 1].joined.is_some()
                && words[link + 1].joined.is_some()
        };
        (index >= 2 && pair(index - 1)) || (index + 2 < words.len() && pair(index + 1))
    };
    let word_typed = |index: usize| match &words[index].joined {
        Some(joined) if in_joined_form(index) => joined.as_ref(),
        _ => words[index].typed(text),
    };
    let mut index = 0;
    while index < words.len() {
        let mut last = index;
        while links.get(last + 1).is_some_and(Option::is_some) {
            last += 2;
        }
        if last == index {
            claimed.push(words[index].clone());
            index += 1;
            continue;
        }
        let mut typed = word_typed(index).to_owned();
        for link in (index + 1..last).step_by(2) {
            typed.extend(links[link]);
            typed.push_str(word_typed(link + 1));
        }
        let span = words[index].span.start..words[last].span.end;
        claimed.push(Word::claimed(span, Cow::Owned(typed)));
        index = last + 1;
    }
}

/// Defers the symbol words at the edge of the deferred words that may
/// still link, as "underscore" in "user underscore". Each one defers the
/// word before it too when that word has a joined form, as "two" in "two
/// dot": it can't be typed alone yet. When the word before can join, it
/// defers the same words whether the cut falls before the symbol word or
/// after it.
///
/// A deferred symbol word whose left word can't join defers that word as
/// well: once typed, its words would read as if they could.
pub(super) fn defer(text: &str, claimed: &mut Claimed<'_>) {
    while let Some(at) = symbol_to_defer(text, claimed.words(), claimed.deferred_from()) {
        claimed.defer_at(at);
    }
}

/// Where the symbol word at the edge of the words from `deferred_from` on
/// defers the text from, when it must: the first deferred word, else the
/// last word before them.
fn symbol_to_defer(text: &str, words: &[Word<'_>], deferred_from: usize) -> Option<usize> {
    let first = words.partition_point(|word| word.span.start < deferred_from);
    let at_cut = words
        .get(first)
        .is_some_and(|word| unclaimed_symbol(text, word).is_some());
    let index = if at_cut { first } else { first.checked_sub(1)? };
    unclaimed_symbol(text, &words[index])?;
    let Some(left) = index.checked_sub(1) else {
        return (!at_cut).then(|| words[index].span.start);
    };
    let joins = joins_after(text, words, left);
    let defers_left = match joins {
        false => at_cut,
        true => words[left].joined.is_some(),
    };
    if defers_left {
        return Some(words[left].span.start);
    }
    (joins && !at_cut).then(|| words[index].span.start)
}

/// The symbol each word becomes when it links its two neighbors.
fn links(text: &str, words: &[Word<'_>]) -> Vec<Option<char>> {
    let mut links = vec![None; words.len()];
    for index in 1..words.len().saturating_sub(1) {
        let Some(character) = unclaimed_symbol(text, &words[index]) else {
            continue;
        };
        let (left, right) = (&words[index - 1], &words[index + 1]);
        // Each side types its joined form only when both sides have one.
        let joined = left.joined.is_some() && right.joined.is_some();
        let form = |word| {
            if joined {
                in_join(text, word)
            } else {
                word.typed(text)
            }
        };
        let breaks = line_break(text, words, index - 1) || line_break(text, words, index);
        if !breaks && joinable_before_a_symbol(form(left)) && joinable_after_a_symbol(form(right)) {
            links[index] = Some(character);
        }
    }
    links
}

/// The symbol `word` names, when no step claimed it.
fn unclaimed_symbol(text: &str, word: &Word<'_>) -> Option<char> {
    match word.claim {
        Some(_) => None,
        None => symbol(word.typed(text)),
    }
}

/// Whether the word at `left` may join through the symbol word after it,
/// whatever comes next: no line break between them, and the word can sit
/// in a join.
fn joins_after(text: &str, words: &[Word<'_>], left: usize) -> bool {
    !line_break(text, words, left) && joinable_before_a_symbol(in_join(text, &words[left]))
}

/// Whether a line break separates the word at `index` from the next.
fn line_break(text: &str, words: &[Word<'_>], index: usize) -> bool {
    text[words[index].span.end..words[index + 1].span.start].contains('\n')
}

/// What `word` types in a join beside another word with a joined form:
/// "one four" types `14`.
fn in_join<'s>(text: &'s str, word: &'s Word<'_>) -> &'s str {
    word.joined.as_deref().unwrap_or_else(|| word.typed(text))
}

/// Whether `word` can sit before a symbol in a join, punctuation before it
/// aside.
fn joinable_before_a_symbol(word: &str) -> bool {
    is_joinable(word.trim_start_matches(|c: char| c.is_ascii_punctuation()))
}

/// Whether `word` can sit after a symbol in a join, punctuation after it
/// aside.
fn joinable_after_a_symbol(word: &str) -> bool {
    is_joinable(word.trim_end_matches(|c: char| c.is_ascii_punctuation()))
}

fn is_joinable(word: &str) -> bool {
    word.starts_with(char::is_alphanumeric)
        && word.ends_with(char::is_alphanumeric)
        && word.chars().all(|c| c.is_alphanumeric() || is_symbol(c))
        && symbol(word).is_none()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::vocabulary::PersonalRules;
    use super::super::{JoinSymbols, Layer, Vocabulary};
    use crate::cancel::CancelToken;

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
    #[case::slash_between_words("src slash main", "src/main")]
    #[case::backslash_between_words("c backslash windows", "c\\windows")]
    #[case::joined_slash_as_neighbor("src/main slash lib", "src/main/lib")]
    #[case::small_word_neighbor("to underscore string", "to_string")]
    // Accepted risk: a symbol word in ordinary prose still joins.
    #[case::symbol_word_in_prose("add a hyphen here", "add a-here")]
    #[case::slash_in_prose("read and slash or write", "read and/or write")]
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
    #[case::match_spoken_with_a_slash("run ci slash cd", "run CI/CD")]
    fn joins_through_vocabulary(#[case] heard: &str, #[case] typed: &str) {
        let vocabulary = Vocabulary::new(["aria-stt", "Cargo.toml", "Kit-Pro", "CI/CD"]).unwrap();
        let corrected = vocabulary
            .correct_spoken(heard, "")
            .map(|corrected| corrected.text);
        assert_eq!(corrected.as_deref().unwrap_or(heard), typed);
    }

    #[rstest]
    #[case::underscore("call user", "underscore id", Some(("_id", true)))]
    #[case::chain("max", "underscore retry underscore count", Some(("_retry_count", true)))]
    #[case::dot_after_a_digit("2", "dot 5", Some((".5", true)))]
    #[case::hyphen_before_a_digit("3", "hyphen 5", Some(("-5", true)))]
    #[case::slash_before_a_digit("24", "slash 7", Some(("/7", true)))]
    #[case::numbers_around_a_slash("", "one slash two", Some(("1/2", false)))]
    #[case::hyphen_before_a_number_word("utf", "hyphen sixteen", Some(("-16", true)))]
    #[case::hyphen_before_a_long_number("version", "hyphen twenty five", Some(("-25", true)))]
    #[case::hyphen_after_a_symbol_word("the end, underscore", "hyphen twenty", Some(("hyphen 20", false)))]
    #[case::digits_after_digits_typed("30", "dot one zero zero", Some((".100", true)))]
    #[case::digits_after_an_opening_symbol("python", "underscore three dot twelve", Some(("_3.12", true)))]
    #[case::space_typed_after("src ", "slash main", None)]
    #[case::line_break_typed_after("src\n", "slash main", None)]
    #[case::nothing_typed("", "underscore id", None)]
    #[case::punctuation_typed("user,", "underscore id", None)]
    fn opens_onto_the_word_typed_before(
        #[case] before: &str,
        #[case] heard: &str,
        #[case] typed: Option<(&str, bool)>,
    ) {
        let corrected = Vocabulary::default().correct_spoken(heard, before);
        assert_eq!(
            corrected
                .as_ref()
                .map(|corrected| (corrected.text.as_str(), corrected.joins_before)),
            typed
        );
    }

    #[test]
    fn a_term_never_reads_across_the_word_typed_before() {
        let vocabulary = Vocabulary::new(["New York"]).unwrap();
        assert_eq!(vocabulary.correct_spoken("york", "new"), None);
    }

    #[test]
    fn personal_rules_get_what_no_join_claimed() {
        let personal = PersonalRules::new(vec![], vec![("teh".into(), "the".into())]);
        let vocabulary = Vocabulary::compile(&[], personal).unwrap();
        assert_eq!(
            vocabulary
                .correct_spoken("teh user underscore id", "")
                .map(|corrected| corrected.text)
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
