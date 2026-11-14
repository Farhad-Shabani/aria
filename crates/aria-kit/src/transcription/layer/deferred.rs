use super::super::Caret;
use super::super::sentences::ends_sentence;
use super::{Vocabulary, numbers, symbols};

const CLOSING_MARKS: [char; 3] = ['.', '?', '!'];

/// More held words than this are typed rather than held, which bounds the
/// delay.
const MAX_HELD_WORDS: usize = 8;

/// Holds a segment's last words while the next segment may still change
/// them: a spoken symbol it may join, or a number it may go on.
///
/// Streaming can split "user underscore id" or "twenty one" across
/// segments. Correct what [`hold`](Self::hold) releases with
/// [`Vocabulary::correct_spoken`], after the text already typed.
///
/// ```
/// use aria_kit::transcription::{DeferredWords, Vocabulary};
///
/// let vocabulary = Vocabulary::default();
/// let mut words = DeferredWords::default();
/// assert_eq!(words.hold("user underscore", None, &vocabulary).text, "user");
/// let next = words.hold("id", None, &vocabulary).text;
/// assert_eq!(next, "underscore id");
/// let corrected = vocabulary.correct_spoken(&next, "user");
/// assert_eq!(corrected.as_deref(), Some("_id"));
/// assert!(words.finish().is_none());
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeferredWords {
    held: Option<Held>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    words: String,
    /// Whether a sentence ended before the first held word.
    sentence_ended_before: Option<bool>,
}

/// Text a stream can type now, once [`DeferredWords`] has taken its share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unheld {
    pub text: String,
    /// Whether a sentence ended before `text`. Words held from an earlier
    /// segment keep that segment's reading.
    pub sentence_ended_before: Option<bool>,
}

impl DeferredWords {
    /// Releases the held words and `text`, minus the words at its end that
    /// the next segment may still change. Words are held only when
    /// `vocabulary` corrects them apart from the rest as it would together,
    /// so a term or personal rule they're part of still matches. The held
    /// words drop their closing mark only when `sentence_ended_before` is
    /// `Some(false)`, as [`SentenceEnds`](super::super::SentenceEnds) does.
    #[must_use = "held words are owed to the transcript"]
    pub fn hold(
        &mut self,
        text: &str,
        sentence_ended_before: Option<bool>,
        vocabulary: &Vocabulary,
    ) -> Unheld {
        // A segment with no words, as when it was all fillers, changes nothing held.
        if text.trim().is_empty() {
            return Unheld {
                text: String::new(),
                sentence_ended_before,
            };
        }
        let mut unheld = self.release(text, sentence_ended_before);
        let (rest, held) = split_last_words(&unheld.text, held_words(&unheld.text));
        if held.is_empty() || splitting_changes(vocabulary, rest, held) {
            return unheld;
        }
        self.held = Some(Held {
            words: held.to_owned(),
            sentence_ended_before: match rest {
                "" => unheld.sentence_ended_before,
                rest => Some(ends_sentence(rest)),
            },
        });
        unheld.text.truncate(rest.len());
        unheld
    }

    /// Releases the held words once nothing follows.
    #[must_use = "held words are owed to the transcript"]
    pub fn finish(&mut self) -> Option<Unheld> {
        self.held.take().map(|held| Unheld {
            text: held.words,
            sentence_ended_before: held.sentence_ended_before,
        })
    }

    /// The held words, then `text`, read as the held words were.
    fn release(&mut self, text: &str, sentence_ended_before: Option<bool>) -> Unheld {
        let Some(held) = self.held.take() else {
            return Unheld {
                text: text.to_owned(),
                sentence_ended_before,
            };
        };
        let words = match sentence_ended_before {
            Some(false) => held.words.trim_end_matches(CLOSING_MARKS),
            _ => &held.words,
        };
        Unheld {
            text: joined(words, text),
            sentence_ended_before: held.sentence_ended_before,
        }
    }
}

fn joined(held: &str, text: &str) -> String {
    let mut joined = held.to_owned();
    joined.push_str(&Caret::between(held, "").insert_open(text));
    joined
}

/// How many words `text` ends on that the next segment may still change.
/// Each kind of word answers for itself, and a symbol may sit between
/// numbers, so the count grows until neither adds to it.
fn held_words(text: &str) -> usize {
    let mut words: Vec<&str> = text
        .split_whitespace()
        .rev()
        .take(MAX_HELD_WORDS + 2)
        .collect();
    words.reverse();
    let Some(last) = words.last_mut() else {
        return 0;
    };
    *last = last.trim_end_matches(CLOSING_MARKS);
    let mut held = 0;
    while held <= MAX_HELD_WORDS {
        let before = &words[..words.len() - held];
        let more = numbers::open_tail(before).max(symbols::open_tail(before));
        if more == 0 {
            return held;
        }
        held += more;
    }
    0
}

/// `text` before its last `count` words, and those words.
fn split_last_words(text: &str, count: usize) -> (&str, &str) {
    let mut from = text.len();
    for _ in 0..count {
        let before = text[..from].trim_end();
        from = before
            .char_indices()
            .rfind(|(_, c)| c.is_whitespace())
            .map_or(0, |(at, space)| at + space.len_utf8());
    }
    (text[..from].trim_end(), text[from..].trim_end())
}

/// Whether `vocabulary` corrects `rest` then `held` differently from the
/// two together.
fn splitting_changes(vocabulary: &Vocabulary, rest: &str, held: &str) -> bool {
    let corrected = |text: &str, before: &str| {
        vocabulary
            .correct_spoken(text, before)
            .unwrap_or_else(|| text.to_owned())
    };
    let together = corrected(&joined(rest, held), "");
    let rest = corrected(rest, "");
    let apart = joined(&rest, &corrected(held, &rest));
    !together.split_whitespace().eq(apart.split_whitespace())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::vocabulary::PersonalRules;
    use super::*;

    /// Each piece is one segment, corrected after what the stream typed.
    fn streamed(pieces: &[(&str, Option<bool>)], vocabulary: &Vocabulary) -> String {
        let mut words = DeferredWords::default();
        let mut typed = String::new();
        let put = |text: &str, typed: &mut String| {
            let corrected = vocabulary.correct_spoken(text, typed);
            let corrected = corrected.as_deref().unwrap_or(text);
            typed.push_str(&Caret::between(typed, "").insert_open(corrected));
        };
        for (piece, sentence_ended_before) in pieces {
            let unheld = words.hold(piece, *sentence_ended_before, vocabulary);
            put(&unheld.text, &mut typed);
        }
        if let Some(held) = words.finish() {
            put(&held.text, &mut typed);
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
        assert_eq!(streamed(pieces, &Vocabulary::default()), typed);
    }

    #[rstest]
    #[case::compound(&[("twenty", None), ("one", None), ("apples", None)], "21 apples")]
    #[case::decimal(&[("two", None), ("point", None), ("five", None)], "2.5")]
    #[case::hundreds_with_and(&[("one hundred and", None), ("five", None)], "105")]
    #[case::unit_after_a_number(&[("costs five", None), ("dollars", None)], "costs $5")]
    #[case::press_ends_on_a_number(&[("it is twenty", None), ("one.", Some(false))], "it is 21.")]
    #[case::symbol_before_a_number(&[("user underscore twenty", None), ("one", None)], "user_21")]
    #[case::mark_after_a_held_number(&[("Twenty.", None), ("One apples.", Some(false))], "21 apples.")]
    #[case::cents_after_dollars(&[("it costs five dollars", None), ("and fifty cents", None)], "it costs $5.50")]
    #[case::cents_right_after_dollars(&[("five dollars", None), ("fifty", None), ("cents", None)], "$5.50")]
    #[case::bracket_before_a_number(&[("(twenty", None), ("one)", None)], "(21)")]
    #[case::digit_words_around_a_symbol(&[("version two", None), ("dot", None), ("five", None)], "version 2.5")]
    fn writes_numbers_across_segments(
        #[case] pieces: &[(&str, Option<bool>)],
        #[case] typed: &str,
    ) {
        assert_eq!(streamed(pieces, &Vocabulary::default()), typed);
    }

    #[rstest]
    #[case::ordinary_words(&[("turn", None), ("left now", None)], "turn left now")]
    #[case::press_ends_on_a_symbol(&[("user", None), ("underscore.", None)], "user underscore.")]
    #[case::sentence_ended_after_the_symbol(&[("the dot.", None), ("Then we rest.", Some(true))], "the dot. Then we rest.")]
    #[case::unread_cut_after_the_symbol(&[("the dot.", None), ("Then we rest.", None)], "the dot. Then we rest.")]
    #[case::no_word_before(&[("hyphen", None), ("of the word", None)], "hyphen of the word")]
    #[case::number_range(&[("3", None), ("hyphen", None), ("5", None)], "3 hyphen 5")]
    #[case::digit_word_in_prose(&[("no", None), ("one", None), ("knows", None)], "no one knows")]
    #[case::sentence_ended_after_a_number(&[("twenty.", None), ("One more.", Some(true))], "20. One more.")]
    fn leaves_unjoined_across_segments(
        #[case] pieces: &[(&str, Option<bool>)],
        #[case] typed: &str,
    ) {
        assert_eq!(streamed(pieces, &Vocabulary::default()), typed);
    }

    #[test]
    fn a_whole_held_segment_keeps_its_reading() {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        assert_eq!(words.hold("Hello.", None, &vocabulary).text, "Hello.");
        assert_eq!(words.hold("Twenty", Some(true), &vocabulary).text, "");
        let unheld = words.hold("one apples.", Some(false), &vocabulary);
        assert_eq!(unheld.text, "Twenty one apples.");
        assert_eq!(unheld.sentence_ended_before, Some(true));
    }

    #[test]
    fn words_held_after_a_sentence_end_read_it() {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        assert_eq!(
            words.hold("It is. Twenty", Some(false), &vocabulary).text,
            "It is."
        );
        let unheld = words.hold("apples.", Some(false), &vocabulary);
        assert_eq!(unheld.text, "Twenty apples.");
        assert_eq!(unheld.sentence_ended_before, Some(true));
    }

    #[test]
    fn words_a_personal_rule_uses_are_released() {
        let personal = PersonalRules::new(vec!["db2".into()], vec![]);
        let vocabulary = Vocabulary::compile(&[], personal).unwrap();
        let mut words = DeferredWords::default();
        assert_eq!(
            words.hold("run d b two", None, &vocabulary).text,
            "run d b two"
        );
        assert!(words.finish().is_none());
    }

    #[test]
    fn words_a_term_keeps_as_heard_are_released() {
        let vocabulary = Vocabulary::new(["Area Fifty One"]).unwrap();
        let mut words = DeferredWords::default();
        let heard = "visit area fifty one";
        assert_eq!(words.hold(heard, None, &vocabulary).text, heard);
        assert!(words.finish().is_none());
    }

    #[test]
    fn a_segment_with_no_words_keeps_the_held_mark() {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        assert_eq!(
            words.hold("I have twenty.", None, &vocabulary).text,
            "I have"
        );
        assert_eq!(words.hold("", Some(false), &vocabulary).text, "");
        assert_eq!(words.finish().unwrap().text, "twenty.");
    }

    /// Every way to cut each sentence into two or three segments.
    #[rstest]
    #[case("it costs twenty five dollars and fifty cents today")]
    #[case("pay nine dollars and one cent now")]
    #[case("version two point five is out")]
    #[case("call user underscore id twenty one times")]
    #[case("max underscore retry underscore count is one hundred and five")]
    #[case("no one knows the twenty second rule")]
    #[case("about two million people or five percent")]
    #[case("file dot twenty one dot txt")]
    #[case("one two three go")]
    #[case("a hundred and fifty people came")]
    #[case("zero point five percent of five dollars")]
    fn any_split_types_what_the_whole_text_would(#[case] heard: &str) {
        let vocabulary = Vocabulary::default();
        let whole = vocabulary.correct_spoken(heard, "");
        let words: Vec<&str> = heard.split(' ').collect();
        for first in 1..words.len() {
            for second in first..words.len() {
                let pieces = [&words[..first], &words[first..second], &words[second..]]
                    .map(|piece| (piece.join(" "), None));
                let pieces: Vec<(&str, Option<bool>)> = pieces
                    .iter()
                    .filter(|(piece, _)| !piece.is_empty())
                    .map(|(piece, ended)| (piece.as_str(), *ended))
                    .collect();
                assert_eq!(
                    streamed(&pieces, &vocabulary),
                    whole.as_deref().unwrap_or(heard),
                    "{pieces:?}"
                );
            }
        }
    }

    #[test]
    fn a_long_number_is_typed_rather_than_held() {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        let nine = "one two three four five six seven eight nine";
        assert_eq!(words.hold(nine, None, &vocabulary).text, nine);
        assert!(words.finish().is_none());
        let eight = "one two three four five six seven eight";
        assert_eq!(words.hold(eight, None, &vocabulary).text, "");
    }
}
