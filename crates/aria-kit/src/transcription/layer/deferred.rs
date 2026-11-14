use super::super::Caret;
use super::super::sentences::{ends_sentence, goes_on, without_closing_mark};
use super::Vocabulary;

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
/// Who decides what:
///
/// - Each step of [`Vocabulary::correct_spoken`] decides what it claims,
///   which of its words more speech may still change, and which spans a cut
///   must not split. A step defers its words as it claims them or in
///   `Step::defer`, and the hold follows it.
/// - `DeferredWords` holds the deferred words a segment ends on, at most
///   `MAX_HELD_WORDS`, and releases them ahead of the next segment.
/// - `sentences::goes_on` decides whether a closing mark drops, for a
///   segment's end and for held words alike.
/// - The caller decides when held words go out with nothing after them,
///   with [`finish`](Self::finish).
///
/// ```
/// use aria_kit::transcription::{DeferredWords, Vocabulary};
///
/// let vocabulary = Vocabulary::default();
/// let mut words = DeferredWords::default();
/// assert_eq!(words.hold("user underscore", None, &vocabulary).text, "user");
/// let next = words.hold("id", None, &vocabulary).text;
/// assert_eq!(next, "underscore id");
/// let corrected = vocabulary.correct_spoken(&next, "user").unwrap();
/// assert_eq!(corrected.text, "_id");
/// // Type it with no space after "user".
/// assert!(corrected.joins_before);
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
    /// the next segment may still change. The steps of
    /// [`Vocabulary::correct_spoken`] say which those are: each defers its
    /// own words. The held words keep their closing mark unless
    /// `sentence_ended_before` says the sentence goes on.
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
        let unmarked = without_closing_mark(&unheld.text);
        let from = vocabulary.spoken_deferred_from(unmarked, MAX_HELD_WORDS);
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

    pub fn is_holding(&self) -> bool {
        self.held.is_some()
    }

    /// Releases the held words once nothing follows.
    #[must_use = "held words are owed to the transcript"]
    pub fn finish(&mut self) -> Option<Unheld> {
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

    use super::super::super::Spacing;
    use super::super::vocabulary::{PersonalRules, Term};
    use super::*;

    /// Each piece is one segment, corrected after what the stream typed.
    fn streamed(pieces: &[(&str, Option<bool>)], vocabulary: &Vocabulary) -> String {
        let mut words = DeferredWords::default();
        let mut typed = String::new();
        let put = |text: &str, typed: &mut String| {
            let mut caret = Caret::between(typed, "");
            let (text, joins_before) = match vocabulary.correct_spoken(text, typed) {
                Some(corrected) => (corrected.text, corrected.joins_before),
                None => (text.to_owned(), false),
            };
            let spacing = Spacing {
                joins_before,
                defers_ending: true,
            };
            let spaced = caret.insert_with(&text, spacing);
            typed.push_str(&spaced);
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
    #[case::number_after_an_opening_hyphen(&[("utf", None), ("hyphen sixteen", None)], "utf-16")]
    #[case::digits_around_a_hyphen(&[("3", None), ("hyphen", None), ("5", None)], "3-5")]
    #[case::hyphen_chain(&[("en hyphen us", None), ("hyphen posix", None)], "en-us-posix")]
    #[case::slash_opens_a_segment(&[("src", None), ("slash main", None)], "src/main")]
    #[case::backslash_alone(&[("c", None), ("backslash", None), ("windows", None)], "c\\windows")]
    fn joins_across_segments(#[case] pieces: &[(&str, Option<bool>)], #[case] typed: &str) {
        assert_eq!(streamed(pieces, &Vocabulary::default()), typed);
    }

    #[rstest]
    #[case::compound(&[("twenty", None), ("one", None), ("apples", None)], "21 apples")]
    #[case::decimal(&[("two", None), ("point", None), ("five", None)], "2.5")]
    #[case::hundreds_with_and(&[("one hundred and", None), ("five", None)], "105")]
    #[case::and_before_an_ordinal(&[("fifty and", None), ("second", None)], "fifty and second")]
    #[case::unit_after_a_number(&[("costs five", None), ("dollars", None)], "costs $5")]
    #[case::press_ends_on_a_number(&[("it is twenty", None), ("one.", Some(false))], "it is 21.")]
    #[case::symbol_before_a_number(&[("user underscore twenty", None), ("one", None)], "user_21")]
    #[case::mark_after_a_held_number(&[("Twenty.", None), ("One apples.", Some(false))], "21 apples.")]
    #[case::cents_after_dollars(&[("it costs five dollars", None), ("and fifty cents", None)], "it costs $5.50")]
    #[case::cents_right_after_dollars(&[("five dollars", None), ("fifty", None), ("cents", None)], "$5.50")]
    #[case::bracket_before_a_number(&[("(twenty", None), ("one)", None)], "(21)")]
    #[case::digit_words_around_a_symbol(&[("version two", None), ("dot", None), ("five", None)], "version 2.5")]
    #[case::digits_go_on_after_a_symbol(&[("thirty dot one", None), ("zero zero", None)], "30.100")]
    #[case::digits_split_around_a_symbol(&[("three", None), ("dot one", None), ("four", None)], "3.14")]
    #[case::hyphen_before_a_number(&[("utf hyphen", None), ("sixteen", None)], "utf-16")]
    #[case::hyphen_before_a_split_number(&[("version hyphen twenty", None), ("five", None)], "version-25")]
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
    #[case::digit_word_in_prose(&[("no", None), ("one", None), ("knows", None)], "no one knows")]
    #[case::sentence_ended_after_a_number(&[("twenty.", None), ("One more.", Some(true))], "20. One more.")]
    #[case::ellipsis_after_a_number(&[("I have twenty...", None), ("or so", Some(false))], "I have 20... or so")]
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

    #[rstest]
    #[case::symbol_after_a_spoken_form(&["s three underscore", "bucket"], "s3_bucket")]
    #[case::number_after_a_spoken_form(&["upgrade d b two twenty", "five times"], "upgrade db2 25 times")]
    #[case::number_after_a_term(&["area fifty one twenty", "five"], "area fifty one 25")]
    fn words_after_a_term_or_spoken_form_are_still_held(
        #[case] pieces: &[&str],
        #[case] typed: &str,
    ) {
        let term = Term {
            canonical: "Area Fifty One".to_owned(),
            priority: 0,
        };
        let personal = PersonalRules::new(vec!["s3".into(), "db2".into()], vec![]);
        let vocabulary = Vocabulary::compile(&[term], personal).unwrap();
        let pieces: Vec<_> = pieces.iter().map(|piece| (*piece, None)).collect();
        assert_eq!(streamed(&pieces, &vocabulary), typed);
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
        let whole = vocabulary
            .correct_spoken(heard, "")
            .map(|corrected| corrected.text);
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

    #[rstest]
    #[case::trailing_symbol("user underscore", "user ")]
    #[case::symbol_after_punctuation("user, underscore", "user, underscore")]
    #[case::number_before_a_symbol("version two dot", "version ")]
    #[case::cents_may_follow("five dollars and", "")]
    #[case::number_word_in_prose("no one", "no ")]
    #[case::number_before_a_word("twenty one apples", "twenty one apples")]
    fn each_step_says_which_words_it_defers(#[case] heard: &str, #[case] typed: &str) {
        let from =
            Vocabulary::default().spoken_deferred_from(without_closing_mark(heard), MAX_HELD_WORDS);
        assert_eq!(&heard[..from.min(heard.len())], typed);
    }

    #[test]
    fn a_form_waiting_on_its_symbol_is_held_whole() {
        let learned = vec![("tilde dot".to_owned(), "~.".to_owned())];
        let vocabulary = Vocabulary::compile(&[], PersonalRules::new(vec![], learned)).unwrap();
        assert_eq!(streamed(&[("cd tilde dot", None)], &vocabulary), "cd ~.");
        assert_eq!(
            streamed(&[("cd tilde dot", None), ("bashrc", None)], &vocabulary),
            vocabulary
                .correct_spoken("cd tilde dot bashrc", "")
                .unwrap()
                .text
        );
    }

    #[test]
    fn a_symbol_after_a_number_run_with_no_written_form_keeps_it_whole() {
        assert_eq!(
            streamed(
                &[("hundred", None), ("twelve hyphen", None)],
                &Vocabulary::default()
            ),
            "hundred twelve hyphen"
        );
    }

    #[test]
    fn a_symbol_after_a_number_and_its_ordinal_keeps_them_whole() {
        assert_eq!(
            streamed(
                &[("apples", None), ("twelve second hyphen", None)],
                &Vocabulary::default()
            ),
            "apples twelve second hyphen"
        );
    }

    #[test]
    fn a_symbol_after_a_term_that_cannot_join_is_not_held() {
        let vocabulary = Vocabulary::new(["New York"]).unwrap();
        let heard = "visit new york dot five six";
        assert_eq!(
            streamed(
                &[("visit new york dot five", None), ("six", None)],
                &vocabulary
            ),
            vocabulary
                .correct_spoken(heard, "")
                .map_or_else(|| heard.to_owned(), |corrected| corrected.text)
        );
    }

    #[test]
    fn a_long_join_holds_only_its_deferred_end() {
        let heard = "a underscore b underscore c underscore d underscore e underscore twenty";
        assert_eq!(
            streamed(&[(heard, None), ("one", None)], &Vocabulary::default()),
            "a_b_c_d_e_21"
        );
    }

    /// The fuzzy pass fixes the word typed before a join that the next
    /// segment completes, where the whole text keeps it inside the join.
    #[test]
    fn a_fuzzy_fix_beside_a_held_join_applies() {
        let personal = PersonalRules::new(vec!["Kubernetes".into()], vec![]);
        let vocabulary = Vocabulary::compile(&[], personal).unwrap();
        assert_eq!(
            streamed(
                &[("kubernets dot twenty", None), ("one", None)],
                &vocabulary
            ),
            "Kubernetes.21"
        );
    }

    /// A vocabulary whose term, spoken form and learned forms sit beside
    /// numbers and symbols.
    fn crowded() -> Vocabulary {
        let learned = [
            ("tilde dot", "~."),
            ("dot com", ".com"),
            ("see sharp dot", "C#."),
            ("twelve dot", "T."),
            ("fifty underscore", "L_"),
        ]
        .map(|(spoken, typed)| (spoken.to_owned(), typed.to_owned()));
        let personal = PersonalRules::new(vec!["db2".into()], learned.to_vec());
        let term = Term {
            canonical: "New York".to_owned(),
            priority: 0,
        };
        Vocabulary::compile(&[term], personal).unwrap()
    }

    #[rstest]
    #[case::form_whose_start_the_cut_settles_into_a_join(&["fifty hyphen zero twelve dot"])]
    #[case::form_beside_a_join(&["x", "new dot nine fifty underscore"])]
    #[case::form_after_a_symbol(&["foo", "underscore tilde dot", "bashrc"])]
    #[case::form_inside_a_join_more_speech_undoes(&["cd tilde dot five", "dollars"])]
    #[case::hyphen_after_a_symbol_that_cannot_join(&["the end, underscore", "hyphen twenty"])]
    #[case::form_after_a_symbol_that_cannot_join(&["underscore dot", "com"])]
    #[case::unfinished_form(&["use see sharp", "dot"])]
    #[case::number_before_a_symbol_a_form_may_open(&["com two dot", "ten"])]
    fn a_held_form_or_symbol_types_what_the_whole_text_would(#[case] pieces: &[&str]) {
        let vocabulary = crowded();
        let heard = pieces.join(" ");
        let whole = vocabulary
            .correct_spoken(&heard, "")
            .map(|corrected| corrected.text);
        let pieces: Vec<_> = pieces.iter().map(|piece| (*piece, None)).collect();
        assert_eq!(
            streamed(&pieces, &vocabulary),
            whole.as_deref().unwrap_or(&heard)
        );
    }

    /// Every two-segment cut outside a term or spoken form types what the
    /// whole text would. A cut inside one, or right after "new york", is the
    /// open issue of terms split across segments.
    #[rstest]
    #[case("visit new york dot five six")]
    #[case("cd tilde dot bashrc now")]
    #[case("run d b two twenty five times")]
    #[case("see sharp dot net or dot com sites")]
    fn any_cut_beside_a_term_or_form_types_what_the_whole_text_would(#[case] heard: &str) {
        let vocabulary = crowded();
        let units: [&[&str]; 5] = [
            &["new", "york"],
            &["d", "b", "two"],
            &["tilde", "dot"],
            &["see", "sharp", "dot"],
            &["dot", "com"],
        ];
        let words: Vec<&str> = heard.split(' ').collect();
        let splits_a_unit = |cut: usize| {
            units.iter().any(|unit| {
                (0..=words.len().saturating_sub(unit.len())).any(|at| {
                    words[at..at + unit.len()] == **unit && at < cut && cut < at + unit.len()
                })
            })
        };
        let whole = vocabulary
            .correct_spoken(heard, "")
            .map(|corrected| corrected.text);
        for cut in (1..words.len()).filter(|cut| !splits_a_unit(*cut)) {
            if words[..cut].ends_with(&["new", "york"]) {
                continue;
            }
            let pieces = [words[..cut].join(" "), words[cut..].join(" ")];
            let pieces: Vec<_> = pieces.iter().map(|piece| (piece.as_str(), None)).collect();
            assert_eq!(
                streamed(&pieces, &vocabulary),
                whole.as_deref().unwrap_or(heard),
                "{pieces:?}"
            );
        }
    }

    /// Known: a symbol opening a segment joins the last word typed, though
    /// the whole text keeps a term's words together and doesn't join.
    #[test]
    fn a_symbol_opening_a_segment_joins_the_last_word_of_a_term() {
        let pieces = [("visit new york", None), ("dot five six", None)];
        assert_eq!(streamed(&pieces, &crowded()), "visit new york.five six");
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

    #[test]
    fn deferring_stops_once_a_hold_would_type_the_words() {
        let vocabulary = crowded();
        let forms = "cd tilde dot tilde dot tilde dot tilde dot tilde dot tilde";
        assert_eq!(
            vocabulary.spoken_deferred_from(forms, usize::MAX),
            "cd ".len()
        );
        let from = vocabulary.spoken_deferred_from(forms, 2);
        assert!(from > "cd ".len() && forms[from..].split_whitespace().count() > 2);
    }
}
