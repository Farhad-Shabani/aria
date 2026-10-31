//! Sentence endings across streamed segments.

/// Holds back the mark that ends a streamed segment until the next one
/// shows whether the sentence went on.
///
/// A segment decoded alone ends as a sentence, so a pause mid-sentence
/// types "should. While it". The next decode hears the words before that
/// pause, and [`TranscriptSegment::sentence_ended_before`] carries its
/// reading. Type [`SentenceEnds::punctuate`] per segment, then
/// [`SentenceEnds::finish`] when nothing follows.
///
/// ```
/// use aria_kit::transcription::SentenceEnds;
///
/// let mut sentences = SentenceEnds::default();
/// let first = sentences.punctuate("I think we should.", None);
/// assert_eq!(first.text, "I think we should");
/// assert!(first.defers_ending);
/// let goes_on = sentences.punctuate("ship it.", Some(false));
/// assert_eq!(goes_on.text, "ship it");
/// let new_sentence = sentences.punctuate("Then rest.", Some(true));
/// assert_eq!(new_sentence.text, ". Then rest");
/// assert_eq!(sentences.finish().unwrap(), ".");
/// ```
///
/// [`TranscriptSegment::sentence_ended_before`]: super::TranscriptSegment::sentence_ended_before
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SentenceEnds {
    /// The last segment's closing mark, not yet typed.
    deferred: Option<char>,
}

/// One segment's text, ready to type after the text before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Punctuated {
    /// Opens with the mark the segment before held back.
    pub text: String,
    /// `text`'s own closing mark is held back. Type `text` with nothing
    /// after it, so the mark can still attach.
    pub defers_ending: bool,
}

/// Abbreviations whose period ends no sentence, as in "etc. and more".
const ABBREVIATIONS: [&str; 2] = ["etc", "vs"];

impl SentenceEnds {
    /// Returns `text` to type after the segment before, its own closing
    /// mark held back. `Some(false)` drops the mark that segment owes;
    /// anything else opens `text` with it.
    #[must_use = "the deferred ending is owed to the previous segment"]
    pub fn punctuate(&mut self, text: &str, sentence_ended_before: Option<bool>) -> Punctuated {
        let mut text = match (self.deferred.take(), sentence_ended_before) {
            (Some(_), Some(false)) | (None, _) => text.to_owned(),
            (Some(mark), _) => format!("{mark} {text}"),
        };
        let defers_ending = match closing_mark(&text) {
            Some(mark) => {
                text.pop();
                self.deferred = Some(mark);
                true
            }
            None => false,
        };
        Punctuated {
            text,
            defers_ending,
        }
    }

    /// The mark the last segment owes, once nothing follows it.
    pub fn finish(&mut self) -> Option<String> {
        self.deferred.take().map(char::into)
    }
}

/// Whether `word` ends a sentence: a lone mark, not "U.S." or "etc.".
pub(crate) fn ends_sentence(word: &str) -> bool {
    closing_mark(word).is_some()
}

/// The one mark that ends `text` as a sentence. `None` for a mark inside
/// the last word, as in "U.S." or "...", and for anything else.
fn closing_mark(text: &str) -> Option<char> {
    let mark = text
        .chars()
        .last()
        .filter(|mark| matches!(mark, '.' | '?' | '!'))?;
    let kept = &text[..text.len() - mark.len_utf8()];
    let last_word = kept.rsplit(char::is_whitespace).next().unwrap_or_default();
    let closes = !last_word.is_empty()
        && !last_word.ends_with(['.', '?', '!'])
        && !(mark == '.' && (last_word.contains('.') || ABBREVIATIONS.contains(&last_word)));
    closes.then_some(mark)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(previous: &str, ended: Option<bool>, next: &str) -> Punctuated {
        let mut sentences = SentenceEnds::default();
        let _ = sentences.punctuate(previous, None);
        sentences.punctuate(next, ended)
    }

    fn typed(text: &str, defers_ending: bool) -> Punctuated {
        Punctuated {
            text: text.to_owned(),
            defers_ending,
        }
    }

    #[test]
    fn a_sentence_the_recognizer_heard_go_on_drops_the_mark() {
        assert_eq!(
            after("I think we should.", Some(false), "ship it tomorrow."),
            typed("ship it tomorrow", true)
        );
        assert_eq!(
            after("Ready?", Some(false), "or not"),
            typed("or not", false)
        );
    }

    #[test]
    fn a_sentence_the_recognizer_heard_end_keeps_the_mark() {
        assert_eq!(
            after("I think we should.", Some(true), "Ship it."),
            typed(". Ship it", true)
        );
        assert_eq!(after("Really?", Some(true), "Yes!"), typed("? Yes", true));
    }

    #[test]
    fn a_pause_at_every_word_keeps_one_sentence() {
        let mut sentences = SentenceEnds::default();
        let mut typed_text: Vec<String> = [
            ("don't need vocabulary.", None),
            ("for those.", Some(false)),
            ("anymore.", Some(false)),
        ]
        .into_iter()
        .map(|(text, ended)| sentences.punctuate(text, ended).text)
        .collect();
        typed_text.extend(sentences.finish());

        assert_eq!(
            typed_text,
            ["don't need vocabulary", "for those", "anymore", "."]
        );
    }

    #[test]
    fn an_unknown_boundary_types_the_mark() {
        assert_eq!(
            after("We should.", None, "while it"),
            typed(". while it", false)
        );
    }

    #[test]
    fn only_a_lone_closing_mark_is_deferred() {
        for text in ["Ship it.", "Ready?", "Stop!"] {
            assert!(
                SentenceEnds::default().punctuate(text, None).defers_ending,
                "{text:?}"
            );
        }
        for text in [
            "Wait...",
            "the U.S.",
            "salsa, etc.",
            "us vs.",
            "done",
            ".",
            "?!",
        ] {
            assert!(
                !SentenceEnds::default().punctuate(text, None).defers_ending,
                "{text:?}"
            );
        }
    }

    #[test]
    fn finish_types_the_deferred_mark_once() {
        let mut sentences = SentenceEnds::default();
        let _ = sentences.punctuate("Ready?", None);
        assert_eq!(sentences.finish().as_deref(), Some("?"));
        assert_eq!(sentences.finish(), None);
    }
}
