use super::super::Caret;
use super::super::sentences::{ends_sentence, goes_on, without_closing_mark};
use super::{Vocabulary, Writing, symbols};

/// More held words than this are typed rather than held, which bounds the
/// delay.
const MAX_HELD_WORDS: usize = 8;

/// Holds a segment's last words while the next segment may still change
/// them: a spoken symbol it may join, a number it may go on, a name it may
/// complete, a fragment it may merge into a listed word, or a quoted
/// assignment its closing quote may complete.
///
/// Streaming can split "user underscore id", "twenty one" or "aria hyphen
/// kit" across segments. Correct what [`hold`](Self::hold) releases with
/// [`Vocabulary::correct_spoken_after`], after the text already typed.
///
/// Who decides what:
///
/// - Each step of [`Vocabulary::correct_spoken`] decides what it claims,
///   which of its words more speech may still change, and which spans a cut
///   must not split. A step defers its words as it claims them or in
///   `Step::defer`, and the hold follows it.
/// - The vocabulary defers the start of a name typed in its own spelling.
/// - `DeferredWords` holds the deferred words a segment ends on, at most
///   `MAX_HELD_WORDS`, and releases them ahead of the next segment.
/// - `sentences::goes_on` decides whether a closing mark drops, for a
///   segment's end and for held words alike.
/// - The caller decides when held words go out with nothing after them,
///   with [`finish`](Self::finish).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct DeferredWords {
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
pub(super) struct Unheld {
    pub(super) text: String,
    /// Whether a sentence ended before `text`. Words held from an earlier
    /// segment keep that segment's reading.
    pub(super) sentence_ended_before: Option<bool>,
}

impl DeferredWords {
    /// Releases the held words and `text`, minus the words at its end that
    /// the next segment may still change. The steps of
    /// [`Vocabulary::correct_spoken`] say which those are: each defers its
    /// own words. The held words keep their closing mark unless
    /// `sentence_ended_before` says the sentence goes on.
    #[must_use = "held words are owed to the transcript"]
    pub(super) fn hold(
        &mut self,
        text: &str,
        sentence_ended_before: Option<bool>,
        vocabulary: &Vocabulary,
        writing: Writing,
    ) -> Unheld {
        // A segment with no words, as when it was all fillers, changes nothing held.
        if text.trim().is_empty() {
            return Unheld {
                text: String::new(),
                sentence_ended_before,
            };
        }
        let mut unheld = self.release(text, sentence_ended_before);
        let unmarked = without_closing_mark(&unheld.text);
        let from = vocabulary.spoken_deferred_from(unmarked, MAX_HELD_WORDS, writing);
        let (rest, held) = unheld.text.split_at(from);
        if from == unmarked.len() || held.split_whitespace().count() > MAX_HELD_WORDS {
            return unheld;
        }
        let rest = rest.trim_end();
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

    pub(super) fn is_holding(&self) -> bool {
        self.held.is_some()
    }

    /// Releases the held words after a pause, unless they end on a named
    /// slash or backslash. Those wait for the next word, as "dot dot forward
    /// slash" waits to type `../src`.
    #[must_use = "held words are owed to the transcript"]
    pub(super) fn release_paused(&mut self) -> Option<Unheld> {
        let waits = self
            .held
            .as_ref()
            .is_some_and(|held| symbols::ends_on_named_slash(without_closing_mark(&held.words)));
        if waits {
            return None;
        }
        self.finish()
    }

    /// Releases the held words once nothing follows.
    #[must_use = "held words are owed to the transcript"]
    pub(super) fn finish(&mut self) -> Option<Unheld> {
        self.held.take().map(|held| Unheld {
            text: held.words,
            sentence_ended_before: held.sentence_ended_before,
        })
    }

    /// The held words, then `text`. The result keeps the held words'
    /// sentence reading. The held words drop their closing mark when `text`
    /// goes on with their sentence.
    fn release(&mut self, text: &str, sentence_ended_before: Option<bool>) -> Unheld {
        let Some(held) = self.held.take() else {
            return Unheld {
                text: text.to_owned(),
                sentence_ended_before,
            };
        };
        let words = if goes_on(sentence_ended_before) {
            without_closing_mark(&held.words)
        } else {
            &held.words
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

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::super::Attachment;
    use super::super::correction::Preceding;
    use super::super::corrector::tests::{crowded, listing, names, streamed};
    use super::*;

    #[test]
    fn a_held_symbol_joins_the_word_typed_before_it() {
        let vocabulary = Vocabulary::default();
        let writing = Writing::Commands;
        let mut words = DeferredWords::default();
        assert_eq!(
            words
                .hold("user underscore", None, &vocabulary, writing)
                .text,
            "user"
        );
        let next = words.hold("id", None, &vocabulary, writing).text;
        assert_eq!(next, "underscore id");
        let before = Preceding::Text("user");
        let corrected = vocabulary
            .correct_spoken_after(&next, before, writing)
            .unwrap();
        assert_eq!(corrected.text, "_id");
        // Type it with no space after "user".
        assert_eq!(corrected.attachment, Attachment::Joined);
        assert!(words.finish().is_none());
    }

    #[test]
    fn a_whole_held_segment_keeps_its_reading() {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        assert_eq!(
            words
                .hold("Hello.", None, &vocabulary, Writing::Commands)
                .text,
            "Hello."
        );
        assert_eq!(
            words
                .hold("Twenty", Some(true), &vocabulary, Writing::Commands)
                .text,
            ""
        );
        let unheld = words.hold("one apples.", Some(false), &vocabulary, Writing::Commands);
        assert_eq!(unheld.text, "Twenty one apples.");
        assert_eq!(unheld.sentence_ended_before, Some(true));
    }

    #[test]
    fn words_held_after_a_sentence_end_read_it() {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        assert_eq!(
            words
                .hold("It is. Twenty", Some(false), &vocabulary, Writing::Commands)
                .text,
            "It is."
        );
        let unheld = words.hold("apples.", Some(false), &vocabulary, Writing::Commands);
        assert_eq!(unheld.text, "Twenty apples.");
        assert_eq!(unheld.sentence_ended_before, Some(true));
    }

    #[test]
    fn words_a_personal_rule_uses_are_released() {
        let vocabulary = listing(&["db2"]);
        let mut words = DeferredWords::default();
        assert_eq!(
            words
                .hold("run d b two", None, &vocabulary, Writing::Commands)
                .text,
            "run d b two"
        );
        assert!(words.finish().is_none());
    }

    #[test]
    fn words_a_term_keeps_as_heard_are_released() {
        let vocabulary = Vocabulary::new(["Area Fifty One"]).unwrap();
        let mut words = DeferredWords::default();
        let heard = "visit area fifty one";
        assert_eq!(
            words.hold(heard, None, &vocabulary, Writing::Commands).text,
            heard
        );
        assert!(words.finish().is_none());
    }

    #[test]
    fn a_segment_with_no_words_keeps_the_held_mark() {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        assert_eq!(
            words
                .hold("I have twenty.", None, &vocabulary, Writing::Commands)
                .text,
            "I have"
        );
        assert_eq!(
            words
                .hold("", Some(false), &vocabulary, Writing::Commands)
                .text,
            ""
        );
        assert_eq!(words.finish().unwrap().text, "twenty.");
    }

    #[test]
    fn only_fragments_that_can_reach_a_listed_word_wait() {
        let v = listing(&["PostgreSQL"]);
        for name in ["Pablo", "Priya", "PyTorch", "postgr2"] {
            assert_eq!(
                DeferredWords::default()
                    .hold(name, None, &v, Writing::Commands)
                    .text,
                name
            );
        }
        // No ASCII word can supply a listed word's full-width digit.
        let wide = listing(&["Nvidia\u{ff12}"]);
        assert_eq!(
            DeferredWords::default()
                .hold("nvid", None, &wide, Writing::Commands)
                .text,
            "nvid"
        );
        for (start, end) in [("use postgre", "sql"), ("use pyst", "gresql")] {
            assert_eq!(
                streamed(&[(start, None), (end, None)], &v, Writing::Commands),
                "use PostgreSQL"
            );
        }
    }

    #[test]
    fn an_unfinished_quote_obeys_the_stream_hold_limit() {
        let vocabulary = listing(&["ID"]);
        let mut words = DeferredWords::default();
        assert_eq!(
            words
                .hold("set x='server", None, &vocabulary, Writing::Commands)
                .text,
            "set"
        );
        let released = words.hold(
            "one two three four five six seven eight nine",
            None,
            &vocabulary,
            Writing::Commands,
        );
        assert!(released.text.starts_with("x='server one two"));
    }

    #[rstest]
    #[case::trailing_symbol("user underscore", "user ")]
    #[case::symbol_after_punctuation("user, underscore", "user, underscore")]
    #[case::slash_after_an_english_word("run forward slash", "run ")]
    #[case::number_before_a_symbol("version two dot", "version ")]
    #[case::cents_may_follow("five dollars and", "")]
    #[case::number_word_in_prose("no one", "no ")]
    #[case::number_before_a_word("twenty one apples", "twenty one apples")]
    fn each_step_says_which_words_it_defers(#[case] heard: &str, #[case] typed: &str) {
        let from = Vocabulary::default().spoken_deferred_from(
            without_closing_mark(heard),
            MAX_HELD_WORDS,
            Writing::Commands,
        );
        assert_eq!(&heard[..from.min(heard.len())], typed);
    }

    #[test]
    fn a_pause_releases_held_words_but_a_slash_waits_for_its_word() {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        let typed = words.hold(
            "open src forward slash",
            None,
            &vocabulary,
            Writing::Commands,
        );
        assert_eq!(typed.text, "open src");
        assert!(words.release_paused().is_none());
        let next = words.hold("main", None, &vocabulary, Writing::Commands);
        assert_eq!(next.text, "forward slash main");

        let mut words = DeferredWords::default();
        let typed = words.hold("version two dot", None, &vocabulary, Writing::Commands);
        assert_eq!(typed.text, "version");
        let released = words.release_paused().map(|unheld| unheld.text);
        assert_eq!(released.as_deref(), Some("two dot"));
        assert!(!words.is_holding());
    }

    #[rstest]
    #[case::a_relative_path("cd dot dot forward slash", "src", "cd", "dot dot forward slash src")]
    #[case::a_number(
        "say two forward slash",
        "three apples",
        "say",
        "two forward slash three apples"
    )]
    #[case::a_provisional_mark(
        "cd dot dot forward slash.",
        "src",
        "cd",
        "dot dot forward slash src"
    )]
    #[case::a_provisional_mark_on_a_backslash(
        "open src back slash.",
        "main",
        "open src",
        "back slash main"
    )]
    fn a_pause_keeps_the_words_a_slash_may_still_change(
        #[case] said: &str,
        #[case] after_the_pause: &str,
        #[case] typed: &str,
        #[case] released: &str,
    ) {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        let first = words.hold(said, None, &vocabulary, Writing::Commands);
        assert_eq!(first.text, typed);
        assert!(words.release_paused().is_none());
        let next = words.hold(after_the_pause, Some(false), &vocabulary, Writing::Commands);
        assert_eq!(next.text, released);
    }

    #[test]
    fn a_name_start_past_the_word_bound_is_typed_and_a_shorter_one_held() {
        let vocabulary = Vocabulary::new(["a-b-c-d-e-f", "b-c", "d-e-g"]).unwrap();
        let heard = "x a dash b dash c dash d dash e";
        let from = vocabulary.spoken_deferred_from(heard, MAX_HELD_WORDS, Writing::Commands);
        assert_eq!(&heard[..from], "x a dash b dash c dash ");
    }

    #[rstest]
    #[case::first_word("open aria", "open ")]
    #[case::two_words("open app delegate", "open ")]
    #[case::up_to_a_symbol("open aria dash", "open ")]
    #[case::whole_name("open aria dash kit", "open aria dash kit")]
    #[case::through_a_pair("open arya", "open ")]
    #[case::no_name_starts("open area", "open area")]
    #[case::ends_on_a_multibyte_letter("open café", "open café")]
    fn a_segment_holds_the_start_of_a_spelled_name(#[case] heard: &str, #[case] typed: &str) {
        let vocabulary = names();
        let from = vocabulary.spoken_deferred_from(heard, MAX_HELD_WORDS, Writing::Commands);
        assert_eq!(&heard[..from], typed);
    }

    #[test]
    fn the_start_of_a_name_typed_as_heard_is_released() {
        let vocabulary = Vocabulary::new(["New York City"]).unwrap();
        let mut words = DeferredWords::default();
        assert_eq!(
            words
                .hold("visit new york", None, &vocabulary, Writing::Commands)
                .text,
            "visit new york"
        );
        assert!(words.finish().is_none());
    }

    #[test]
    fn a_long_number_is_typed_rather_than_held() {
        let vocabulary = Vocabulary::default();
        let mut words = DeferredWords::default();
        let nine = "one two three four five six seven eight nine";
        assert_eq!(
            words.hold(nine, None, &vocabulary, Writing::Commands).text,
            nine
        );
        assert!(words.finish().is_none());
        let eight = "one two three four five six seven eight";
        assert_eq!(
            words.hold(eight, None, &vocabulary, Writing::Commands).text,
            ""
        );
    }

    #[test]
    fn deferring_stops_once_a_hold_would_type_the_words() {
        let vocabulary = crowded();
        let forms = "cd tilde dot tilde dot tilde dot tilde dot tilde dot tilde";
        assert_eq!(
            vocabulary.spoken_deferred_from(forms, usize::MAX, Writing::Commands),
            "cd ".len()
        );
        let from = vocabulary.spoken_deferred_from(forms, 2, Writing::Commands);
        assert!(from > "cd ".len() && forms[from..].split_whitespace().count() > 2);
    }
}
