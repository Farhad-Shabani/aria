//! Correcting a dictation's segments as they stream in.

use super::layer::{Corrected, DeferredWords, TypedBefore, Unheld, Vocabulary, Writing};
use super::sentences::{Punctuated, SentenceEnds};
use super::spacing::{Attachment, Caret, Ending, Spacing};

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
    endings: Endings,
    words: DeferredWords,
    sentences: SentenceEnds,
    /// Everything typed so far, spaced as typed.
    text: String,
}

/// How a segment's closing mark is typed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Endings {
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

impl<'a> SegmentCorrector<'a> {
    pub fn new(vocabulary: &'a Vocabulary) -> Self {
        Self {
            vocabulary,
            writing: Writing::default(),
            endings: Endings::default(),
            words: DeferredWords::default(),
            sentences: SentenceEnds::default(),
            text: String::new(),
        }
    }

    pub fn with_writing(mut self, writing: Writing) -> Self {
        self.writing = writing;
        self
    }

    pub fn with_endings(mut self, endings: Endings) -> Self {
        self.endings = endings;
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
        let sentence_ended_before = match self.endings {
            Endings::Deferred => sentence_ended_before,
            // Every mark stays as decoded, so no reading drops a held one.
            Endings::AsDecoded => None,
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

    /// Types `corrected` as [`Self::push`] types a segment once corrected,
    /// for tests whose text is corrected already.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use = "the corrector counts the text as typed"]
    pub fn push_corrected(
        &mut self,
        corrected: Corrected,
        sentence_ended_before: Option<bool>,
    ) -> Option<Punctuated> {
        self.punctuate(corrected, sentence_ended_before)
    }

    fn correct(&mut self, unheld: Unheld) -> Option<Punctuated> {
        let before =
            TypedBefore::streamed(&self.text, &self.sentences, unheld.sentence_ended_before);
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
        let punctuated = match self.endings {
            Endings::Deferred => {
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
            Endings::AsDecoded => Punctuated {
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

#[cfg(test)]
mod tests {
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
        let mut corrector = SegmentCorrector::new(&vocabulary).with_endings(Endings::AsDecoded);
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
}
