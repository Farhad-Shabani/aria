//! Correcting a dictation's segments as they stream in.

use super::super::sentences::{Punctuated, SentenceEnds};
use super::super::spacing::{Attachment, Caret, Ending, Spacing};
use super::correction::TypedBefore;
use super::deferred::{DeferredWords, Unheld};
use super::{Corrected, Vocabulary, Writing};

/// Corrects each segment of a dictation after the text typed before it,
/// holding back what the next segment may still change.
///
/// ```
/// use aria_kit::transcription::{SegmentCorrector, Vocabulary};
///
/// let vocabulary = Vocabulary::default();
/// let mut corrector = SegmentCorrector::new(&vocabulary);
/// let typed = corrector.push("open user underscore", None).unwrap();
/// assert_eq!(typed.text, "open user");
/// let typed = corrector.push("id.", Some(false)).unwrap();
/// assert_eq!(typed.text, "_id");
/// let end = corrector.finish();
/// assert_eq!(end.mark.unwrap().text, ".");
/// assert_eq!(end.text, "open user_id.");
/// ```
pub struct SegmentCorrector<'a> {
    vocabulary: &'a Vocabulary,
    writing: Writing,
    closing_mark: ClosingMark,
    words: DeferredWords,
    sentences: SentenceEnds,
    /// Everything typed so far, spaced as typed.
    text: String,
}

impl<'a> SegmentCorrector<'a> {
    pub fn new(vocabulary: &'a Vocabulary) -> Self {
        Self {
            vocabulary,
            writing: Writing::default(),
            closing_mark: ClosingMark::default(),
            words: DeferredWords::default(),
            sentences: SentenceEnds::default(),
            text: String::new(),
        }
    }

    pub fn with_writing(mut self, writing: Writing) -> Self {
        self.writing = writing;
        self
    }

    pub fn with_closing_mark(mut self, closing_mark: ClosingMark) -> Self {
        self.closing_mark = closing_mark;
        self
    }

    /// The text to type for `segment`: the words held before it, then its
    /// own up to those the next segment may still change. `None` when it
    /// has nothing to type.
    #[must_use = "the corrector counts the text as typed"]
    pub fn push(
        &mut self,
        segment: &str,
        sentence_ended_before: Option<bool>,
    ) -> Option<Punctuated> {
        let sentence_ended_before = match self.closing_mark {
            ClosingMark::Deferred => sentence_ended_before,
            // Every mark stays as decoded, so no reading drops a held one.
            ClosingMark::AsDecoded => None,
        };
        let unheld = self.words.hold(
            segment,
            sentence_ended_before,
            self.vocabulary,
            self.writing,
        );
        self.correct(unheld)
    }

    /// The held words to type after a pause, unless they end on a named
    /// slash or backslash, which waits for its word.
    #[must_use = "the corrector counts the text as typed"]
    pub fn release_paused(&mut self) -> Option<Punctuated> {
        let held = self.words.release_paused()?;
        self.correct(held)
    }

    pub fn is_holding(&self) -> bool {
        self.words.is_holding()
    }

    /// Everything typed so far, without what is still held.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn finish(mut self) -> StreamEnd {
        let words = self.words.finish().and_then(|held| self.correct(held));
        let mark = self.sentences.finish().map(|mark| {
            self.keep(Punctuated {
                text: mark,
                spacing: Spacing {
                    ending: Ending::Open,
                    ..Spacing::default()
                },
            })
        });
        StreamEnd {
            words,
            mark,
            text: self.text,
        }
    }

    fn correct(&mut self, unheld: Unheld) -> Option<Punctuated> {
        let before = preceding(&self.text, &self.sentences, unheld.sentence_ended_before);
        let corrected = self
            .vocabulary
            .correct_spoken_after(&unheld.text, before, self.writing)
            .unwrap_or(Corrected {
                text: unheld.text,
                attachment: Attachment::default(),
            });
        self.punctuate(corrected, unheld.sentence_ended_before)
    }

    fn punctuate(
        &mut self,
        corrected: Corrected,
        sentence_ended_before: Option<bool>,
    ) -> Option<Punctuated> {
        if corrected.text.is_empty() {
            return None;
        }
        let punctuated = match self.closing_mark {
            ClosingMark::Deferred => {
                let mut punctuated = self.sentences.punctuate(
                    &corrected.text,
                    corrected.attachment,
                    sentence_ended_before,
                );
                // The next segment may still join this one, so the space
                // before a word after the caret waits for the end.
                if punctuated.spacing.ending == Ending::Closed {
                    punctuated.spacing.ending = Ending::Open;
                }
                punctuated
            }
            ClosingMark::AsDecoded => Punctuated {
                text: corrected.text,
                spacing: Spacing {
                    attachment: corrected.attachment,
                    ending: Ending::Closed,
                },
            },
        };
        Some(self.keep(punctuated))
    }

    fn keep(&mut self, punctuated: Punctuated) -> Punctuated {
        let spaced =
            Caret::between(&self.text, "").insert_with(&punctuated.text, punctuated.spacing);
        self.text.push_str(&spaced);
        punctuated
    }
}

/// How a segment's closing mark is typed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ClosingMark {
    /// Held until the next segment shows whether the sentence ended there,
    /// for segments decoded as they stream.
    #[default]
    Deferred,
    /// As the recognizer wrote it, for segments of one decode of the whole
    /// recording.
    AsDecoded,
}

/// What a dictation still owed once nothing follows: its held words, then
/// its closing mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEnd {
    pub words: Option<Punctuated>,
    pub mark: Option<Punctuated>,
    /// Everything typed, these included.
    pub text: String,
}

/// What sits before a streamed segment typed after `typed`. It is
/// [`TypedBefore::Apart`] when `sentence_ended_before` says a sentence
/// ended, or when it is `None` and `sentences` holds back a closing mark.
fn preceding<'a>(
    typed: &'a str,
    sentences: &SentenceEnds,
    sentence_ended_before: Option<bool>,
) -> TypedBefore<'a> {
    if typed.is_empty() {
        TypedBefore::Nothing
    } else if sentences.reads_on(sentence_ended_before) {
        TypedBefore::Text(typed)
    } else {
        TypedBefore::Apart
    }
}

#[cfg(test)]
pub(super) mod tests {
    use rstest::rstest;

    use super::super::vocabulary::{PersonalRules, Term};
    use super::*;

    fn typed(corrector: &mut SegmentCorrector<'_>, segment: &str, ended: Option<bool>) -> String {
        corrector
            .push(segment, ended)
            .map(|punctuated| punctuated.text)
            .unwrap_or_default()
    }

    #[test]
    fn a_segment_with_nothing_to_type_leaves_the_owed_mark_owed() {
        let vocabulary = Vocabulary::default();
        let mut corrector = SegmentCorrector::new(&vocabulary);
        assert_eq!(typed(&mut corrector, "We should.", None), "We should");

        assert_eq!(corrector.push("", Some(true)), None);
        assert_eq!(corrector.push("twenty", Some(true)), None);

        let end = corrector.finish();
        assert_eq!(end.words.map(|words| words.text).as_deref(), Some(". 20"));
        assert_eq!(end.mark, None);
        assert_eq!(end.text, "We should. 20");
    }

    #[test]
    fn a_streamed_segment_leaves_its_end_open() {
        let vocabulary = Vocabulary::default();
        let mut corrector = SegmentCorrector::new(&vocabulary);
        let open_src = corrector.push("open src", None).unwrap();
        assert_eq!(open_src.spacing.ending, Ending::Open);
        let end = corrector.finish();
        assert_eq!(end.text, "open src");
    }

    #[test]
    fn segments_as_decoded_keep_their_marks() {
        let vocabulary = Vocabulary::default();
        let mut corrector =
            SegmentCorrector::new(&vocabulary).with_closing_mark(ClosingMark::AsDecoded);
        let should = corrector.push("We should.", Some(false)).unwrap();
        assert_eq!(should.text, "We should.");
        assert_eq!(should.spacing.ending, Ending::Closed);
        assert_eq!(typed(&mut corrector, "Ship it.", Some(false)), "Ship it.");
        let end = corrector.finish();
        assert_eq!(end.mark, None);
        assert_eq!(end.text, "We should. Ship it.");
    }

    #[test]
    fn held_words_type_after_a_pause_after_the_text_before_them() {
        let vocabulary = Vocabulary::default();
        let mut corrector = SegmentCorrector::new(&vocabulary);
        assert_eq!(typed(&mut corrector, "it is twenty", None), "it is");
        assert!(corrector.is_holding());
        assert_eq!(corrector.text(), "it is");

        let twenty = corrector.release_paused().unwrap();

        assert_eq!(twenty.text, "20");
        assert!(!corrector.is_holding());
        assert_eq!(corrector.finish().text, "it is 20");
    }

    #[test]
    fn the_writing_decides_a_slash_after_a_plain_word() {
        let vocabulary = Vocabulary::default();
        let mut prose = SegmentCorrector::new(&vocabulary).with_writing(Writing::Prose);
        let mut commands = SegmentCorrector::new(&vocabulary);
        for corrector in [&mut prose, &mut commands] {
            let _ = corrector.push("and forward slash or", None);
        }
        assert_eq!(prose.finish().text, "and/or");
        assert_eq!(commands.finish().text, "and /or");
    }

    /// Each piece is one segment, corrected after what the stream typed.
    pub(in super::super) fn streamed(
        pieces: &[(&str, Option<bool>)],
        vocabulary: &Vocabulary,
        writing: Writing,
    ) -> String {
        let mut corrector = SegmentCorrector::new(vocabulary).with_writing(writing);
        for (piece, sentence_ended_before) in pieces {
            let _ = corrector.push(piece, *sentence_ended_before);
        }
        corrector.finish().text
    }

    /// What the whole text types in one go.
    fn whole(heard: &str, vocabulary: &Vocabulary, writing: Writing) -> String {
        vocabulary
            .correct_spoken(heard, writing)
            .map_or_else(|| heard.to_owned(), |corrected| corrected.text)
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
    #[case::slash_opens_a_segment(&[("src", None), ("forward slash main", None)], "src/main")]
    #[case::backslash_alone(&[("c", None), ("backslash", None), ("windows", None)], "c\\windows")]
    #[case::backslash_said_apart(&[("c back", None), ("slash windows", None)], "c\\windows")]
    #[case::symbol_opens_the_dictation(&[("forward slash", None), ("conf", None)], "/conf")]
    #[case::repeated_symbol_split(&[("forward slash", None), ("forward slash conf", None)], "//conf")]
    #[case::listed_run_split(&[("Backslash, backslash,", None), ("confidence.", None)], "\\\\confidence.")]
    #[case::long_join_holds_only_its_deferred_end(&[("a underscore b underscore c underscore d underscore e underscore twenty", None), ("one", None)], "a_b_c_d_e_21")]
    fn joins_across_segments(#[case] pieces: &[(&str, Option<bool>)], #[case] typed: &str) {
        assert_eq!(
            streamed(pieces, &Vocabulary::default(), Writing::Commands),
            typed
        );
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
    #[case::year(&[("in twenty", None), ("fifteen", None)], "in 2015")]
    #[case::year_split_after_oh(&[("twenty oh", None), ("five", None)], "2005")]
    #[case::digit_words(&[("nine five", None), ("five", None)], "955")]
    #[case::time(&[("at nine fifty five", None), ("pm", None)], "at 9:55 PM")]
    #[case::date(&[("on March", None), ("fifth", None)], "on March 5")]
    #[case::date_and_year(&[("March fifth", None), ("twenty fifteen", None)], "March 5, 2015")]
    fn writes_numbers_across_segments(
        #[case] pieces: &[(&str, Option<bool>)],
        #[case] typed: &str,
    ) {
        assert_eq!(
            streamed(pieces, &Vocabulary::default(), Writing::Commands),
            typed
        );
    }

    #[rstest]
    #[case::ordinary_words(&[("turn", None), ("left now", None)], "turn left now")]
    #[case::press_ends_on_back(&[("go", None), ("back", None)], "go back")]
    #[case::back_goes_on_without_a_slash(&[("go back", None), ("a little", None)], "go back a little")]
    #[case::press_ends_on_a_symbol(&[("user", None), ("underscore.", None)], "user underscore.")]
    #[case::sentence_ended_after_the_symbol(&[("the dot.", None), ("Then we rest.", Some(true))], "the dot. Then we rest.")]
    #[case::unread_cut_after_the_symbol(&[("the dot.", None), ("Then we rest.", None)], "the dot. Then we rest.")]
    #[case::word_before_ends_a_sentence(&[("Done.", None), ("hyphen", None), ("of the word", None)], "Done. hyphen of the word")]
    #[case::digit_word_in_prose(&[("no", None), ("one", None), ("knows", None)], "no one knows")]
    #[case::sentence_ended_after_a_number(&[("twenty.", None), ("One more.", Some(true))], "20. One more.")]
    #[case::ellipsis_after_a_number(&[("I have twenty...", None), ("or so", Some(false))], "I have 20... or so")]
    #[case::symbol_after_a_number_run_with_no_written_form(&[("hundred", None), ("twelve hyphen", None)], "hundred twelve hyphen")]
    #[case::symbol_after_a_number_and_its_ordinal(&[("apples", None), ("twelve second hyphen", None)], "apples twelve second hyphen")]
    #[case::year_before_dollars(&[("nineteen ninety nine", None), ("dollars", None)], "nineteen ninety nine dollars")]
    #[case::month_before_words(&[("the end of March", None), ("and then", None)], "the end of March and then")]
    fn leaves_unjoined_across_segments(
        #[case] pieces: &[(&str, Option<bool>)],
        #[case] typed: &str,
    ) {
        assert_eq!(
            streamed(pieces, &Vocabulary::default(), Writing::Commands),
            typed
        );
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
        assert_eq!(streamed(&pieces, &vocabulary, Writing::Commands), typed);
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
    #[case("in twenty oh five and nineteen ninety nine")]
    #[case("room nine five five at ten oh five")]
    #[case("meet at nine fifty five pm or three am")]
    #[case("on March twenty first twenty fifteen and June five")]
    fn any_split_types_what_the_whole_text_would(#[case] heard: &str) {
        assert_any_split_types_the_whole(heard, &Vocabulary::default(), Writing::Commands);
    }

    /// Listed words whose spoken forms span words, hold a number word, or
    /// begin another listed word's forms.
    #[rstest]
    #[case("detect db 1 is that")]
    #[case("run d b one now")]
    #[case("the db 1 backup is up")]
    #[case("use i eighteen n now")]
    #[case("open aria dot toml now")]
    fn any_split_types_a_spoken_form_whole(#[case] heard: &str) {
        let vocabulary = listing(&["db1", "db1-backup", "i18n", "Aria"]);
        assert_any_split_types_the_whole(heard, &vocabulary, Writing::Commands);
    }

    #[test]
    fn any_split_types_a_merged_word_whole() {
        let vocabulary = listing(&["PostgreSQL"]);
        assert_any_split_types_the_whole("use postgre sql now", &vocabulary, Writing::Commands);
    }

    #[rstest]
    #[case("open test_aria", "open test_aria")]
    #[case("open src/aria", "open src/aria")]
    #[case("open file dot aria", "open file.aria")]
    #[case("say my aria", "say Alias")]
    #[case("an éclair", "an éclair")]
    fn fuzzy_holds_respect_the_words_other_steps_read(#[case] heard: &str, #[case] expected: &str) {
        let project = Term {
            canonical: "file.aria".into(),
            priority: 1,
        };
        let personal = PersonalRules::new(
            vec!["Aria".into(), "Clair".into(), "Alias".into()],
            vec![("my aria".into(), "Alias".into())],
        );
        let vocabulary = Vocabulary::compile(&[project], personal).unwrap();
        assert_eq!(
            streamed(&[(heard, None)], &vocabulary, Writing::Commands),
            expected
        );
        assert_any_split_types_the_whole(heard, &vocabulary, Writing::Commands);
    }

    #[rstest]
    #[case(&["open kubernets", "dot yaml"], "open kubernets.yaml")]
    #[case(&["use postgre sql", "dot conf"], "use postgre sql.conf")]
    fn a_fuzzy_word_at_segment_end_may_join_a_symbol(
        #[case] segments: &[&str],
        #[case] expected: &str,
    ) {
        let vocabulary = listing(&["PostgreSQL", "Kubernetes"]);
        let pieces: Vec<_> = segments.iter().map(|s| (*s, None)).collect();
        assert_eq!(streamed(&pieces, &vocabulary, Writing::Commands), expected);
    }

    #[rstest]
    #[case("pass --flag='id' then ask for id", "pass --flag='id' then ask for ID")]
    #[case("set title=\"id\" or say 'id'", "set title=\"id\" or say 'ID'")]
    #[case(
        "set x='server id' then open kubernets",
        "set x='server id' then open Kubernetes"
    )]
    #[case("set x=\"id and kubernets\" now", "set x=\"id and kubernets\" now")]
    #[case(r#"x="use \"id\" and id" then id"#, r#"x="use \"id\" and id" then ID"#)]
    #[case(
        "x='twenty one user underscore id' and twenty one",
        "x='twenty one user underscore id' and 21"
    )]
    fn quoted_assignment_values_keep_their_words(#[case] heard: &str, #[case] expected: &str) {
        let vocabulary = listing(&["ID", "Kubernetes"]);
        assert_eq!(whole(heard, &vocabulary, Writing::Commands), expected);
        assert_any_split_types_the_whole(heard, &vocabulary, Writing::Commands);
    }

    #[test]
    fn an_unclosed_assignment_preserves_just_its_first_word() {
        let vocabulary = listing(&["ID", "Kubernetes"]);
        let heard = "set x='id then ask for id and kubernets";
        let expected = "set x='id then ask for ID and Kubernetes";
        assert_eq!(whole(heard, &vocabulary, Writing::Commands), expected);
        assert_eq!(vocabulary.correct(heard).as_deref(), Some(expected));
        assert_any_split_types_the_whole(heard, &vocabulary, Writing::Commands);
        let next = vocabulary
            .correct_spoken_after(
                "for id and kubernets",
                TypedBefore::Text("set x='id then ask"),
                Writing::Commands,
            )
            .unwrap();
        assert_eq!(next.text, "for ID and Kubernetes");
    }

    #[test]
    fn written_values_survive_project_and_personal_correction() {
        let terms = [Term {
            canonical: "file.aria".into(),
            priority: 1,
        }];
        let personal = PersonalRules::new(vec!["ID".into()], vec![("aye dee".into(), "ID".into())]);
        let vocabulary = Vocabulary::compile(&terms, personal).unwrap();
        let heard = "x='file dot aria aye dee' then file dot aria aye dee";
        let expected = "x='file dot aria aye dee' then file.aria ID";
        assert_eq!(vocabulary.correct(heard).as_deref(), Some(expected));
        assert_eq!(whole(heard, &vocabulary, Writing::Commands), expected);
        assert_any_split_types_the_whole(heard, &vocabulary, Writing::Commands);
    }

    pub(in super::super) fn listing(words: &[&str]) -> Vocabulary {
        let listed = words.iter().map(|word| (*word).to_owned()).collect();
        Vocabulary::compile(&[], PersonalRules::new(listed, vec![])).unwrap()
    }

    /// Cuts `heard` at its spaces into two or three segments, every way.
    fn assert_any_split_types_the_whole(heard: &str, vocabulary: &Vocabulary, writing: Writing) {
        let whole = whole(heard, vocabulary, writing);
        let words: Vec<&str> = heard.split(' ').collect();
        for first in 1..words.len() {
            for second in first..words.len() {
                let pieces = [&words[..first], &words[first..second], &words[second..]]
                    .map(|piece| piece.join(" "));
                let pieces: Vec<(&str, Option<bool>)> = pieces
                    .iter()
                    .filter(|piece| !piece.is_empty())
                    .map(|piece| (piece.as_str(), None))
                    .collect();
                assert_eq!(streamed(&pieces, vocabulary, writing), whole, "{pieces:?}");
            }
        }
    }

    #[rstest]
    #[case::number_before_a_name_said_apart("version two back slash five")]
    #[case::repeated_name_said_apart("c back slash back slash server")]
    #[case::listed_with_commas("x forward slash, forward slash, y")]
    #[case::run_before_a_join("dot dot forward slash src forward slash main")]
    #[case::run_opens_the_dictation("hyphen hyphen verbose now")]
    #[case::comma_inside_a_name("hyphen back, slash, b")]
    #[case::name_after_a_comma("x, back slash y")]
    #[case::name_after_a_symbol("x dot back slash")]
    #[case::name_after_an_opening_symbol("dot back slash x")]
    #[case::name_after_a_line_break("x\nback slash y")]
    #[case::flag_after_a_word("git commit hyphen hyphen amend now")]
    #[case::flag_after_a_comma("then, hyphen hyphen verbose")]
    #[case::path_from_here("run dot forward slash build now")]
    #[case::path_from_the_parent("cd dot dot forward slash src forward slash main")]
    #[case::flag_after_a_number("version two hyphen hyphen all")]
    #[case::slash_between_words("open src forward slash main now")]
    #[case::slash_last("cd into src forward slash")]
    #[case::slash_opens_the_dictation("forward slash conf now")]
    #[case::slash_after_a_comma("path, forward slash usr now")]
    #[case::slash_before_a_number("open twenty four forward slash seven now")]
    #[case::slash_after_an_english_word("run forward slash review now")]
    #[case::slash_after_a_contraction("let's forward slash review now")]
    #[case::run_said_last("go up dot dot forward slash")]
    #[case::slash_after_a_hyphenated_word("re-run forward slash test now")]
    #[case::slash_after_a_typographic_apostrophe("it’s forward slash x now")]
    #[case::slash_after_a_spoken_compound("build hyphen tools forward slash main now")]
    #[case::slash_after_a_spoken_compound_of_a_path("user underscore id forward slash x now")]
    #[case::slash_after_an_opening_symbol("dot local forward slash share now")]
    #[case::slash_after_digits_said_one_by_one("open two five oh forward slash report now")]
    #[case::slash_after_a_run_ending_a_sentence("type dot forward slash? run forward slash help")]
    fn any_split_around_a_name_of_several_words_types_the_whole(#[case] heard: &str) {
        assert_any_split_types_the_whole(heard, &Vocabulary::default(), Writing::Commands);
    }

    #[rstest]
    #[case::pair_of_words("read and forward slash or write now")]
    #[case::branch("push origin forward slash main now")]
    #[case::after_a_comma("done, forward slash review now")]
    fn any_split_in_prose_types_the_whole(#[case] heard: &str) {
        assert_any_split_types_the_whole(heard, &Vocabulary::default(), Writing::Prose);
    }

    #[test]
    fn any_split_after_a_term_of_several_words_types_the_whole() {
        let vocabulary = listing(&["S3 Bucket"]);
        let heard = "open s three bucket forward slash report";
        assert_eq!(
            whole(heard, &vocabulary, Writing::Commands),
            "open S3 Bucket /report"
        );
        assert_any_split_types_the_whole(heard, &vocabulary, Writing::Commands);
    }

    #[rstest]
    #[case::form_naming_a_listed_run("it was hyphen hyphen frankly", ("hyphen hyphen", "—"))]
    #[case::form_naming_a_run_between_words("wait dot dot dot okay", ("dot dot dot", "…"))]
    #[case::form_opening_the_dictation("dot net is great", ("dot net", ".NET"))]
    #[case::form_opening_the_dictation_before_a_symbol("dot net underscore id", ("dot net", ".NET"))]
    #[case::form_opening_on_a_symbol_after_a_word("use dot net now", ("dot net", ".NET"))]
    #[case::form_begun_inside_a_name("x slash back slash now", ("slash command", "/command"))]
    #[case::form_begun_inside_an_opening_name("slash back slash now", ("slash command", "/command"))]
    fn any_split_around_a_form_on_symbol_words_types_the_whole(
        #[case] heard: &str,
        #[case] learned: (&str, &str),
    ) {
        let learned = vec![(learned.0.to_owned(), learned.1.to_owned())];
        let vocabulary = Vocabulary::compile(&[], PersonalRules::new(vec![], learned)).unwrap();
        assert_any_split_types_the_whole(heard, &vocabulary, Writing::Commands);
    }

    #[test]
    fn a_form_waiting_on_its_symbol_is_held_whole() {
        let learned = vec![("tilde dot".to_owned(), "~.".to_owned())];
        let vocabulary = Vocabulary::compile(&[], PersonalRules::new(vec![], learned)).unwrap();
        assert_eq!(
            streamed(&[("cd tilde dot", None)], &vocabulary, Writing::Commands),
            "cd ~."
        );
        assert_eq!(
            streamed(
                &[("cd tilde dot", None), ("bashrc", None)],
                &vocabulary,
                Writing::Commands
            ),
            whole("cd tilde dot bashrc", &vocabulary, Writing::Commands)
        );
    }

    #[test]
    fn a_symbol_after_a_term_that_cannot_join_is_not_held() {
        let vocabulary = Vocabulary::new(["New York"]).unwrap();
        let heard = "visit new york dot five six";
        assert_eq!(
            streamed(
                &[("visit new york dot five", None), ("six", None)],
                &vocabulary,
                Writing::Commands
            ),
            whole(heard, &vocabulary, Writing::Commands)
        );
    }

    /// The fuzzy pass fixes the word typed before a join that the next
    /// segment completes, where the whole text keeps it inside the join.
    #[test]
    fn a_fuzzy_fix_beside_a_held_join_applies() {
        let vocabulary = listing(&["Kubernetes"]);
        assert_eq!(
            streamed(
                &[("kubernets dot twenty", None), ("one", None)],
                &vocabulary,
                Writing::Commands
            ),
            "Kubernetes.21"
        );
    }

    /// A vocabulary whose term, spoken form and learned forms sit beside
    /// numbers and symbols.
    pub(in super::super) fn crowded() -> Vocabulary {
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
        let pieces: Vec<_> = pieces.iter().map(|piece| (*piece, None)).collect();
        assert_eq!(
            streamed(&pieces, &vocabulary, Writing::Commands),
            whole(&heard, &vocabulary, Writing::Commands)
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
        let whole = whole(heard, &vocabulary, Writing::Commands);
        for cut in (1..words.len()).filter(|cut| !splits_a_unit(*cut)) {
            if words[..cut].ends_with(&["new", "york"]) {
                continue;
            }
            let pieces = [words[..cut].join(" "), words[cut..].join(" ")];
            let pieces: Vec<_> = pieces.iter().map(|piece| (piece.as_str(), None)).collect();
            assert_eq!(
                streamed(&pieces, &vocabulary, Writing::Commands),
                whole,
                "{pieces:?}"
            );
        }
    }

    /// Known: a symbol opening a segment joins the last word typed, though
    /// the whole text keeps a term's words together and doesn't join.
    #[test]
    fn a_symbol_opening_a_segment_joins_the_last_word_of_a_term() {
        let pieces = [("visit new york", None), ("dot five", None)];
        assert_eq!(
            streamed(&pieces, &crowded(), Writing::Commands),
            "visit new york.five"
        );
    }

    pub(in super::super) fn names() -> Vocabulary {
        let terms = ["aria-kit", "AppDelegate.swift"].map(|canonical| Term {
            canonical: canonical.to_owned(),
            priority: 1,
        });
        let pair = vec![("arya".to_owned(), "aria".to_owned())];
        Vocabulary::compile(&terms, PersonalRules::new(vec![], pair)).unwrap()
    }

    #[rstest]
    #[case("open aria dash kit now")]
    #[case("open aria hyphen kit now")]
    #[case("open app delegate dot swift now")]
    #[case::through_a_pair("open arya hyphen kit now")]
    fn any_split_types_a_name_whole(#[case] heard: &str) {
        assert_any_split_types_the_whole(heard, &names(), Writing::Commands);
    }

    #[rstest]
    #[case::capitalized(&[("Aria", None), ("hyphen kit", None)], "aria-kit")]
    #[case::marks(&[("Open Aria.", None), ("Hyphen kit now.", Some(false))], "Open aria-kit now.")]
    fn a_name_joins_across_cased_segments(
        #[case] pieces: &[(&str, Option<bool>)],
        #[case] typed: &str,
    ) {
        assert_eq!(streamed(pieces, &names(), Writing::Commands), typed);
    }
}
