//! Spies and builders the dictation tests share.

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{OperationSource, TimedWord};
use aria_kit::voice_activity::{VoiceActivityDetector, VoiceActivityError};

use crate::audio::{RecordingReader, SilenceDuration};
use crate::delivery::{DeliveryError, DeliveryOutcome, OutputMode, Placement, Rect, TextDelivery};
use crate::transcription::{
    Hypothesis, Language, Transcriber, Transcript, TranscriptionAudio, TranscriptionError,
};

use super::{
    Cursor, DeliveryMode, DictationError, DictationEventEmitter, DictationExecutor,
    DictationResult, DictationSettings, ExecutionControl, RecordingId, StopMode, Transcribed,
    TranscriptSink, Utterance,
};

/// One ordered record of what every spy built from it saw, so a test can
/// assert that history heard a segment before the cursor typed it.
#[derive(Clone, Default)]
pub(crate) struct CallLog(Arc<Mutex<Vec<String>>>);

impl CallLog {
    fn push(&self, entry: String) {
        self.0.lock().unwrap().push(entry);
    }

    pub(crate) fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    pub(crate) fn history(&self) -> RecordingSink {
        RecordingSink { log: self.clone() }
    }

    pub(crate) fn cursor(&self) -> Cursor<RecordingDelivery> {
        Cursor::new(RecordingDelivery {
            log: self.clone(),
            fail: false,
        })
    }

    pub(crate) fn failing_cursor(&self) -> Cursor<RecordingDelivery> {
        Cursor::new(RecordingDelivery {
            log: self.clone(),
            fail: true,
        })
    }
}

/// Logs `engine:` for the serving engine, and `segment:` and `completed:`
/// for each utterance history receives.
pub(crate) struct RecordingSink {
    log: CallLog,
}

impl TranscriptSink for RecordingSink {
    type Error = DictationError;

    fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), DictationError> {
        self.log.push(format!("segment:{}", utterance.text));
        Ok(())
    }

    fn completed(&mut self, utterance: Utterance<'_>) -> Result<(), DictationError> {
        self.log.push(format!("completed:{}", utterance.text));
        Ok(())
    }

    fn served_by(&mut self, engine: &'static str) {
        self.log.push(format!("engine:{engine}"));
    }
}

/// Logs `prepare:`, `deliver:`, and `submit`. With `fail`, delivery and
/// submission report an unavailable target once logged.
pub(crate) struct RecordingDelivery {
    pub(crate) log: CallLog,
    pub(crate) fail: bool,
}

impl TextDelivery for RecordingDelivery {
    type Prepared = OutputMode;

    const DEFAULT_MODE: OutputMode = OutputMode::Insert;
    const OUTLIVES_PROCESS: bool = true;

    fn frontmost(&self) -> Option<crate::history::Application> {
        None
    }

    fn prepare(
        &self,
        mode: OutputMode,
        _should_cancel: impl Fn() -> bool,
    ) -> Result<Self::Prepared, DeliveryError> {
        self.log.push(format!("prepare:{mode:?}"));
        Ok(mode)
    }

    fn finish(
        &self,
        prepared: &Self::Prepared,
        should_cancel: impl Fn() -> bool,
    ) -> Result<(), DeliveryError> {
        if *prepared == OutputMode::InsertAndSubmit {
            if should_cancel() {
                return Err(DeliveryError::Cancelled);
            }
            self.log.push("submit".to_owned());
            if self.fail {
                return Err(DeliveryError::TargetUnavailable);
            }
        }
        Ok(())
    }

    fn deliver(
        &self,
        _prepared: &Self::Prepared,
        text: &str,
        _should_cancel: impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        self.log.push(format!("deliver:{text}"));
        if self.fail {
            Err(DeliveryError::TargetUnavailable)
        } else {
            Ok(DeliveryOutcome::Inserted)
        }
    }
}

pub(crate) const PLACED: Rect = Rect {
    x: 100.0,
    y: 200.0,
    width: 1.0,
    height: 16.0,
};

/// Types everything and reports the caret at `PLACED`.
pub(crate) struct PlacedDelivery;

impl TextDelivery for PlacedDelivery {
    type Prepared = ();
    const DEFAULT_MODE: OutputMode = OutputMode::Insert;
    const OUTLIVES_PROCESS: bool = true;

    fn frontmost(&self) -> Option<crate::history::Application> {
        None
    }

    fn prepare(
        &self,
        _mode: OutputMode,
        _should_cancel: impl Fn() -> bool,
    ) -> Result<(), DeliveryError> {
        Ok(())
    }

    fn finish(
        &self,
        _prepared: &(),
        _should_cancel: impl Fn() -> bool,
    ) -> Result<(), DeliveryError> {
        Ok(())
    }

    fn deliver(
        &self,
        _prepared: &(),
        _text: &str,
        _should_cancel: impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        Ok(DeliveryOutcome::Inserted)
    }

    fn placement(&self, _prepared: &()) -> Option<Placement> {
        Some(Placement {
            caret: PLACED,
            field: None,
            font: None,
        })
    }
}

/// Any nonzero sample is speech.
pub(crate) struct Loud;

impl VoiceActivityDetector for Loud {
    const FRAME_SAMPLES: usize = 4;

    fn probability(&mut self, frame: &[i16]) -> Result<f32, VoiceActivityError> {
        Ok(if frame.iter().any(|&sample| sample != 0) {
            1.0
        } else {
            0.0
        })
    }
}

/// An engine that decodes whole audio by calling its closure.
pub(crate) struct Hypotheses<F>(pub(crate) F)
where
    F: Fn(TranscriptionAudio<'_>) -> Result<Hypothesis, TranscriptionError> + Send + Sync + 'static;

impl<F> Transcriber for Hypotheses<F>
where
    F: Fn(TranscriptionAudio<'_>) -> Result<Hypothesis, TranscriptionError> + Send + Sync + 'static,
{
    fn validate_language(&self, _: &Language, _: &CancelToken) -> Result<(), TranscriptionError> {
        Ok(())
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        _: &Language,
        _: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        (self.0)(audio)
    }
}

/// An engine that hands every decode to the operation its closure opens.
pub(crate) struct Operations<F>(pub(crate) F)
where
    F: Fn(&CancelToken) -> OperationSource + Send + Sync + 'static;

impl<F> Transcriber for Operations<F>
where
    F: Fn(&CancelToken) -> OperationSource + Send + Sync + 'static,
{
    fn validate_language(&self, _: &Language, _: &CancelToken) -> Result<(), TranscriptionError> {
        Ok(())
    }

    fn transcribe_with(
        &self,
        _: TranscriptionAudio<'_>,
        _: &Language,
        _: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        panic!("the delegated operation owns decoding")
    }

    fn open_operation(
        &self,
        _: &Language,
        _: bool,
        cancel: &CancelToken,
    ) -> Result<Option<OperationSource>, TranscriptionError> {
        Ok(Some((self.0)(cancel)))
    }
}

/// `text` with each word timed by one `(start, end)` pair, in seconds.
pub(crate) fn timed(text: &str, words: &[(f32, f32)]) -> Hypothesis {
    let words = words
        .iter()
        .map(|&(start, end)| TimedWord { start, end })
        .collect();
    Hypothesis::with_words(text, words)
}

/// A manual-stop press in automatic language with history on and every
/// optional behavior off.
pub(crate) fn settings(delivery_mode: DeliveryMode, output_mode: OutputMode) -> DictationSettings {
    DictationSettings {
        dictation_history: true,
        history_audio_retention: false,
        mute_output: false,
        recognition_language: Language::Automatic,
        output_mode,
        delivery_mode,
        stop_mode: StopMode::Manual,
        silence_duration: SilenceDuration::default(),
        auto_stop: None,
        own_voice_only: false,
        project: None,
        transcript_only: false,
        application: None,
        target: crate::delivery::TargetPolicy::default(),
    }
}

/// How long a test waits for a scripted press to reach its next edge.
pub(crate) const EDGE_WAIT: Duration = Duration::from_secs(5);
/// What every scripted press hears.
pub(crate) const HEARD: &str = "heard";

/// A moment in a scripted press.
#[derive(Debug, PartialEq)]
pub(crate) enum Edge {
    Recording(RecordingId),
    Stopped,
    Cancelled,
}

/// Records until a stop or a cancel, reports each edge, and hears `HEARD`.
pub(crate) struct RecordsUntilStopped {
    edges: Mutex<mpsc::Sender<Edge>>,
}

impl RecordsUntilStopped {
    pub(crate) fn new() -> (Self, mpsc::Receiver<Edge>) {
        let (edges, received) = mpsc::channel();
        let executor = Self {
            edges: Mutex::new(edges),
        };
        (executor, received)
    }

    fn report(&self, edge: Edge) {
        // A test that stopped listening has its answer already.
        let _ = self.edges.lock().expect("the edge sender").send(edge);
    }
}

impl DictationExecutor for RecordsUntilStopped {
    fn execute(
        &self,
        _settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
    ) -> Result<DictationResult, DictationError> {
        let recording = control.start_recording(events)?;
        self.report(Edge::Recording(control.recording_id()));
        while !recording.should_stop() {
            std::thread::sleep(Duration::from_millis(5));
        }
        if control.is_cancelled() {
            self.report(Edge::Cancelled);
            return Err(DictationError::Cancelled);
        }
        self.report(Edge::Stopped);
        Ok(DictationResult {
            text: HEARD.to_owned(),
            own_voice: false,
        })
    }

    fn transcribe(
        &self,
        _recording: &mut RecordingReader,
        _language: &Language,
        _own_voice_only: bool,
        _project: Option<&crate::vocabulary::Directory>,
        _cancel: &CancelToken,
    ) -> Result<Transcribed, DictationError> {
        Ok(Transcribed {
            transcript: Transcript::new(HEARD).map_err(DictationError::Transcription)?,
            engine: "test",
        })
    }
}

pub(crate) fn recording_started(edges: &mpsc::Receiver<Edge>) -> RecordingId {
    match edges.recv_timeout(EDGE_WAIT).expect("a press records") {
        Edge::Recording(id) => id,
        other => panic!("expected a recording, got {other:?}"),
    }
}
