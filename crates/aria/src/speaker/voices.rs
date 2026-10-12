//! Who spoke when, read from the audio rather than from the transcript.
//!
//! Per-segment labels depend on transcription boundaries, which can contain
//! too little speech or several voices. A voice track instead analyzes audio
//! independently and assigns labels by each paragraph's audio span.

mod grid;
mod pyannote;
mod timeline;

use std::ops::Range;

#[cfg(test)]
pub(crate) use pyannote::testing;
pub use pyannote::{Chunk, Grouping, PyannoteVoices, Standing, Voiceprint};

/// A running account of who is speaking in a recording.
///
/// Only the question the transcript asks. How a track hears the audio is its
/// own business, and it hears it on its own thread.
pub trait Voices {
    /// Who owns each span. `None` where the track cannot say, such as past
    /// its reach or over audio nobody could embed.
    fn attribute(&self, spans: &[Range<u64>]) -> Vec<Option<String>>;
}

/// Asks a track who owned each paragraph, skipping the ones nobody timed.
///
/// A paragraph without a span has no audio to point at. Passing some other
/// span in its place would still get a confident answer, about somebody
/// else's audio, so it is left `None` for the settling pass to fill from a
/// neighbour.
pub fn attribute_spans(voices: &impl Voices, spans: &[Option<Range<u64>>]) -> Vec<Option<String>> {
    let timed: Vec<Range<u64>> = spans.iter().flatten().cloned().collect();
    let mut answers = voices.attribute(&timed).into_iter();
    spans
        .iter()
        .map(|span| span.as_ref().and_then(|_| answers.next().flatten()))
        .collect()
}

/// One stretch of the recording, and whose voice was in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spoken {
    pub span: Range<u64>,
    pub speaker: String,
}

/// What an observation changed about names already written.
///
/// A voice opening for the first time changes nothing: audio nobody had
/// attributed is now attributed. Two voices turning out to be one changes
/// every paragraph the loser owned, and that is the only reason to rewrite a
/// transcript mid-session — or to tell a reader watching it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Revision {
    pub merges: Vec<Merged>,
}

impl Revision {
    pub fn revised(&self) -> bool {
        !self.merges.is_empty()
    }
}

/// A voice that turned out to be one already heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub from: String,
    pub into: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names each span after where it starts, and records what it was asked.
    struct Positional(std::cell::RefCell<Vec<Range<u64>>>);

    impl Voices for Positional {
        fn attribute(&self, spans: &[Range<u64>]) -> Vec<Option<String>> {
            self.0.borrow_mut().extend_from_slice(spans);
            spans
                .iter()
                .map(|span| Some(format!("at {}", span.start)))
                .collect()
        }
    }

    #[test]
    fn a_paragraph_with_no_span_is_never_asked_about() {
        let voices = Positional(Vec::new().into());
        let named = attribute_spans(&voices, &[Some(10..20), None, Some(30..40)]);

        assert_eq!(
            voices.0.borrow().as_slice(),
            [10..20, 30..40],
            "the track hears only the spans somebody measured"
        );
        assert_eq!(
            named,
            [Some("at 10".to_owned()), None, Some("at 30".to_owned()),],
            "and its answers land back on the paragraphs they were about"
        );
    }

    #[test]
    fn a_track_that_answers_short_leaves_the_rest_unnamed() {
        struct Silent;
        impl Voices for Silent {
            fn attribute(&self, _spans: &[Range<u64>]) -> Vec<Option<String>> {
                Vec::new()
            }
        }
        assert_eq!(attribute_spans(&Silent, &[Some(0..10), None]), [None, None]);
    }
}
