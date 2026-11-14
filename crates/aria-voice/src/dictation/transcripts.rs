//! Domain values shared by live transcript publication and recent history.

use std::ops::Range;
use std::sync::Arc;

use super::{DictationSettings, ExecutionControl};
use crate::speaker::Label;
use aria_platform::delivery::{Caret, Placement};

/// What the pipeline heard in one stretch of speech.
///
/// `speaker` is the voice heard in it, for a sink that writes labels.
///
/// `at` and `span` are both the recording's clock and they answer different
/// questions. `at` is where the words' audio starts, so a sink always has a
/// stamp. `span` is which audio these words are, and only an engine that
/// times its words can say — without it a sink has text to write but nothing
/// to attribute a voice by.
///
/// A sink that types `text` spaces it with `spacing`.
#[derive(Clone)]
pub struct Utterance<'a> {
    pub text: &'a str,
    pub(crate) speaker: Option<&'a Label>,
    pub(crate) at: u64,
    pub(crate) span: Option<Range<u64>>,
    pub spacing: Spacing,
}

/// How an utterance's text meets the text typed around it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Spacing {
    /// The text opens with a spoken symbol that joins the last word typed,
    /// so no space comes before it.
    pub joins_before: bool,
    /// The text's closing mark is held back for the next utterance, so its
    /// end stays open.
    pub defers_ending: bool,
}

impl Spacing {
    /// Returns `text` as typed at `caret`, which moves past it.
    #[must_use = "the caret has moved past text that must be typed"]
    pub fn insert(self, text: &str, caret: &mut Caret) -> String {
        match (self.joins_before, self.defers_ending) {
            (true, true) => caret.attach_open(text),
            (true, false) => caret.attach(text),
            (false, true) => caret.insert_open(text),
            (false, false) => caret.insert(text),
        }
    }
}

impl<'a> Utterance<'a> {
    /// An utterance nobody tried to attribute, from a decode with no
    /// position of its own — a whole-file pass rather than a live stream.
    pub fn anonymous(text: &'a str) -> Self {
        Self {
            text,
            speaker: None,
            at: 0,
            span: None,
            spacing: Spacing::default(),
        }
    }
}

/// Where a press's words went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Inserted,
    Copied,
    /// Never typed: the press ended away from its field, and history keeps
    /// the words.
    InHistory,
    /// Never typed, and with history saving off nothing keeps them.
    NotTyped,
}

impl From<aria_platform::delivery::DeliveryOutcome> for DeliveryOutcome {
    fn from(outcome: aria_platform::delivery::DeliveryOutcome) -> Self {
        match outcome {
            aria_platform::delivery::DeliveryOutcome::Inserted => Self::Inserted,
            aria_platform::delivery::DeliveryOutcome::Copied => Self::Copied,
        }
    }
}

/// Where a sink's text went, once any did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivered {
    pub outcome: DeliveryOutcome,
    /// How many words went there. `None` when the whole transcript did.
    pub words: Option<u32>,
}

/// Where the pipeline publishes what it heard. Segments arrive as they
/// finalize. The completion reaches every sink once the decoder has it, even
/// after a later sink failed, so a rejected last insertion still leaves the
/// utterance in history — a sink may see `completed` after its own `segment`
/// returned an error. A sink that persists — a transcript file, the dictation
/// history — reports its write failures here.
///
/// A dictation sink outlives one dictation. It learns of each through
/// `dictation_started`, once the microphone is running and before any text,
/// and clears what one left behind in `dictation_finished`, however it
/// ended. System transcription opens its file before it starts and calls neither.
pub trait TranscriptSink {
    type Error;

    /// Whether it can take text while recording runs. A sink that types
    /// nothing can.
    const STREAMS: bool = true;

    fn dictation_started(
        &mut self,
        _settings: &DictationSettings,
        _control: &Arc<ExecutionControl>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), Self::Error>;

    fn completed(&mut self, utterance: Utterance<'_>) -> Result<(), Self::Error>;

    /// The operation named the engine serving this dictation.
    fn served_by(&mut self, _engine: &'static str) {}

    /// Where the text went, once any did. A sink that types nothing says so.
    fn delivered(&self) -> Option<Delivered> {
        None
    }

    /// Whether streamed text is waiting for its field to regain focus.
    fn is_queueing(&self) -> bool {
        false
    }

    /// Tries queued text again, with no new segment to carry it.
    fn retry(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Where the next streamed text would land on screen.
    fn placement(&self) -> Option<Placement> {
        None
    }

    /// The dictation ended, as `result` says.
    fn dictation_finished(&mut self, _result: Result<(), &super::DictationError>) {}

    fn recording_finished(
        &mut self,
        _recording: &mut aria_kit::audio::RecordingReader,
        _result: Result<(), &super::DictationError>,
    ) {
    }
}
