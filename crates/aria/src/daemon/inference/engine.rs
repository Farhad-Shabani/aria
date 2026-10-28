//! The model an inference host serves, and one request run against it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aria_stt::EngineConfig;
use tokio::sync::mpsc;

use crate::audio::{
    Recording, RecordingReader, RecordingWriter, SpooledRecordingWriter, f32_from_pcm16,
};
use crate::primitives::cancel::CancelToken;
use crate::primitives::local_socket::ExclusiveListener;
use crate::rpc::{RpcErrorCode, RpcFailure};
use crate::transcription::{
    AudioInputKind, Stream, Transcriber, TranscriptSegment, TranscriptUpdate, Transcription,
    TranscriptionError,
};
use aria_kit::transcription::check_cancelled;

use super::{OUTPUT_POLL, OUTPUT_STALL_TIMEOUT};
use crate::inference::{
    Completion, Event, Info, MAX_AUDIO_SECONDS, MAX_TEXT_BYTES, Start, Strategy,
};

pub(in crate::daemon) struct Engine {
    name: &'static str,
    input: AudioInputKind,
    stream: Option<Stream>,
    transcriber: Arc<Box<dyn Transcriber>>,
    spool: PathBuf,
    // A cancelled native call may outlive its waiter and must retain the spool namespace.
    _ownership: Arc<ExclusiveListener>,
}

/// A request this engine accepted, and the reader its upload fills.
pub(in crate::daemon) struct Prepared {
    start: Start,
    recording: RecordingReader,
}

impl Engine {
    pub(super) fn load(
        config: EngineConfig,
        spool: PathBuf,
        ownership: Arc<ExclusiveListener>,
        cancel: &CancelToken,
    ) -> Result<Self, TranscriptionError> {
        let (name, input, stream) = (config.name(), config.input(), config.stream());
        let transcriber = config.load(cancel)?;
        Ok(Self::new(
            name,
            input,
            stream,
            transcriber,
            spool,
            ownership,
        ))
    }

    pub(super) fn new(
        name: &'static str,
        input: AudioInputKind,
        stream: Option<Stream>,
        transcriber: Box<dyn Transcriber>,
        spool: PathBuf,
        ownership: Arc<ExclusiveListener>,
    ) -> Self {
        Self {
            name,
            input,
            stream,
            transcriber: Arc::new(transcriber),
            spool,
            _ownership: ownership,
        }
    }

    pub(super) fn name(&self) -> &'static str {
        self.name
    }

    pub(in crate::daemon) fn info(&self) -> Info {
        Info {
            engine: self.name.to_owned(),
            strategy: if self.stream.is_some() {
                Strategy::Streaming
            } else {
                Strategy::Whole
            },
        }
    }

    /// Refuses a request this engine cannot serve before any audio arrives.
    pub(in crate::daemon) fn prepare(&self, start: Start) -> Result<(Prepared, Input), RpcFailure> {
        if start.engine != self.name {
            return Err(RpcFailure::new(
                RpcErrorCode::Unsupported,
                "the requested engine is not loaded on this server",
            ));
        }
        if start.strategy == Strategy::Streaming && self.stream.is_none() {
            return Err(RpcFailure::new(
                RpcErrorCode::Unsupported,
                "the server engine does not support streaming",
            ));
        }
        let (recording, input) = self.recording()?;
        Ok((Prepared { start, recording }, input))
    }

    fn recording(&self) -> Result<(RecordingReader, Input), RpcFailure> {
        if self.input == AudioInputKind::Wav {
            let (reader, writer) =
                Recording::open_spooled(&self.spool, Duration::from_secs(MAX_AUDIO_SECONDS))
                    .map_err(|_| preparation_failure())?;
            Ok((reader, Input::Wav(writer)))
        } else {
            let (reader, writer) = Recording::open();
            Ok((reader, Input::Samples(writer)))
        }
    }

    pub(in crate::daemon) fn transcribe(
        &self,
        prepared: Prepared,
        events: &mpsc::Sender<Event>,
        cancel: &CancelToken,
    ) -> Result<Completion, RpcFailure> {
        let Prepared {
            start,
            mut recording,
        } = prepared;
        let whole;
        let streaming;
        let mut operation = match (start.strategy, self.stream) {
            (Strategy::Streaming, Some(stream)) => {
                streaming = Transcription::stream(stream, Arc::clone(&self.transcriber));
                streaming.start_with(&start.language, cancel)
            }
            // `prepare` refuses streaming from an engine without a stream.
            (Strategy::Whole, _) | (Strategy::Streaming, None) => {
                whole = Transcription::complete(self.input, Arc::clone(&self.transcriber));
                whole.start_with(&start.language, cancel)
            }
        }
        .map_err(transcription_failure)?;
        let mut budget = OutputBudget::default();
        while let Some(update) = operation
            .next_update(&mut recording)
            .map_err(transcription_failure)?
        {
            check_cancelled(cancel).map_err(transcription_failure)?;
            // The client refuses a preview once a completion is claimed.
            if operation.completed().is_some()
                && matches!(update, TranscriptUpdate::Provisional { .. })
            {
                continue;
            }
            let completed = operation
                .completed()
                .map(|transcript| self.transcript(transcript.as_str()));
            budget.admit(&update, completed.as_ref())?;
            let read_to = recording.held_to();
            let update = match update {
                TranscriptUpdate::Finalized(segment) => {
                    TranscriptUpdate::Finalized(bounded_segment(segment, read_to)?)
                }
                provisional => provisional,
            };
            let retained_from = operation.retained_from().min(read_to);
            recording.release_before(retained_from);
            let event = Event::Update {
                update,
                retained_from,
                read_to,
                completed,
            };
            emit(event, events, OUTPUT_STALL_TIMEOUT, cancel)?;
        }
        check_cancelled(cancel).map_err(transcription_failure)?;
        let transcript = operation
            .completed()
            .ok_or_else(|| transcription_failure(TranscriptionError::NoSpeech))?;
        within_limit(transcript.as_str().len())?;
        Ok(self.transcript(transcript.as_str()))
    }

    fn transcript(&self, text: &str) -> Completion {
        Completion {
            text: text.to_owned(),
            engine: self.name.to_owned(),
        }
    }
}

/// What one operation may send: each text, and the finalized text in total.
#[derive(Default)]
struct OutputBudget {
    finalized: usize,
}

impl OutputBudget {
    fn admit(
        &mut self,
        update: &TranscriptUpdate,
        completed: Option<&Completion>,
    ) -> Result<(), RpcFailure> {
        let text = match update {
            TranscriptUpdate::Provisional { text, .. } => text.as_str(),
            TranscriptUpdate::Finalized(segment) => {
                let separator = usize::from(self.finalized > 0);
                self.finalized = self
                    .finalized
                    .saturating_add(separator)
                    .saturating_add(segment.as_str().len());
                segment.as_str()
            }
        };
        within_limit(text.len())?;
        within_limit(self.finalized)?;
        completed.map_or(Ok(()), |transcript| within_limit(transcript.text.len()))
    }
}

fn within_limit(bytes: usize) -> Result<(), RpcFailure> {
    if bytes > MAX_TEXT_BYTES {
        Err(RpcFailure::new(
            RpcErrorCode::PayloadTooLarge,
            "inference output exceeds its limit",
        ))
    } else {
        Ok(())
    }
}

/// Uploaded PCM goes to memory for an engine that reads samples, or to a
/// private WAV spool for one that reads a file.
pub(in crate::daemon) enum Input {
    Samples(RecordingWriter),
    Wav(SpooledRecordingWriter),
}

impl Input {
    pub(in crate::daemon) fn push(&mut self, samples: &[i16]) -> Result<(), RpcFailure> {
        match self {
            Self::Samples(writer) => {
                let samples: Vec<_> = samples.iter().copied().map(f32_from_pcm16).collect();
                writer.push(&samples);
                Ok(())
            }
            Self::Wav(writer) => writer.push(samples).map_err(|_| preparation_failure()),
        }
    }

    pub(in crate::daemon) fn finish(self) {
        match self {
            Self::Samples(writer) => writer.finish(),
            Self::Wav(writer) => writer.finish(),
        }
    }
}

/// Waits for room in a full queue until `timeout`, then fails Busy.
fn emit(
    mut event: Event,
    events: &mpsc::Sender<Event>,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<(), RpcFailure> {
    let deadline = Instant::now() + timeout;
    loop {
        check_cancelled(cancel).map_err(transcription_failure)?;
        match events.try_send(event) {
            Ok(()) => return Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(transcription_failure(TranscriptionError::Cancelled));
            }
            Err(mpsc::error::TrySendError::Full(pending)) => event = pending,
        }
        if Instant::now() >= deadline {
            return Err(RpcFailure::new(
                RpcErrorCode::Busy,
                "inference output is not being consumed",
            ));
        }
        std::thread::sleep(OUTPUT_POLL);
    }
}

/// Word timings can run past the audio actually received. A span is clipped
/// to it, and one that starts past it is a model fault.
fn bounded_segment(
    segment: TranscriptSegment,
    audio_end: u64,
) -> Result<TranscriptSegment, RpcFailure> {
    segment.bounded(audio_end).map_err(|_| {
        RpcFailure::new(
            RpcErrorCode::TranscriptionFailed,
            "inference returned an invalid audio span",
        )
    })
}

fn preparation_failure() -> RpcFailure {
    RpcFailure::new(
        RpcErrorCode::TranscriptionFailed,
        "preparing inference audio failed",
    )
}

pub(super) fn transcription_failure(error: TranscriptionError) -> RpcFailure {
    let (code, message) = match error {
        TranscriptionError::Cancelled => (RpcErrorCode::Cancelled, "inference was cancelled"),
        TranscriptionError::NoSpeech => (RpcErrorCode::NoSpeech, "no speech was transcribed"),
        TranscriptionError::UnsupportedLanguage { .. } => (
            RpcErrorCode::Unsupported,
            "the engine does not support the requested language",
        ),
        TranscriptionError::Model { .. }
        | TranscriptionError::Load { .. }
        | TranscriptionError::Capture(_)
        | TranscriptionError::Inference { .. }
        | TranscriptionError::UnsupportedAudioPath { .. }
        | TranscriptionError::UnsupportedAudioInput { .. }
        | TranscriptionError::UnsupportedBackend { .. }
        | TranscriptionError::UnsupportedDevice { .. }
        | TranscriptionError::NoNativeStream
        | TranscriptionError::NoDelegatedOperation
        | TranscriptionError::LoadPanicked
        | TranscriptionError::InvalidSegment
        | TranscriptionError::SpeakerTracking
        | TranscriptionError::Processing(_) => {
            (RpcErrorCode::TranscriptionFailed, "inference failed")
        }
    };
    RpcFailure::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Event {
        Event::Update {
            update: TranscriptUpdate::Provisional {
                text: "words".into(),
                source_window: None,
            },
            retained_from: 0,
            read_to: 0,
            completed: None,
        }
    }

    #[test]
    fn output_backpressure_waits_for_room_and_respects_cancellation() {
        let (events, mut receiver) = mpsc::channel(1);
        events.try_send(event()).unwrap();
        let sending = events.clone();
        let worker = std::thread::spawn(move || {
            emit(
                event(),
                &sending,
                Duration::from_secs(1),
                &CancelToken::never(),
            )
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(!worker.is_finished());
        receiver.blocking_recv().unwrap();
        worker.join().unwrap().unwrap();
        let cancelled = CancelToken::new();
        cancelled.cancel();
        assert_eq!(
            emit(event(), &events, OUTPUT_STALL_TIMEOUT, &cancelled)
                .unwrap_err()
                .code,
            RpcErrorCode::Cancelled
        );
        assert_eq!(
            emit(
                event(),
                &events,
                Duration::from_millis(10),
                &CancelToken::never(),
            )
            .unwrap_err()
            .code,
            RpcErrorCode::Busy
        );
    }

    #[test]
    fn exported_word_timings_stop_at_real_audio_without_losing_the_span() {
        let tail = TranscriptSegment::new("tail words", Some(80..320)).unwrap();
        let bounded = bounded_segment(tail, 160).unwrap();
        assert_eq!(bounded.as_str(), "tail words");
        assert_eq!(bounded.span(), Some(80..160));
        let beyond_audio = TranscriptSegment::new("impossible", Some(160..320)).unwrap();
        assert_eq!(
            bounded_segment(beyond_audio, 160).unwrap_err().code,
            RpcErrorCode::TranscriptionFailed
        );
    }

    #[test]
    fn output_budget_bounds_each_text_and_the_finalized_total() {
        let half = "x".repeat(MAX_TEXT_BYTES / 2);
        let segment = || TranscriptUpdate::Finalized(TranscriptSegment::new(&half, None).unwrap());
        let mut budget = OutputBudget::default();
        assert!(budget.admit(&segment(), None).is_ok());
        assert_eq!(
            budget.admit(&segment(), None).unwrap_err().code,
            RpcErrorCode::PayloadTooLarge
        );
        let preview = TranscriptUpdate::Provisional {
            text: "x".repeat(MAX_TEXT_BYTES + 1),
            source_window: None,
        };
        assert!(OutputBudget::default().admit(&preview, None).is_err());
    }
}
