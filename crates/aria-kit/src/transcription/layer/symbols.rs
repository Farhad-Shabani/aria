use crate::cancel::CancelToken;

use super::super::Caret;
use super::vocabulary::Word;
use super::{Layer, TranscriptionError, Vocabulary, check_cancelled, check_size};

const CLOSING_MARKS: [char; 3] = ['.', '?', '!'];

/// Joins the words around "underscore", "hyphen", or "dot", as in
/// "user underscore id" → `user_id`.
#[derive(Debug, Clone, Copy, Default)]
pub struct JoinSymbols;

impl Layer for JoinSymbols {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        let corrected = Vocabulary::default().correct_spoken_checked(text, "", &|bytes| {
            check_cancelled(cancel)?;
            check_size(bytes)
        })?;
        if let Some(corrected) = corrected {
            *text = corrected;
        }
        Ok(())
    }
}

impl Vocabulary {
    /// Corrects dictated `text` in one pass. Terms win, spoken symbols join
    /// the rest, and personal rules get what neither claimed. `before` is
    /// text already typed: a symbol `text` opens with joins its last word.
    pub fn correct_spoken(&self, text: &str, before: &str) -> Option<String> {
        self.correct_spoken_checked(text, before, &|_| Ok::<_, std::convert::Infallible>(()))
            .unwrap_or_else(|never| match never {})
    }

    pub(super) fn correct_spoken_checked<E>(
        &self,
        text: &str,
        before: &str,
        check: &impl Fn(usize) -> Result<(), E>,
    ) -> Result<Option<String>, E> {
        self.correct_with(text, before, joins, check)
    }
}

/// Holds a segment's last word while it is a symbol word, until the next
/// segment shows what it joins.
///
/// Streaming can split "user underscore id" across segments. Correct what
/// [`hold`](Self::hold) returns with [`Vocabulary::correct_spoken`], after the
/// text already typed.
///
/// ```
/// use aria_kit::transcription::{DeferredSymbols, Vocabulary};
///
/// let mut symbols = DeferredSymbols::default();
/// assert_eq!(symbols.hold("user underscore", None), "user");
/// let next = symbols.hold("id", None);
/// assert_eq!(next, "underscore id");
/// let corrected = Vocabulary::default().correct_spoken(&next, "user");
/// assert_eq!(corrected.as_deref(), Some("_id"));
/// assert_eq!(symbols.finish(), None);
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeferredSymbols {
    held: Option<String>,
}

impl DeferredSymbols {
    /// Returns the held word and `text`, minus a symbol word `text` ends on.
    /// The held word drops its closing mark only when `sentence_ended_before`
    /// is `Some(false)`, as [`SentenceEnds`](super::super::SentenceEnds) does.
    #[must_use = "held words are owed to the transcript"]
    pub fn hold(&mut self, text: &str, sentence_ended_before: Option<bool>) -> String {
        let mut text = match self.held.take() {
            Some(held) if sentence_ended_before == Some(false) => {
                joined(held.trim_end_matches(CLOSING_MARKS), text)
            }
            Some(held) => joined(&held, text),
            None => text.to_owned(),
        };
        let kept = text.trim_end();
        let (rest, last) = kept.rsplit_once(char::is_whitespace).unwrap_or(("", kept));
        if symbol(last.trim_end_matches(CLOSING_MARKS)).is_none() {
            return text;
        }
        self.held = Some(last.to_owned());
        text.truncate(rest.trim_end().len());
        text
    }

    /// Returns the held word once nothing follows.
    #[must_use = "held words are owed to the transcript"]
    pub fn finish(&mut self) -> Option<String> {
        self.held.take()
    }
}

fn joined(held: &str, text: &str) -> String {
    let mut joined = held.to_owned();
    joined.push_str(&Caret::between(held, "").insert_open(text));
    joined
}

fn symbol(word: &str) -> Option<char> {
    match word.to_lowercase().as_str() {
        "underscore" => Some('_'),
        "hyphen" => Some('-'),
        "dot" => Some('.'),
        _ => None,
    }
}

/// `before` is the last word already typed, the left neighbor of a symbol
/// that opens `text`.
fn joins(text: &str, words: &[Word<'_>], before: Option<&str>) -> Vec<Option<char>> {
    let mut joins = vec![None; words.len()];
    for index in 0..words.len().saturating_sub(1) {
        if words[index].term.is_some() {
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
        let left = left.trim_start_matches(|c: char| c.is_ascii_punctuation());
        let right = words[index + 1]
            .typed(text)
            .trim_end_matches(|c: char| c.is_ascii_punctuation());
        let gap_start = index.checked_sub(1).map_or(0, |left| words[left].span.end);
        let line_break = text[gap_start..words[index + 1].span.start].contains('\n');
        if line_break || !is_joinable(left) || !is_joinable(right) {
            continue;
        }
        // Typed text can't change, and a caret keeps "-5" apart from it.
        let opens_number =
            index == 0 && character == '-' && right.starts_with(|c: char| c.is_ascii_digit());
        if opens_number {
            continue;
        }
        joins[index] = Some(character);
    }
    joins
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

    /// Each piece is one segment, corrected after what the stream typed.
    fn streamed(pieces: &[(&str, Option<bool>)]) -> String {
        let mut symbols = DeferredSymbols::default();
        let mut typed = String::new();
        let put = |text: &str, typed: &mut String| {
            let corrected = Vocabulary::default().correct_spoken(text, typed);
            let corrected = corrected.as_deref().unwrap_or(text);
            typed.push_str(&Caret::between(typed, "").insert_open(corrected));
        };
        for (piece, sentence_ended_before) in pieces {
            let text = symbols.hold(piece, *sentence_ended_before);
            put(&text, &mut typed);
        }
        if let Some(held) = symbols.finish() {
            put(&held, &mut typed);
        }
        typed
    }

    #[rstest]
    #[case::underscore(&[("user", None), ("underscore", None), ("id", None)], "user_id")]
    #[case::keeps_casing(&[("User", None), ("underscore", None), ("ID", None)], "User_ID")]
    #[case::symbol_ends_a_segment(&[("user underscore", None), ("id and more", None)], "user_id and more")]
    #[case::symbol_opens_a_segment(&[("user", None), ("underscore id and more", None)], "user_id and more")]
    #[case::chain(&[("max underscore", None), ("retry underscore", None), ("count", None)], "max_retry_count")]
    #[case::after_a_hyphenated_word(&[("aria-stt", None), ("hyphen", None), ("test", None)], "aria-stt-test")]
    #[case::mark_after_the_symbol(&[("User underscore.", None), ("ID.", Some(false))], "User_ID.")]
    fn joins_across_segments(#[case] pieces: &[(&str, Option<bool>)], #[case] typed: &str) {
        assert_eq!(streamed(pieces), typed);
    }

    #[rstest]
    #[case::ordinary_words(&[("turn", None), ("left now", None)], "turn left now")]
    #[case::press_ends_on_a_symbol(&[("user", None), ("underscore.", None)], "user underscore.")]
    #[case::sentence_ended_after_the_symbol(&[("the dot.", None), ("Then we rest.", Some(true))], "the dot. Then we rest.")]
    #[case::unread_cut_after_the_symbol(&[("the dot.", None), ("Then we rest.", None)], "the dot. Then we rest.")]
    #[case::no_word_before(&[("hyphen", None), ("of the word", None)], "hyphen of the word")]
    #[case::number_range(&[("3", None), ("hyphen", None), ("5", None)], "3 hyphen 5")]
    fn leaves_unjoined_across_segments(
        #[case] pieces: &[(&str, Option<bool>)],
        #[case] typed: &str,
    ) {
        assert_eq!(streamed(pieces), typed);
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
