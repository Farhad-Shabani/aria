//! Sentence endings across streamed segments.

use super::spacing::{Attachment, Ending, Spacing};

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
/// use aria_kit::transcription::{Attachment, Ending, SentenceEnds};
///
/// let mut sentences = SentenceEnds::default();
/// let as_written = Attachment::AsWritten;
/// let first = sentences.punctuate("I think we should.", as_written, None);
/// assert_eq!(first.text, "I think we should");
/// assert_eq!(first.spacing.ending, Ending::MarkDeferred);
/// let goes_on = sentences.punctuate("ship it.", as_written, Some(false));
/// assert_eq!(goes_on.text, "ship it");
/// let new_sentence = sentences.punctuate("Then rest.", as_written, Some(true));
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
    /// How to type `text`. When its own closing mark is held back, type it
    /// with nothing after, so the mark can still attach.
    pub spacing: Spacing,
}

/// Abbreviations whose period ends no sentence, as in "etc. and more".
const ABBREVIATIONS: [&str; 2] = ["etc", "vs"];

impl SentenceEnds {
    /// Returns `text` to type after the segment before, its own closing
    /// mark held back. `Some(false)` drops the mark that segment owes;
    /// anything else opens `text` with it. `attachment` says how `text`
    /// meets the text before it.
    #[must_use = "the deferred ending is owed to the previous segment"]
    pub fn punctuate(
        &mut self,
        text: &str,
        attachment: Attachment,
        sentence_ended_before: Option<bool>,
    ) -> Punctuated {
        let (mut text, attachment) = match self.deferred.take() {
            // The owed mark meets the text before it now, not `text`.
            Some(mark) if !goes_on(sentence_ended_before) => {
                (format!("{mark} {text}"), Attachment::AsWritten)
            }
            _ => (text.to_owned(), attachment),
        };
        let ending = match closing_mark(&text) {
            Some(mark) => {
                text.pop();
                self.deferred = Some(mark);
                Ending::MarkDeferred
            }
            None => Ending::Closed,
        };
        Punctuated {
            text,
            spacing: Spacing { attachment, ending },
        }
    }

    pub fn is_deferring(&self) -> bool {
        self.deferred.is_some()
    }

    /// Whether a segment read as `sentence_ended_before` goes on from the
    /// text before it. Without a reading, it does unless a closing mark
    /// waits.
    pub fn reads_on(&self, sentence_ended_before: Option<bool>) -> bool {
        match sentence_ended_before {
            Some(ended) => !ended,
            None => !self.is_deferring(),
        }
    }

    /// The mark the last segment owes, once nothing follows it.
    pub fn finish(&mut self) -> Option<String> {
        self.deferred.take().map(char::into)
    }
}

/// Whether a segment read as `sentence_ended_before` goes on with the
/// sentence before it, so that sentence's closing mark drops. Only a
/// reading says so: without one, the mark stays as the recognizer wrote it.
pub(crate) fn goes_on(sentence_ended_before: Option<bool>) -> bool {
    sentence_ended_before == Some(false)
}

/// Whether `word` ends a sentence: a lone mark, not "U.S." or "etc.".
pub(crate) fn ends_sentence(word: &str) -> bool {
    closing_mark(word).is_some()
}

/// `text` without the mark that ends it as a sentence.
pub(crate) fn without_closing_mark(text: &str) -> &str {
    match closing_mark(text) {
        Some(mark) => &text[..text.len() - mark.len_utf8()],
        None => text,
    }
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
    use super::super::Caret;
    use super::*;

    fn after(previous: &str, ended: Option<bool>, next: &str) -> Punctuated {
        let mut sentences = SentenceEnds::default();
        let _ = sentences.punctuate(previous, Attachment::AsWritten, None);
        sentences.punctuate(next, Attachment::AsWritten, ended)
    }

    fn typed(text: &str, ending: Ending) -> Punctuated {
        Punctuated {
            text: text.to_owned(),
            spacing: Spacing {
                attachment: Attachment::AsWritten,
                ending,
            },
        }
    }

    #[test]
    fn a_sentence_the_recognizer_heard_go_on_drops_the_mark() {
        assert_eq!(
            after("I think we should.", Some(false), "ship it tomorrow."),
            typed("ship it tomorrow", Ending::MarkDeferred)
        );
        assert_eq!(
            after("Ready?", Some(false), "or not"),
            typed("or not", Ending::Closed)
        );
    }

    #[test]
    fn a_sentence_the_recognizer_heard_end_keeps_the_mark() {
        assert_eq!(
            after("I think we should.", Some(true), "Ship it."),
            typed(". Ship it", Ending::MarkDeferred)
        );
        assert_eq!(
            after("Really?", Some(true), "Yes!"),
            typed("? Yes", Ending::MarkDeferred)
        );
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
        .map(|(text, ended)| sentences.punctuate(text, Attachment::AsWritten, ended).text)
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
            typed(". while it", Ending::Closed)
        );
    }

    #[test]
    fn only_a_lone_closing_mark_is_deferred() {
        for text in ["Ship it.", "Ready?", "Stop!"] {
            assert_eq!(
                SentenceEnds::default()
                    .punctuate(text, Attachment::AsWritten, None)
                    .spacing
                    .ending,
                Ending::MarkDeferred,
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
            assert_eq!(
                SentenceEnds::default()
                    .punctuate(text, Attachment::AsWritten, None)
                    .spacing
                    .ending,
                Ending::Closed,
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_reading_says_whether_the_next_text_reads_on() {
        let mut sentences = SentenceEnds::default();
        let _ = sentences.punctuate("We should go.", Attachment::AsWritten, None);
        assert!(sentences.reads_on(Some(false)));
        assert!(!sentences.reads_on(Some(true)));
        assert!(!sentences.reads_on(None));
        let _ = sentences.punctuate("now", Attachment::AsWritten, Some(false));
        assert!(sentences.reads_on(None));
    }

    #[test]
    fn an_owed_mark_meets_the_text_before_it() {
        let mut sentences = SentenceEnds::default();
        let _ = sentences.punctuate("okay.", Attachment::AsWritten, None);
        let next = sentences.punctuate("/help", Attachment::Apart, Some(true));
        let typed = Caret::between("okay", "").insert_with(&next.text, next.spacing);
        assert_eq!(format!("okay{typed}"), "okay. /help");

        let _ = sentences.punctuate("src.", Attachment::AsWritten, None);
        let goes_on = sentences.punctuate("/main", Attachment::Joined, Some(false));
        assert_eq!(goes_on.spacing.attachment, Attachment::Joined);
    }

    #[test]
    fn finish_types_the_deferred_mark_once() {
        let mut sentences = SentenceEnds::default();
        let _ = sentences.punctuate("Ready?", Attachment::AsWritten, None);
        assert_eq!(sentences.finish().as_deref(), Some("?"));
        assert_eq!(sentences.finish(), None);
    }
}
