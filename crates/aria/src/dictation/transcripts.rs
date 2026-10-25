//! Domain values shared by live transcript publication and recent history.

use std::ops::Range;
use std::sync::Arc;

use super::{DictationSettings, ExecutionControl};
use crate::delivery::{DeliveryOutcome, Placement};
use crate::speaker::Label;

/// What the pipeline heard in one stretch of speech.
///
/// `speaker` is absent when diarization is off, or when a voice after a pause
/// could not be embedded. `embedding` is the voice it measured, which a sink
/// keeps so a later pass can relabel what it already wrote.
///
/// `at` and `span` are both the recording's clock and they answer different
/// questions. `at` is where the decoder had reached, so a sink always has a
/// stamp. `span` is which audio these words are, and only an engine that
/// times its words can say — without it a sink has text to write but nothing
/// to attribute a voice by.
#[derive(Clone)]
pub(crate) struct Utterance<'a> {
    pub(crate) text: &'a str,
    pub(crate) speaker: Option<&'a Label>,
    pub(crate) embedding: Option<&'a [f32]>,
    pub(crate) at: u64,
    pub(crate) span: Option<Range<u64>>,
}

impl<'a> Utterance<'a> {
    /// An utterance nobody tried to attribute, from a decode with no
    /// position of its own — a whole-file pass rather than a live stream.
    pub(crate) fn anonymous(text: &'a str) -> Self {
        Self {
            text,
            speaker: None,
            embedding: None,
            at: 0,
            span: None,
        }
    }
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
/// ended. Listen opens its file before the session and calls neither.
pub(crate) trait TranscriptSink {
    type Error;

    fn dictation_started(
        &mut self,
        _settings: &DictationSettings,
        _control: &Arc<ExecutionControl>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), Self::Error>;

    fn completed(&mut self, utterance: Utterance<'_>) -> Result<(), Self::Error>;

    /// The engine serving this dictation, once its operation names one.
    fn serving_engine(&mut self, _engine: &'static str) {}

    /// Where the text went, once any did. A sink that types nothing says so.
    fn delivered(&self) -> Option<DeliveryOutcome> {
        None
    }

    /// Where the next streamed text would land on screen.
    fn placement(&self) -> Option<Placement> {
        None
    }

    fn dictation_finished(&mut self) {}

    fn recording_finished(
        &mut self,
        _recording: &mut crate::audio::RecordingReader,
        _result: Result<(), &super::DictationError>,
    ) {
    }
}
