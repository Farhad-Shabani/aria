use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aria_kit::cancel::CancelToken;
use aria_kit::speaker::{Embedder, Heard, HeardUpdate};
use aria_kit::transcription::check_cancelled;
use aria_kit::transcription::layer::{
    self, Backtrack, Layer, Layered, RemoveHesitations, RemoveRepeats,
};
use aria_kit::voice_activity::{SileroVad, VoiceActivityDetector};

use crate::audio::{AudioCapture, CaptureError, RecordingMode, RecordingReader, StopSignal};
use crate::delivery::{DeliveryOutcome, Placement};
use crate::primitives::error_chain;
use crate::speaker::{Label, SegmentLabeler, SegmentReader, SpeakerName, Verdict};
use crate::transcription::{
    Language, Mode, Transcriber, Transcript, Transcription, TranscriptionError,
};
use crate::vocabulary::Vocabulary;

use super::Meter;
use super::speech::{Scan, Tally};
use super::{
    CaptionSnapshot, DeliveryMode, DictationError, DictationEventEmitter, DictationResult,
    DictationSettings, DictationStage, ExecutionControl, GhostSnapshot, StopMode, TranscriptSink,
    Utterance,
};

/// Records a press, hears it, and publishes each segment to its sinks.
/// Where the text goes — history, the cursor — is the sinks' business.
pub(crate) struct DictationPipeline<C, T, M: Mode, E, V, S, L = ()> {
    capture: C,
    transcription: Transcription<T, M>,
    /// Whose voice each segment is in. One reader across presses, so a
    /// voice keeps its label from one dictation to the next. Locked: presses
    /// come one at a time, but not from one thread.
    reader: Mutex<SegmentReader<E>>,
    /// `None` counts every recording as speech. Locked for the same reason
    /// as the reader.
    tally: Mutex<Option<Tally<V>>>,
    /// Locked for the same reason as the reader.
    sinks: Mutex<S>,
    cleanup: bool,
    backtrack: bool,
    layers: L,
}

impl<C, T, M: Mode, E, S> DictationPipeline<C, T, M, E, SileroVad, S> {
    pub(crate) fn new(
        capture: C,
        transcription: Transcription<T, M>,
        reader: SegmentReader<E>,
        sinks: S,
    ) -> Self {
        Self {
            capture,
            transcription,
            reader: Mutex::new(reader),
            tally: Mutex::new(None),
            sinks: Mutex::new(sinks),
            cleanup: false,
            backtrack: false,
            layers: (),
        }
    }
}

impl<C, T, M: Mode, E, V, S, L: Layer> DictationPipeline<C, T, M, E, V, S, L> {
    pub(crate) fn with<N: Layer>(
        self,
        layer: N,
    ) -> DictationPipeline<C, T, M, E, V, S, Layered<N, L>> {
        DictationPipeline {
            capture: self.capture,
            transcription: self.transcription,
            reader: self.reader,
            tally: self.tally,
            sinks: self.sinks,
            cleanup: self.cleanup,
            backtrack: self.backtrack,
            layers: Layered::new(self.layers, layer),
        }
    }

    pub(crate) fn cleaning(mut self, enabled: bool) -> Self {
        self.cleanup = enabled;
        self
    }

    pub(crate) fn backtracking(mut self, enabled: bool) -> Self {
        self.backtrack = enabled;
        self
    }

    /// Types no words from a recording until `detector` hears speech in it.
    /// With `None`, every recording counts as speech.
    pub(crate) fn hearing_speech<W: VoiceActivityDetector>(
        self,
        detector: Option<W>,
    ) -> DictationPipeline<C, T, M, E, W, S, L> {
        DictationPipeline {
            capture: self.capture,
            transcription: self.transcription,
            reader: self.reader,
            tally: Mutex::new(detector.map(Tally::new)),
            sinks: self.sinks,
            cleanup: self.cleanup,
            backtrack: self.backtrack,
            layers: self.layers,
        }
    }

    fn reader(&self) -> MutexGuard<'_, SegmentReader<E>> {
        self.reader
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn tally(&self) -> MutexGuard<'_, Option<Tally<V>>> {
        self.tally
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn sinks(&self) -> MutexGuard<'_, S> {
        self.sinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn finalize(
        &self,
        transcript: Transcript,
        vocabulary: Option<&Vocabulary>,
        cancel: &CancelToken,
    ) -> Result<Transcript, TranscriptionError> {
        let mut text = transcript.into_text();
        if self.backtrack {
            Backtrack.apply(&mut text, cancel)?;
        }
        apply_corrections(&mut text, self.cleanup, vocabulary, cancel)?;
        // Cleanup can leave nothing, and then this fails as `NoSpeech`.
        let transcript = Transcript::new(text)?;
        if !self.layers.enabled() {
            return Ok(transcript);
        }
        let started = Instant::now();
        let mut text = transcript.as_str().to_owned();
        let result = self.layers.apply(&mut text, cancel);
        check_cancelled(cancel)?;
        let failure = match &result {
            Err(TranscriptionError::Processing(layer::Error::Failed(error))) => error
                .downcast_ref::<super::CleanupError>()
                .map(super::CleanupError::category),
            Err(TranscriptionError::Processing(layer::Error::TooLarge)) => Some("processing_size"),
            _ => None,
        };
        if let Some(category) = failure {
            tracing::warn!(
                category,
                elapsed_ms = started.elapsed().as_millis(),
                "dictation cleanup failed; delivering deterministic text"
            );
            return Ok(transcript);
        }
        result?;
        Transcript::new(text)
    }
}

impl<C, T, M: Mode, E, V, S, L: Layer> DictationPipeline<C, T, M, E, V, S, L>
where
    E: aria_kit::speaker::Embedder,
{
    /// Replaces the enrolled profiles the next press is judged against. A
    /// voice enrolled while the daemon ran is invisible until this is called.
    pub(crate) fn refresh_enrolled(&self, enrolled: Vec<aria_kit::speaker::Profile>) {
        self.reader().labeler_mut().set_enrolled(enrolled);
    }
}

impl<C, T: Transcriber, M: Mode, E: Embedder, V: VoiceActivityDetector, S, L: Layer>
    DictationPipeline<C, T, M, E, V, S, L>
{
    /// Transcribes caller-provided audio with the shared engine and speaker filter.
    /// Returns text without publishing it to sinks, and the engine a delegated
    /// operation ran on.
    pub(crate) fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        own_voice_only: bool,
        vocabulary: Option<&Vocabulary>,
        cancel: &CancelToken,
    ) -> Result<(Transcript, Option<&'static str>), DictationError> {
        if cancel.is_cancelled() {
            return Err(DictationError::Cancelled);
        }
        if !Scan::new(self.tally().as_mut(), recording).held_speech() {
            return Err(DictationError::NoSpeech);
        }
        let mut reader = self.reader();
        reader.rewind();
        reader.labeler_mut().set_only_own_voice(own_voice_only);
        // Filter the complete recording before decoding, using predicted voice matches.
        let mut gated = own_voice_recording(recording, reader.labeler_mut())?;
        if own_voice_only && gated.is_none() {
            return Err(DictationError::NoSpeech);
        }
        drop(reader);
        if cancel.is_cancelled() {
            return Err(DictationError::Cancelled);
        }
        let heard = gated.as_mut().unwrap_or(recording);
        let mut operation = self.transcription.start_with(language, cancel)?;
        let served_by = operation.served_by();
        let transcript = operation.finish(heard)?;
        let transcript = self.finalize(transcript, vocabulary, cancel)?;
        Ok((transcript, served_by))
    }
}

impl<C, T, M, E, V, S, L: Layer> DictationPipeline<C, T, M, E, V, S, L>
where
    C: AudioCapture,
    T: Transcriber,
    M: Mode,
    E: Embedder,
    V: VoiceActivityDetector,
    S: TranscriptSink,
    S::Error: Into<DictationError>,
{
    pub(crate) fn execute(
        &self,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
        vocabulary: Option<&Vocabulary>,
    ) -> Result<DictationResult, DictationError> {
        let captions = !settings.transcript_only;
        if captions {
            events.emit(CaptionSnapshot::empty(control.recording_id(), false));
        }
        let mut sinks = self.sinks();
        let result = self.execute_stages(settings, control, events, &mut *sinks, vocabulary);
        sinks.dictation_finished();
        if captions {
            events.emit(CaptionSnapshot::empty(control.recording_id(), true));
        }
        if let Err(error) = &result {
            // Capture may still be letting go. The terminal stage waits for
            // it, so it never lands ahead of the `Transcribing` the guard
            // announces.
            control.await_capture_release();
            events.emit(DictationStage::ended_by(error.into()));
        }
        result
    }

    /// Recording and transcription overlap: capture runs on its own thread
    /// while streaming transcription takes chunks as they arrive. Both stages
    /// are therefore logged once transcription returns,
    /// which is the first moment the recording's end is known here.
    fn execute_stages(
        &self,
        settings: DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
        sinks: &mut S,
        vocabulary: Option<&Vocabulary>,
    ) -> Result<DictationResult, DictationError> {
        check_execution(control)?;

        // A transcript handed back has nowhere to stream to.
        if settings.delivery_mode == DeliveryMode::Streaming
            && (!M::INCREMENTAL || settings.transcript_only)
        {
            return Err(DictationError::StreamingDeliveryUnavailable);
        }

        let stage_started = Instant::now();
        let mut recording = self.start_recording(&settings, control, events, stage_started)?;

        let result = (|| {
            sinks
                .dictation_started(&settings, control)
                .map_err(|error| stop_recording(control, map_sink_error(error, control)))
                .map_err(|error| log_delivery_error(stage_started.elapsed(), error))?;
            tracing::info!(stage = "transcription", "dictation stage started");
            // Held only for the transcription stage: the meter ends with
            // capture, and the pill has its next stage to draw by then.
            let meter = Meter::watching(&recording, control, events);
            // Transcription can fail before it has read the press to its end —
            // an unsupported language is refused on the first line. Capture would
            // otherwise keep running until its own timeout, and the meter above
            // would wait on it before anything could stop it.
            let heard = self
                .hear(
                    &settings,
                    &mut recording,
                    control,
                    events,
                    sinks,
                    vocabulary,
                )
                .map_err(|error| stop_recording(control, error));
            drop(meter);
            log_capture_stage(&recording, stage_started);
            let (transcript, mut completion) = heard?;
            tracing::info!(
                stage = "transcription",
                status = "completed",
                elapsed_ms = stage_started.elapsed().as_millis(),
                after_recording_ms = recording
                    .ended_at()
                    .map(|ended| ended.elapsed().as_millis()),
                "dictation stage completed"
            );
            // Streaming has typed every segment already; nothing is left to
            // deliver, so the stage would only be a word on the pill.
            if settings.delivery_mode == DeliveryMode::Complete {
                check_execution(control)?;
                events.emit(DictationStage::Delivering);
                deliver_transcript(&transcript, &mut completion, &recording, control, sinks)?;
            }
            Ok(DictationResult {
                text: transcript.into_text(),
                own_voice: self.reader().labeler().judges_own_voice(),
            })
        })();
        if result.is_err() {
            control.request_recording_stop();
        }
        sinks.recording_finished(&mut recording, result.as_ref().map(|_| ()));
        if let Ok(transcript) = &result {
            events.emit(DictationStage::delivered(
                sinks.delivered(),
                &transcript.text,
            ));
        }
        result
    }

    fn start_recording(
        &self,
        settings: &DictationSettings,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
        stage_started: Instant,
    ) -> Result<RecordingReader, DictationError> {
        let guard = control.start_recording(events)?;
        let mode = match settings.stop_mode {
            StopMode::OnSilence => RecordingMode::StopOnSilence {
                trailing_silence: settings.silence_duration,
            },
            StopMode::Manual => RecordingMode::UntilStopped,
        };
        let recording = self
            .capture
            .start(mode, StopSignal::from_fn(move || guard.should_stop()))
            .map_err(|error| map_capture_error(error, control))
            .map_err(|error| log_stage_error("recording", stage_started.elapsed(), error))?;
        tracing::info!(stage = "recording", "dictation stage started");
        Ok(recording)
    }

    /// Hears the dictation to its end, publishing each segment as it
    /// finalizes. The completion comes back still pending in complete
    /// delivery, where the caller publishes it as the delivery stage.
    fn hear(
        &self,
        settings: &DictationSettings,
        recording: &mut RecordingReader,
        control: &Arc<ExecutionControl>,
        events: &DictationEventEmitter,
        sinks: &mut S,
        vocabulary: Option<&Vocabulary>,
    ) -> Result<(Transcript, Completion), DictationError> {
        let started_at = Instant::now();
        let started = |error| log_transcription_error(error, control, started_at);
        let cancel = {
            let control = Arc::clone(control);
            CancelToken::from_fn(move || control.should_stop())
        };
        let mut reader = self.reader();
        reader.rewind();
        reader
            .labeler_mut()
            .set_only_own_voice(settings.own_voice_only);
        let mut tally = self.tally();
        let mut scan = Scan::new(tally.as_mut(), recording);
        let mut operation = self
            .transcription
            .start_with(&settings.recognition_language, &cancel)
            .map_err(started)?;
        if let Some(engine) = operation.served_by() {
            sinks.served_by(engine);
        }
        // Complete delivery filters audio before decoding. Streaming instead
        // checks the predicted voice for each finalized segment.
        let mut gated = match settings.delivery_mode {
            DeliveryMode::Complete => own_voice_recording(recording, reader.labeler_mut())?,
            DeliveryMode::Streaming => None,
        };
        if gated.is_some() {
            // Own-voice filtering already read the whole press, so judge it now.
            scan.hear_rest();
        }
        let recording = gated.as_mut().unwrap_or(recording);
        let mut published = Published::new(settings, control, events, started_at);
        let previews = (published.caption.is_some() || published.streaming)
            && M::INCREMENTAL
            && !settings.own_voice_only;
        // Words finalized before the press held speech are the decoder
        // guessing at silence. The operation's transcript still carries them.
        let mut guessed = 0_usize;
        let mut trusted = Vec::new();
        loop {
            let update = if previews {
                reader.next_update(&mut operation, recording)
            } else {
                reader
                    .next(&mut operation, recording)
                    .map(|heard| heard.map(HeardUpdate::Finalized))
            }
            .map_err(|error| published.failure.take().unwrap_or_else(|| started(error)))?;
            scan.hear_through(operation.read_to());
            match update {
                Some(HeardUpdate::Finalized(_)) if !scan.heard() => guessed += 1,
                Some(HeardUpdate::Provisional(_)) if !scan.heard() => {}
                Some(HeardUpdate::Finalized(heard)) => {
                    trusted.push(heard.segment.as_str().to_owned());
                    let mut text = heard.segment.as_str().to_owned();
                    apply_corrections(&mut text, self.cleanup, vocabulary, &cancel).map_err(
                        |error| published.failure.take().unwrap_or_else(|| started(error)),
                    )?;
                    let speaker = reader.labeler().heard_speaker().cloned();
                    published.segment(heard, &text, speaker, sinks)?;
                }
                // A preview is the decoder's guess; only a finalized segment
                // is worth correcting.
                Some(HeardUpdate::Provisional(text)) => published.provisional(&text, sinks),
                None => break,
            }
        }
        if !scan.held_speech() {
            return Err(started(TranscriptionError::NoSpeech));
        }
        let transcript = operation
            .finish(recording)
            .map_err(|error| published.failure.take().unwrap_or_else(|| started(error)))?;
        let transcript = if guessed > 0 {
            tracing::debug!(
                segments = guessed,
                "segments decoded before speech was heard dropped"
            );
            Transcript::new(trusted.join(" ")).map_err(started)?
        } else {
            transcript
        };
        let transcript = match settings.delivery_mode {
            DeliveryMode::Complete => self
                .finalize(transcript, vocabulary, &cancel)
                .map_err(started)?,
            DeliveryMode::Streaming => transcript,
        };
        published.finish(transcript, sinks)
    }
}

/// Calls each layer directly rather than through `Layered`, whose 64 KiB
/// bound would stop deterministic cleanup on a long transcript.
fn apply_corrections(
    text: &mut String,
    cleanup: bool,
    vocabulary: Option<&Vocabulary>,
    cancel: &CancelToken,
) -> Result<(), TranscriptionError> {
    check_cancelled(cancel)?;
    if cleanup {
        RemoveHesitations.apply(text, cancel)?;
        RemoveRepeats.apply(text, cancel)?;
    }
    if let Some(corrected) = vocabulary.and_then(|vocabulary| vocabulary.correct(text)) {
        *text = corrected;
    }
    check_cancelled(cancel)
}

/// What one dictation has put out so far, with the events and logs each
/// step owes.
struct Published<'a> {
    streaming: bool,
    kept: Vec<String>,
    failure: Option<DictationError>,
    caption: Option<CaptionSnapshot>,
    completion: Completion,
    control: &'a ExecutionControl,
    events: &'a DictationEventEmitter,
    started_at: Instant,
}

impl<'a> Published<'a> {
    fn new(
        settings: &DictationSettings,
        control: &'a ExecutionControl,
        events: &'a DictationEventEmitter,
        started_at: Instant,
    ) -> Self {
        Self {
            streaming: settings.delivery_mode == DeliveryMode::Streaming,
            kept: Vec::new(),
            failure: None,
            caption: (!settings.transcript_only)
                .then(|| CaptionSnapshot::empty(control.recording_id(), false)),
            completion: Completion::Pending,
            control,
            events,
            started_at,
        }
    }

    /// `speaker` is the enrolled voice of the press so far, which clients
    /// show; the segment's own label is what history keeps.
    fn segment<S>(
        &mut self,
        heard: Heard,
        corrected: &str,
        speaker: Option<SpeakerName>,
        sinks: &mut S,
    ) -> Result<(), DictationError>
    where
        S: TranscriptSink,
        S::Error: Into<DictationError>,
    {
        if let Some(caption) = &mut self.caption {
            caption.provisional = Arc::from("");
        }
        // A streamed segment of only fillers is nothing to type.
        if corrected.is_empty() {
            return Ok(());
        }
        let Some((label, embedding)) = voice(heard.verdict) else {
            tracing::debug!("segment in another voice not delivered");
            return Ok(());
        };
        let first = self.kept.is_empty();
        self.kept.push(corrected.to_owned());
        if self.failure.is_some() {
            return Ok(());
        }
        let published = sinks.segment(Utterance {
            text: corrected,
            speaker: label.as_ref(),
            embedding: embedding.as_deref(),
            at: heard.at,
            span: heard.span,
        });
        if let Err(error) = published {
            let error = match map_sink_error(error, self.control) {
                // Text already landed, so typing stopped partway.
                DictationError::DeliveryTargetChanged if self.streaming && !first => {
                    DictationError::DeliveryInterrupted
                }
                error => error,
            };
            let error = log_delivery_error(self.started_at.elapsed(), error);
            if !self.streaming {
                return Err(error);
            }
            // Stop listening, then drain without typing, so history keeps
            // everything said up to here.
            self.control.request_recording_stop();
            self.failure = Some(error);
            self.events
                .emit(GhostSnapshot::empty(self.control.recording_id()));
            return Ok(());
        }
        if self.streaming && first {
            tracing::info!(
                stage = "delivery",
                elapsed_ms = self.started_at.elapsed().as_millis(),
                "dictation first segment transcribed"
            );
        }
        if let Some(caption) = &mut self.caption {
            let joined = if caption.committed.is_empty() {
                corrected.to_owned()
            } else {
                format!("{} {corrected}", caption.committed)
            };
            caption.committed = caption_suffix(&joined);
            self.events.emit(caption.clone());
        }
        self.events.emit(DictationStage::SegmentTranscribed {
            text: Arc::from(corrected),
            speaker,
        });
        if self.streaming {
            self.events.emit(DictationStage::SegmentDelivered);
            self.events
                .emit(GhostSnapshot::empty(self.control.recording_id()));
        }
        Ok(())
    }

    fn finish<S>(
        mut self,
        transcript: Transcript,
        sinks: &mut S,
    ) -> Result<(Transcript, Completion), DictationError>
    where
        S: TranscriptSink,
        S::Error: Into<DictationError>,
    {
        if !self.streaming {
            return Ok((transcript, self.completion));
        }
        let transcript = Transcript::new(self.kept.join(" "))
            .map_err(|error| log_transcription_error(error, self.control, self.started_at))?;
        let completed = self.completion.publish(Some(&transcript), sinks);
        if let Some(error) = self.failure.take() {
            if let Err(error) = completed {
                let error: DictationError = error.into();
                tracing::warn!(
                    error = %error_chain(&error),
                    "completion not published after a failed segment"
                );
            }
            return Err(error);
        }
        completed.map_err(|error| {
            log_delivery_error(
                self.started_at.elapsed(),
                map_sink_error(error, self.control),
            )
        })?;
        tracing::info!(
            stage = "delivery",
            status = "completed",
            elapsed_ms = self.started_at.elapsed().as_millis(),
            "dictation streaming delivery completed"
        );
        Ok((transcript, self.completion))
    }

    fn provisional<S: TranscriptSink>(&mut self, text: &str, sinks: &S) {
        if let Some(caption) = &mut self.caption {
            caption.provisional = caption_suffix(text);
            self.events.emit(caption.clone());
        }
        if self.streaming && self.failure.is_none() {
            self.events.emit(GhostSnapshot {
                recording: self.control.recording_id(),
                text: caption_suffix(text),
                // Empty text skips the Accessibility read.
                placement: if text.is_empty() {
                    None
                } else {
                    sinks.placement()
                },
            });
        }
    }
}

const CAPTION_CHARACTERS: usize = 2_048;

fn caption_suffix(text: &str) -> Arc<str> {
    let start = text
        .char_indices()
        .rev()
        .nth(CAPTION_CHARACTERS - 1)
        .map_or(0, |(index, _)| index);
    Arc::from(&text[start..])
}

/// Who a segment is attributed to, or `None` for a voice that must not be
/// published. A dictation is deliberate, so noise and unknown voices pass;
/// only a stranger under own-voice is kept out.
fn voice(verdict: Verdict) -> Option<(Option<Label>, Option<Vec<f32>>)> {
    match verdict {
        Verdict::Keep { label, embedding } => Some((label, embedding)),
        Verdict::Drop => Some((None, None)),
        Verdict::Stranger => None,
    }
}

fn log_transcription_error(
    error: TranscriptionError,
    control: &ExecutionControl,
    stage_started: Instant,
) -> DictationError {
    let stage = match &error {
        TranscriptionError::Capture(_) => "recording",
        TranscriptionError::Model { .. }
        | TranscriptionError::Load { .. }
        | TranscriptionError::Inference { .. }
        | TranscriptionError::UnsupportedLanguage { .. }
        | TranscriptionError::UnsupportedAudioInput { .. }
        | TranscriptionError::UnsupportedAudioPath { .. }
        | TranscriptionError::UnsupportedBackend { .. }
        | TranscriptionError::UnsupportedDevice { .. }
        | TranscriptionError::NoNativeStream
        | TranscriptionError::NoDelegatedOperation
        | TranscriptionError::LoadPanicked
        | TranscriptionError::Cancelled
        | TranscriptionError::NoSpeech
        | TranscriptionError::InvalidSegment
        | TranscriptionError::Processing(_) => "transcription",
    };
    log_stage_error(
        stage,
        stage_started.elapsed(),
        map_transcription_error(error, control),
    )
}

/// Complete delivery: the whole transcript reaches the sinks at once, history
/// first, so a cursor that then refuses it cannot lose the utterance.
fn deliver_transcript<S>(
    transcript: &Transcript,
    completion: &mut Completion,
    recording: &RecordingReader,
    control: &ExecutionControl,
    sinks: &mut S,
) -> Result<(), DictationError>
where
    S: TranscriptSink,
    S::Error: Into<DictationError>,
{
    tracing::info!(stage = "delivery", "dictation stage started");
    let stage_started = Instant::now();
    completion
        .publish(Some(transcript), sinks)
        .map_err(|error| map_sink_error(error, control))
        .map_err(|error| log_delivery_error(stage_started.elapsed(), error))?;
    tracing::info!(
        stage = "delivery",
        status = "completed",
        elapsed_ms = stage_started.elapsed().as_millis(),
        after_recording_ms = recording
            .ended_at()
            .map(|ended| ended.elapsed().as_millis()),
        "dictation stage completed"
    );
    Ok(())
}

/// A finished recording cut down to the enrolled voice, for a decode that
/// hears everything at once. `None` when everything stays, which leaves the
/// recording as it is; `NoSpeech` when nothing in that voice was said.
/// Saved beside the original when that one is a file, so a decoder that
/// reads files can take it.
fn own_voice_recording<E: Embedder>(
    recording: &mut RecordingReader,
    labeler: &mut SegmentLabeler<E>,
) -> Result<Option<RecordingReader>, DictationError> {
    if !labeler.only_own_voice() {
        return Ok(None);
    }
    recording.fill().map_err(DictationError::Capture)?;
    let Some(kept) = labeler.own_voice_audio(recording.samples()) else {
        return Ok(None);
    };
    if kept.is_empty() {
        return Err(DictationError::NoSpeech);
    }
    let directory = recording
        .finished_audio()
        .map_err(DictationError::Capture)?
        .path()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf);
    match directory {
        Some(directory) => RecordingReader::from_samples_in(&directory, &kept)
            .map(Some)
            .map_err(DictationError::Capture),
        None => Ok(Some(RecordingReader::from_samples(&kept))),
    }
}

enum Completion {
    Pending,
    Published,
}

impl Completion {
    fn publish<S: TranscriptSink>(
        &mut self,
        transcript: Option<&Transcript>,
        sinks: &mut S,
    ) -> Result<(), S::Error> {
        if matches!(self, Self::Published) {
            return Ok(());
        }
        let Some(transcript) = transcript else {
            return Ok(());
        };
        sinks.completed(Utterance::anonymous(transcript.as_str()))?;
        *self = Self::Published;
        Ok(())
    }
}

/// Two sinks in order. A failure in the first hides that call from the
/// second, so history is written before the cursor moves.
impl<A, B> TranscriptSink for (A, B)
where
    A: TranscriptSink,
    B: TranscriptSink,
    A::Error: Into<DictationError>,
    B::Error: Into<DictationError>,
{
    type Error = DictationError;

    fn dictation_started(
        &mut self,
        settings: &DictationSettings,
        control: &Arc<ExecutionControl>,
    ) -> Result<(), DictationError> {
        self.0
            .dictation_started(settings, control)
            .map_err(Into::into)?;
        self.1
            .dictation_started(settings, control)
            .map_err(Into::into)
    }

    fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), DictationError> {
        self.0.segment(utterance.clone()).map_err(Into::into)?;
        self.1.segment(utterance).map_err(Into::into)
    }

    fn completed(&mut self, utterance: Utterance<'_>) -> Result<(), DictationError> {
        self.0.completed(utterance.clone()).map_err(Into::into)?;
        self.1.completed(utterance).map_err(Into::into)
    }

    fn served_by(&mut self, engine: &'static str) {
        self.0.served_by(engine);
        self.1.served_by(engine);
    }

    fn delivered(&self) -> Option<DeliveryOutcome> {
        self.0.delivered().or(self.1.delivered())
    }

    fn placement(&self) -> Option<Placement> {
        self.0.placement().or_else(|| self.1.placement())
    }

    fn dictation_finished(&mut self) {
        self.0.dictation_finished();
        self.1.dictation_finished();
    }

    fn recording_finished(
        &mut self,
        recording: &mut RecordingReader,
        result: Result<(), &DictationError>,
    ) {
        self.0.recording_finished(recording, result);
        self.1.recording_finished(recording, result);
    }
}

fn stop_recording(control: &ExecutionControl, error: DictationError) -> DictationError {
    control.request_recording_stop();
    error
}

fn check_execution(control: &ExecutionControl) -> Result<(), DictationError> {
    if control.is_cancelled() {
        return Err(DictationError::Cancelled);
    }
    if control.deadline_expired() {
        return Err(DictationError::OverallTimeout(control.overall_timeout()));
    }
    Ok(())
}

/// The press ends inside transcription, so the recording stage is logged from
/// what the reader saw rather than from when this runs.
fn log_capture_stage(recording: &RecordingReader, stage_started: Instant) {
    let Some(ended_at) = recording.ended_at() else {
        tracing::info!(
            stage = "recording",
            status = "unfinished",
            elapsed_ms = stage_started.elapsed().as_millis(),
            "dictation stage finished"
        );
        return;
    };
    tracing::info!(
        stage = "recording",
        status = "completed",
        elapsed_ms = ended_at.duration_since(stage_started).as_millis(),
        audio_ms = recording.duration().as_millis(),
        "dictation stage completed"
    );
}

fn log_delivery_error(elapsed: Duration, error: DictationError) -> DictationError {
    log_stage_error("delivery", elapsed, error)
}

fn log_stage_error(
    stage: &'static str,
    elapsed: Duration,
    error: DictationError,
) -> DictationError {
    tracing::info!(
        stage,
        status = error.status(),
        elapsed_ms = elapsed.as_millis(),
        "dictation stage finished"
    );
    error
}

fn map_transcription_error(
    error: TranscriptionError,
    control: &ExecutionControl,
) -> DictationError {
    if control.is_cancelled() {
        return DictationError::Cancelled;
    }
    match error {
        TranscriptionError::Cancelled if control.deadline_expired() => {
            DictationError::OverallTimeout(control.overall_timeout())
        }
        TranscriptionError::Capture(error) => map_capture_error(error, control),
        error => error.into(),
    }
}

fn map_capture_error(error: CaptureError, control: &ExecutionControl) -> DictationError {
    if control.is_cancelled() {
        return DictationError::Cancelled;
    }
    match error {
        CaptureError::DeadlineExceeded(timeout) => DictationError::RecordingTimeout(timeout),
        _error if control.deadline_expired() => {
            DictationError::OverallTimeout(control.overall_timeout())
        }
        error => error.into(),
    }
}

/// A cancelled press reports the cancel, whatever a sink said; a sink that
/// gave up on the deadline reports the timeout.
fn map_sink_error(error: impl Into<DictationError>, control: &ExecutionControl) -> DictationError {
    if control.is_cancelled() {
        return DictationError::Cancelled;
    }
    match error.into() {
        DictationError::Cancelled if control.deadline_expired() => {
            DictationError::OverallTimeout(control.overall_timeout())
        }
        error => error,
    }
}

#[cfg(test)]
mod tests {
    /// Dictation's own tests run without a speaker model, so every utterance
    /// is unattributed.
    fn unlabeled() -> SegmentReader<aria_speaker::wespeaker::WeSpeaker> {
        SegmentReader::new(crate::speaker::SegmentLabeler::new(None, None))
    }

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use aria_kit::cancel::CancelToken;
    use aria_kit::speaker::{Diarizer, SpeakerError};

    use crate::audio::{
        Recording, RecordingWriter, ReplayPace, WavReplayCapture, canonical_fixture, fixture_of,
    };
    use crate::delivery::{DeliveryError, OutputMode, TextDelivery};
    use crate::dictation::testing::{
        CallLog, Loud, PLACED, PlacedDelivery, RecordingDelivery, settings,
    };
    use crate::dictation::{Cursor, RecordingId};
    use crate::rpc::RpcErrorCode;
    use crate::transcription::{
        Cadence, ChunkStream, Emitted, Hypothesis, Language, Stream, Streaming, Transcription,
        TranscriptionAudio,
    };

    use super::*;

    struct Answering(&'static str);

    struct FinalLayer<F>(F);

    impl<F> Layer for FinalLayer<F>
    where
        F: Fn(&mut String, &CancelToken) -> Result<(), TranscriptionError> + Send + Sync,
    {
        fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
            (self.0)(text, cancel)
        }
    }

    struct Parts(&'static [&'static str]);

    impl Transcriber for Parts {
        fn validate_language(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn transcribe_with(
            &self,
            _: TranscriptionAudio<'_>,
            _: &Language,
            _: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            panic!("native stream expected")
        }

        fn open_stream(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
            struct Chunks(std::iter::Copied<std::slice::Iter<'static, &'static str>>);
            impl ChunkStream for Chunks {
                fn chunk_samples(&self) -> usize {
                    16_000
                }
                fn feed(&mut self, _: &[f32]) -> Result<Emitted, TranscriptionError> {
                    Ok(Emitted {
                        words: Hypothesis::new(self.0.next().unwrap_or("")),
                        joins_previous: false,
                    })
                }
            }
            Ok(Some(Box::new(Chunks(self.0.iter().copied()))))
        }
    }

    /// Answers "hello" to any audio at all, and nothing to none.
    struct Echoing;

    impl Transcriber for Echoing {
        fn validate_language(
            &self,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn transcribe_with(
            &self,
            audio: TranscriptionAudio<'_>,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            let heard = match audio {
                TranscriptionAudio::Samples(samples) => !samples.is_empty(),
                TranscriptionAudio::Wav(_) => true,
            };
            Ok(Hypothesis::new(if heard { "hello" } else { "" }))
        }
    }

    impl Transcriber for Answering {
        fn validate_language(
            &self,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn transcribe_with(
            &self,
            _audio: TranscriptionAudio<'_>,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            Ok(Hypothesis::new(self.0))
        }

        fn open_stream(
            &self,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
            Ok(Some(Box::new(Self(self.0))))
        }
    }

    impl ChunkStream for Answering {
        fn chunk_samples(&self) -> usize {
            16_000
        }

        fn feed(&mut self, _chunk: &[f32]) -> Result<Emitted, TranscriptionError> {
            Ok(Emitted {
                words: Hypothesis::new(std::mem::take(&mut self.0)),
                joins_previous: false,
            })
        }
    }

    struct UnreachableSink;

    impl TranscriptSink for UnreachableSink {
        type Error = DictationError;

        fn segment(&mut self, _utterance: Utterance<'_>) -> Result<(), DictationError> {
            panic!("whole transcription cannot publish after rejection");
        }

        fn completed(&mut self, _utterance: Utterance<'_>) -> Result<(), DictationError> {
            panic!("whole transcription cannot publish after rejection");
        }
    }

    fn capture() -> (tempfile::TempDir, WavReplayCapture) {
        replay(canonical_fixture)
    }

    /// Enough audio for the labeler to judge a voice: 4 s.
    fn judged_capture() -> (tempfile::TempDir, WavReplayCapture) {
        replay(|directory| fixture_of(directory, 64_000))
    }

    fn replay(
        fixture: impl FnOnce(&std::path::Path) -> std::path::PathBuf,
    ) -> (tempfile::TempDir, WavReplayCapture) {
        let directory = tempfile::tempdir().expect("test directory is created");
        let fixture = fixture(directory.path());
        let capture = WavReplayCapture::new(
            directory.path(),
            Duration::from_secs(10),
            &fixture,
            ReplayPace::Immediate,
        )
        .expect("fixture is replayable");
        (directory, capture)
    }

    /// A speaker model that hears one fixed voice in every window.
    struct Voice([f32; 2]);

    impl Embedder for Voice {
        fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
            Ok(Some(self.0.to_vec()))
        }

        fn dimension(&self) -> usize {
            2
        }
    }

    const OWN: [f32; 2] = [1.0, 0.0];
    const OTHER: [f32; 2] = [0.0, 1.0];

    /// A reader that hears `voice` everywhere, with one enrolled profile.
    fn hearing(voice: [f32; 2]) -> SegmentReader<Voice> {
        let enrolled = vec![aria_kit::speaker::enrolled_voice(
            "alex",
            crate::speaker::Role::Owner,
            OWN.to_vec(),
        )];
        SegmentReader::new(crate::speaker::SegmentLabeler::new(
            None,
            Some(Diarizer::new(Voice(voice), enrolled)),
        ))
    }

    fn own_voice_only(mut settings: DictationSettings) -> DictationSettings {
        settings.own_voice_only = true;
        settings
    }

    fn caption_events() -> (
        DictationEventEmitter,
        std::sync::mpsc::Receiver<CaptionSnapshot>,
    ) {
        let (send, receive) = std::sync::mpsc::channel();
        // Unbounded and on the emitting thread: the daemon's subscription
        // drops events once 16 wait, and a slow run fills that with levels.
        let events = DictationEventEmitter::forwarded_by(move |event| {
            if let super::super::DictationEvent::Caption(caption) = event {
                let _ = send.send(caption);
            }
        });
        (events, receive)
    }

    impl<C, T, M, E, V, S, L> DictationPipeline<C, T, M, E, V, S, L>
    where
        C: AudioCapture,
        T: Transcriber,
        M: Mode,
        E: Embedder,
        V: VoiceActivityDetector,
        S: TranscriptSink,
        S::Error: Into<DictationError>,
        L: Layer,
    {
        /// One manual-stop press that nothing observes or corrects.
        fn run(&self, settings: DictationSettings) -> Result<DictationResult, DictationError> {
            self.run_observed(settings, &DictationEventEmitter::unsubscribed())
        }

        fn run_observed(
            &self,
            settings: DictationSettings,
            events: &DictationEventEmitter,
        ) -> Result<DictationResult, DictationError> {
            self.execute(
                settings,
                &ExecutionControl::for_test(StopMode::Manual),
                events,
                None,
            )
        }
    }

    /// What the unlabeled reader hears in a short loud recording when the
    /// engine answers `text`.
    fn heard(directory: &std::path::Path, text: &'static str) -> Heard {
        let transcription = Transcription::complete_samples(Answering(text));
        let mut operation = transcription.start(&Language::Automatic).unwrap();
        let mut recording = RecordingReader::from_samples_in(directory, &[0.5; 1600]).unwrap();
        unlabeled()
            .next(&mut operation, &mut recording)
            .unwrap()
            .unwrap()
    }

    /// Joins every `deliver:` entry into what the focused field ends up holding.
    fn delivered(log: &[String]) -> String {
        log.iter()
            .filter_map(|entry| entry.strip_prefix("deliver:"))
            .collect()
    }

    const SPEECH: &[f32] = &[0.5; 8000];

    type Writer = Arc<Mutex<Option<RecordingWriter>>>;

    /// A live recording that starts with `opening`. [`Scripted`] pushes the rest.
    struct Live {
        writer: Writer,
        opening: &'static [f32],
    }

    impl AudioCapture for Live {
        fn start(
            &self,
            _mode: RecordingMode,
            _stop: StopSignal,
        ) -> Result<RecordingReader, CaptureError> {
            let (recording, mut push) = Recording::open();
            push.push(self.opening);
            *self.writer.lock().unwrap() = Some(push);
            Ok(recording)
        }
    }

    /// Decode `n` pushes `steps[n].0` into the live recording and answers
    /// `steps[n].1`. Past the script it ends the recording and answers
    /// `last`, so how many decodes a press takes is under the test's control.
    struct Scripted {
        writer: Writer,
        steps: &'static [(&'static [f32], &'static str)],
        last: &'static str,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Transcriber for Scripted {
        fn validate_language(
            &self,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn transcribe_with(
            &self,
            _audio: TranscriptionAudio<'_>,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut writer = self.writer.lock().unwrap();
            let Some(&(audio, text)) = self.steps.get(call) else {
                if let Some(writer) = writer.take() {
                    writer.finish();
                }
                return Ok(Hypothesis::new(self.last));
            };
            writer.as_mut().unwrap().push(audio);
            Ok(Hypothesis::new(text))
        }
    }

    fn scripted(
        opening: &'static [f32],
        steps: &'static [(&'static [f32], &'static str)],
        last: &'static str,
    ) -> (Live, Scripted) {
        let writer = Writer::default();
        let capture = Live {
            writer: Arc::clone(&writer),
            opening,
        };
        let engine = Scripted {
            writer,
            steps,
            last,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        (capture, engine)
    }

    /// Hears "turn lift", then revises it to "turn left", so the first
    /// decode is only a preview.
    fn revising<S>(
        sinks: S,
    ) -> DictationPipeline<
        Live,
        Scripted,
        Streaming,
        aria_speaker::wespeaker::WeSpeaker,
        SileroVad,
        S,
    > {
        let (capture, engine) = scripted(SPEECH, &[(SPEECH, "turn lift")], "turn left");
        DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(500, 20_000), engine),
            unlabeled(),
            sinks,
        )
    }

    #[test]
    fn incremental_caption_mode_consumes_previews_during_capture() {
        let log = CallLog::default();
        let pipeline = revising((log.history(), log.cursor()));
        let (events, snapshots) = caption_events();
        let result = pipeline
            .run_observed(
                settings(DeliveryMode::Complete, OutputMode::Insert),
                &events,
            )
            .unwrap();
        drop(events);
        let snapshots: Vec<_> = snapshots.iter().collect();
        assert!(
            snapshots
                .iter()
                .any(|snapshot| snapshot.provisional.as_ref() == "turn lift")
        );
        assert!(
            snapshots
                .iter()
                .any(|snapshot| snapshot.committed.as_ref() == "turn left")
        );
        assert_eq!(result.text, "turn left");
        assert!(log.entries().iter().all(|entry| !entry.contains("lift")));
        assert_eq!(
            log.entries()
                .iter()
                .filter(|entry| entry.starts_with("deliver:"))
                .collect::<Vec<_>>(),
            [&"deliver:turn left".to_owned()]
        );
    }

    #[test]
    fn provisional_captions_replace_and_clear_without_reaching_sinks() {
        let (events, snapshots) = caption_events();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let settings = settings(DeliveryMode::Complete, OutputMode::Insert);
        let mut published = Published::new(&settings, &control, &events, Instant::now());
        let log = CallLog::default();
        let mut sinks = (log.history(), log.cursor());
        sinks.dictation_started(&settings, &control).unwrap();
        published.provisional("turn lift at the light", &sinks);
        published.provisional("turn left", &sinks);
        published.provisional("", &sinks);
        assert_eq!(log.entries(), ["prepare:Insert"]);

        let directory = tempfile::tempdir().unwrap();
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.5; 1600]).unwrap();
        let transcription = Transcription::complete_samples(Answering("turn left"));
        let mut operation = transcription
            .start_with(&Language::Automatic, &CancelToken::never())
            .unwrap();
        let heard = unlabeled()
            .next(&mut operation, &mut recording)
            .unwrap()
            .unwrap();
        published.provisional("uncommitted guess", &sinks);
        published
            .segment(heard, "turn left", None, &mut sinks)
            .unwrap();
        assert_eq!(log.entries(), ["prepare:Insert", "segment:turn left"]);
        let transcript = operation.finish(&mut recording).unwrap();
        let (transcript, mut completion) = published.finish(transcript, &mut sinks).unwrap();
        deliver_transcript(
            &transcript,
            &mut completion,
            &recording,
            &control,
            &mut sinks,
        )
        .unwrap();
        assert_eq!(
            log.entries(),
            [
                "prepare:Insert",
                "segment:turn left",
                "completed:turn left",
                "deliver:turn left"
            ]
        );
        drop(events);
        let snapshots: Vec<_> = snapshots.iter().collect();
        assert_eq!(snapshots[0].provisional.as_ref(), "turn lift at the light");
        assert_eq!(snapshots[1].provisional.as_ref(), "turn left");
        assert!(snapshots[2].provisional.is_empty());
        assert_eq!(snapshots.last().unwrap().committed.as_ref(), "turn left");
        assert!(snapshots.last().unwrap().provisional.is_empty());
    }

    #[test]
    fn caption_fields_keep_a_bounded_unicode_suffix() {
        let text = format!("old{}", "é🦀".repeat(2048));
        assert_eq!(caption_suffix(&text).as_ref(), "é🦀".repeat(1024));
        let (events, snapshots) = caption_events();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let settings = settings(DeliveryMode::Complete, OutputMode::Insert);
        let mut published = Published::new(&settings, &control, &events, Instant::now());
        let sink = CallLog::default().history();
        published.provisional(&text, &sink);
        drop(published);
        drop(events);
        assert_eq!(
            snapshots.iter().next().unwrap().provisional.chars().count(),
            2048
        );
    }

    #[test]
    fn caption_lifecycle_clears_success_no_speech_and_delivery_failure() {
        for (text, fail) in [("hello", false), ("", false), ("hello", true)] {
            let (_directory, capture) = capture();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::complete_samples(Answering(text)),
                unlabeled(),
                Cursor::new(RecordingDelivery {
                    log: CallLog::default(),
                    fail,
                }),
            );
            let (events, snapshots) = caption_events();
            let result = pipeline.run_observed(
                settings(DeliveryMode::Complete, OutputMode::Insert),
                &events,
            );
            assert_eq!(result.is_ok(), !text.is_empty() && !fail);
            drop(events);
            let snapshots: Vec<_> = snapshots.iter().collect();
            assert!(!snapshots.first().unwrap().ended);
            let terminal = snapshots.last().unwrap();
            assert!(terminal.ended);
            assert!(terminal.committed.is_empty() && terminal.provisional.is_empty());
            assert!(
                snapshots
                    .iter()
                    .all(|snapshot| snapshot.recording == RecordingId(0)
                        && snapshot.provisional.is_empty())
            );
            if !text.is_empty() {
                assert!(
                    snapshots
                        .iter()
                        .any(|snapshot| snapshot.committed.as_ref() == text)
                );
            }
        }
    }

    #[test]
    fn caption_failure_before_transcription_still_clears_the_recording() {
        struct Cancelled;
        impl Transcriber for Cancelled {
            fn validate_language(
                &self,
                _language: &Language,
                _cancel: &CancelToken,
            ) -> Result<(), TranscriptionError> {
                Err(TranscriptionError::Cancelled)
            }
            fn transcribe_with(
                &self,
                _audio: TranscriptionAudio<'_>,
                _language: &Language,
                _cancel: &CancelToken,
            ) -> Result<Hypothesis, TranscriptionError> {
                panic!("cancelled validation prevents decoding")
            }
        }
        let (_directory, capture) = capture();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Cancelled),
            unlabeled(),
            CallLog::default().history(),
        );
        let (events, snapshots) = caption_events();
        let result = pipeline.run_observed(
            settings(DeliveryMode::Complete, OutputMode::Insert),
            &events,
        );
        assert!(matches!(result, Err(DictationError::Cancelled)));
        drop(events);
        let snapshots: Vec<_> = snapshots.iter().collect();
        assert_eq!(snapshots.len(), 2);
        assert!(!snapshots[0].ended);
        assert!(snapshots[1].ended);
        assert!(
            snapshots
                .iter()
                .all(|snapshot| snapshot.committed.is_empty() && snapshot.provisional.is_empty())
        );
    }

    #[test]
    fn own_voice_captions_publish_only_accepted_committed_text() {
        for voice in [OWN, OTHER] {
            let (_directory, capture) = judged_capture();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::agreement(Cadence::from_millis(500, 20_000), Answering("hello")),
                hearing(voice),
                CallLog::default().history(),
            );
            let (events, snapshots) = caption_events();
            let result = pipeline.run_observed(
                own_voice_only(settings(DeliveryMode::Complete, OutputMode::Insert)),
                &events,
            );
            assert_eq!(result.is_ok(), voice == OWN);
            drop(events);
            let snapshots: Vec<_> = snapshots.iter().collect();
            assert!(
                snapshots
                    .iter()
                    .all(|snapshot| snapshot.provisional.is_empty())
            );
            assert_eq!(
                snapshots
                    .iter()
                    .any(|snapshot| snapshot.committed.as_ref() == "hello"),
                voice == OWN
            );
            assert!(snapshots.last().unwrap().ended);
        }
    }

    #[test]
    fn captions_follow_both_delivery_modes_but_not_a_transcript_only_run() {
        for (mode, transcript_only, captioned) in [
            (DeliveryMode::Streaming, false, true),
            (DeliveryMode::Complete, true, false),
        ] {
            let (_directory, capture) = capture();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::agreement(Cadence::from_millis(500, 20_000), Answering("hello")),
                unlabeled(),
                CallLog::default().history(),
            );
            let (events, snapshots) = caption_events();
            let mut settings = settings(mode, OutputMode::Insert);
            settings.transcript_only = transcript_only;
            pipeline.run_observed(settings, &events).unwrap();
            drop(events);
            let snapshots: Vec<_> = snapshots.iter().collect();
            assert_eq!(
                snapshots
                    .iter()
                    .any(|snapshot| snapshot.committed.as_ref() == "hello"),
                captioned
            );
            assert_eq!(snapshots.last().is_some_and(|last| last.ended), captioned);
        }
    }

    #[test]
    fn whole_transcription_rejects_segmented_delivery_before_capture() {
        let (_directory, capture) = capture();
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("hello")),
            unlabeled(),
            (UnreachableSink, log.cursor()),
        );
        let result = pipeline.run(settings(DeliveryMode::Streaming, OutputMode::Insert));

        assert!(matches!(
            result,
            Err(DictationError::StreamingDeliveryUnavailable)
        ));
        assert!(log.entries().is_empty());
    }

    #[test]
    fn a_transcript_only_press_hands_the_text_back_and_types_nothing() {
        let (_directory, capture) = capture();
        let typed = CallLog::default();
        let history = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("hello")),
            unlabeled(),
            (history.history(), typed.failing_cursor()),
        );
        let mut settings = settings(DeliveryMode::Complete, OutputMode::Insert);
        settings.transcript_only = true;

        let result = pipeline
            .run(settings)
            .expect("the press completes without typing");

        assert_eq!(result.text, "hello");
        assert!(typed.entries().is_empty(), "nothing prepared or typed");
        assert_eq!(
            history.entries().last().map(String::as_str),
            Some("completed:hello"),
            "history still hears the press"
        );
    }

    #[test]
    fn a_transcript_only_press_cannot_stream() {
        let (_directory, capture) = capture();
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            unlabeled(),
            (UnreachableSink, log.cursor()),
        );
        let mut settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
        settings.transcript_only = true;
        let result = pipeline.run(settings);

        assert!(matches!(
            result,
            Err(DictationError::StreamingDeliveryUnavailable)
        ));
        assert!(log.entries().is_empty());
    }

    #[test]
    fn a_committed_segment_reaches_history_before_the_cursor() {
        let (_directory, capture) = capture();
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            unlabeled(),
            (log.history(), log.cursor()),
        );
        let result = pipeline.run(settings(DeliveryMode::Streaming, OutputMode::Insert));

        assert_eq!(result.unwrap().text, "hello");
        assert_eq!(
            log.entries(),
            [
                "prepare:Insert",
                "segment:hello",
                "deliver:hello",
                "completed:hello"
            ]
        );
    }

    #[test]
    fn a_lexicon_keeps_streaming_delivery_history_and_result_in_agreement() {
        for (stream, expected_text, expected_log) in [
            (
                Stream::Agreement(Cadence::from_millis(100, 20_000)),
                "run db2",
                &[
                    "prepare:Insert",
                    "segment:run db2",
                    "deliver:run db2",
                    "completed:run db2",
                ][..],
            ),
            (
                Stream::Native,
                "run d b two",
                &[
                    "prepare:Insert",
                    "segment:run d b",
                    "deliver:run d b",
                    "segment:two",
                    "deliver: two",
                    "completed:run d b two",
                ][..],
            ),
        ] {
            let (_directory, capture) = capture();
            let memory = tempfile::tempdir().expect("a memory directory");
            std::fs::write(memory.path().join("lexicon.txt"), "db2\n").expect("the word list");
            let log = CallLog::default();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::stream(stream, Answering("run d b two")),
                unlabeled(),
                (log.history(), log.cursor()),
            );
            let matcher = crate::vocabulary::Vocabulary::compile(
                &[],
                crate::memory::Lexicon::new(memory.path())
                    .snapshot()
                    .unwrap(),
            )
            .unwrap();
            let control = ExecutionControl::for_test(StopMode::Manual);
            let settings = settings(DeliveryMode::Streaming, OutputMode::Insert);

            let result = pipeline.execute(
                settings,
                &control,
                &DictationEventEmitter::unsubscribed(),
                Some(&matcher),
            );

            assert_eq!(result.unwrap().text, expected_text);
            assert_eq!(log.entries(), expected_log);
        }
    }

    #[test]
    #[ignore = "collects repeated production-pipeline timings into an explicit output artifact"]
    fn vocabulary_streaming_delivery_benchmark() {
        use crate::vocabulary::{PersonalRules, Term, Vocabulary};
        use serde_json::json;

        struct TimedSink {
            started: Instant,
            delivered: Arc<Mutex<Vec<(u64, String)>>>,
        }
        impl TranscriptSink for TimedSink {
            type Error = DictationError;
            fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), Self::Error> {
                self.delivered.lock().unwrap().push((
                    self.started.elapsed().as_micros() as u64,
                    utterance.text.to_owned(),
                ));
                Ok(())
            }
            fn completed(&mut self, _utterance: Utterance<'_>) -> Result<(), Self::Error> {
                Ok(())
            }
        }

        let output = std::env::var_os("ARIA_VOCABULARY_PIPELINE_RESULTS")
            .expect("set ARIA_VOCABULARY_PIPELINE_RESULTS to a fresh artifact path");
        let matcher = Vocabulary::compile(
            &[Term {
                canonical: "aria-stt".into(),
                priority: 1,
            }],
            PersonalRules::default(),
        )
        .unwrap();
        let mut rows = Vec::new();
        for repetition in 0..32 {
            let conditions = if repetition % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for native in [false, true] {
                for enabled in conditions {
                    let (_directory, capture) = capture();
                    let delivered = Arc::new(Mutex::new(Vec::new()));
                    let started = Instant::now();
                    let pipeline = DictationPipeline::new(
                        capture,
                        Transcription::stream(
                            if native {
                                Stream::Native
                            } else {
                                Stream::Agreement(Cadence::from_millis(100, 20_000))
                            },
                            Answering("please open aria dash stt now"),
                        ),
                        unlabeled(),
                        TimedSink {
                            started,
                            delivered: Arc::clone(&delivered),
                        },
                    );
                    let vocabulary = enabled.then_some(&matcher);
                    let result = pipeline
                        .execute(
                            settings(DeliveryMode::Streaming, OutputMode::Insert),
                            &ExecutionControl::for_test(StopMode::Manual),
                            &DictationEventEmitter::unsubscribed(),
                            vocabulary,
                        )
                        .unwrap();
                    let total_us = started.elapsed().as_micros() as u64;
                    let segments = delivered.lock().unwrap();
                    let text = segments
                        .iter()
                        .map(|(_, text)| text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    assert_eq!(text, result.text, "delivery and result diverged");
                    assert_eq!(
                        text,
                        if enabled {
                            "please open aria-stt now"
                        } else {
                            "please open aria dash stt now"
                        }
                    );
                    let gaps: Vec<_> = segments
                        .windows(2)
                        .map(|pair| pair[1].0 - pair[0].0)
                        .collect();
                    rows.push(json!({"repetition": repetition, "warmup": repetition < 2, "native": native, "project": enabled, "first_sink_us": segments.first().unwrap().0, "total_us": total_us, "gaps_us": gaps, "segments": *segments, "text": text}));
                }
            }
        }
        aria_harness::artifact::write_json(std::path::Path::new(&output), &json!({
            "schema": 1,
            "provenance": aria_harness::artifact::Provenance::capture().unwrap(),
            "measurement": "production streaming pipeline and owned sink; immediate WAV replay, deterministic transcriber; not real-ASR or platform insertion latency",
            "rows": rows,
        })).unwrap();
    }

    #[test]
    fn correction_belongs_to_each_operation_and_never_leaks_to_the_next() {
        use crate::vocabulary::{PersonalRules, Term, Vocabulary};

        let hyphen = Vocabulary::compile(
            &[Term {
                canonical: "aria-stt".into(),
                priority: 1,
            }],
            PersonalRules::default(),
        )
        .unwrap();
        let underscore = Vocabulary::compile(
            &[Term {
                canonical: "aria_stt".into(),
                priority: 1,
            }],
            PersonalRules::default(),
        )
        .unwrap();
        for mode in [DeliveryMode::Complete, DeliveryMode::Streaming] {
            let (_directory, capture) = capture();
            let log = CallLog::default();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::agreement(
                    Cadence::from_millis(100, 20_000),
                    Answering("aria s t t"),
                ),
                unlabeled(),
                log.history(),
            );
            let cases: [(Option<&Vocabulary>, &str); 4] = [
                (Some(&hyphen), "aria-stt"),
                (Some(&underscore), "aria_stt"),
                (None, "aria s t t"),
                (Some(&hyphen), "aria-stt"),
            ];
            for (matcher, expected) in cases {
                let result = pipeline
                    .execute(
                        settings(mode, OutputMode::Insert),
                        &ExecutionControl::for_test(StopMode::Manual),
                        &DictationEventEmitter::unsubscribed(),
                        matcher,
                    )
                    .unwrap();
                assert_eq!(result.text, expected);
                assert_eq!(
                    log.entries().last().unwrap(),
                    &format!("completed:{expected}")
                );
            }
        }
    }

    #[test]
    fn backtracking_joins_segments_only_for_opted_in_complete_delivery() {
        for (parts, original, corrected) in [
            (
                &["um meet at 2", "actually 3"][..],
                "um meet at 2 actually 3",
                "um meet at 3",
            ),
            (
                &["send it to Ali scratch", "that to Sara"][..],
                "send it to Ali scratch that to Sara",
                "send it to Sara",
            ),
        ] {
            for mode in [DeliveryMode::Complete, DeliveryMode::Streaming] {
                for enabled in [false, true] {
                    let (_directory, capture) = judged_capture();
                    let log = CallLog::default();
                    let pipeline = DictationPipeline::new(
                        capture,
                        Transcription::stream(Stream::Native, Parts(parts)),
                        unlabeled(),
                        (log.history(), log.cursor()),
                    )
                    .backtracking(enabled)
                    .with(());
                    let result = pipeline
                        .run(settings(mode, OutputMode::InsertAndSubmit))
                        .unwrap();
                    let expected = if enabled && mode == DeliveryMode::Complete {
                        corrected
                    } else {
                        original
                    };
                    assert_eq!(result.text, expected);
                    let log = log.entries();
                    assert!(log.contains(&format!("completed:{expected}")));
                    assert!(
                        log.iter()
                            .filter(|entry| entry.starts_with("segment:"))
                            .count()
                            >= 2
                    );
                    assert_eq!(delivered(&log), expected);
                    assert_eq!(log.iter().filter(|entry| *entry == "submit").count(), 1);
                    assert_eq!(log.last().unwrap(), "submit");
                }
            }
        }
    }

    #[test]
    fn backtracking_waits_for_agreement_segments_to_complete() {
        for mode in [DeliveryMode::Complete, DeliveryMode::Streaming] {
            let (capture, engine) = scripted(
                SPEECH,
                &[
                    (SPEECH, "meet at 2"),
                    (SPEECH, "meet at 2"),
                    (SPEECH, "meet at 2 actually 3"),
                ],
                "meet at 2 actually 3",
            );
            let log = CallLog::default();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::agreement(Cadence::from_millis(100, 20_000), engine),
                unlabeled(),
                (log.history(), log.cursor()),
            )
            .backtracking(true);
            let result = pipeline.run(settings(mode, OutputMode::Insert)).unwrap();
            let expected = if mode == DeliveryMode::Complete {
                "meet at 3"
            } else {
                "meet at 2 actually 3"
            };
            assert_eq!(result.text, expected);
            let log = log.entries();
            assert!(log.contains(&"segment:meet at 2".to_owned()));
            assert!(log.contains(&"segment:actually 3".to_owned()));
            assert!(log.contains(&format!("completed:{expected}")));
            assert_eq!(delivered(&log), expected);
        }
    }

    #[test]
    fn backtracking_is_local_to_each_completed_transcript() {
        let pipeline = DictationPipeline::new(
            (),
            Transcription::complete_samples(Answering("")),
            unlabeled(),
            UnreachableSink,
        )
        .backtracking(true);
        for text in ["meet at 2", "actually 3", "scratch", "that to Sara"] {
            assert_eq!(
                pipeline
                    .finalize(Transcript::new(text).unwrap(), None, &CancelToken::never())
                    .unwrap()
                    .as_str(),
                text
            );
        }
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            pipeline.finalize(
                Transcript::new("meet at 2 actually 3").unwrap(),
                None,
                &cancel
            ),
            Err(TranscriptionError::Cancelled)
        ));
    }

    #[test]
    fn completion_layers_are_ordered_optional_and_skip_streaming() {
        for mode in [DeliveryMode::Complete, DeliveryMode::Streaming] {
            for enabled in [false, true] {
                let (_directory, capture) = judged_capture();
                let log = CallLog::default();
                let calls = Mutex::new(Vec::new());
                let layer = FinalLayer(|text: &mut String, _: &CancelToken| {
                    calls.lock().unwrap().push(text.clone());
                    *text = "Meet at 3".to_owned();
                    Ok(())
                });
                let punctuation = FinalLayer(|text: &mut String, _: &CancelToken| {
                    text.push('.');
                    Ok(())
                });
                let pipeline = DictationPipeline::new(
                    capture,
                    Transcription::stream(Stream::Native, Parts(&["um meet at 2", "actually 3"])),
                    unlabeled(),
                    (log.history(), log.cursor()),
                )
                .cleaning(true)
                .with(enabled.then_some(layer))
                .with(enabled.then_some(punctuation));
                let result = pipeline.run(settings(mode, OutputMode::Insert)).unwrap();
                let applied = enabled && mode == DeliveryMode::Complete;
                let expected = if applied {
                    "Meet at 3."
                } else {
                    "meet at 2 actually 3"
                };
                assert_eq!(result.text, expected);
                let calls = calls.lock().unwrap();
                assert_eq!(calls.len(), usize::from(applied));
                if applied {
                    assert_eq!(calls[0], "meet at 2 actually 3");
                }
                let log = log.entries();
                assert!(log.contains(&format!("completed:{expected}")));
                assert_eq!(
                    log.iter()
                        .filter(|entry| entry.starts_with("segment:"))
                        .count(),
                    3
                );
                assert_eq!(delivered(&log), expected);
            }
        }
    }

    #[test]
    fn full_recording_transcription_uses_completion_layers_and_cancellation() {
        let (directory, capture) = capture();
        let layer = FinalLayer(|text: &mut String, _: &CancelToken| {
            assert_eq!(text, "meet at 3");
            *text = "Meet at 3.".to_owned();
            Ok(())
        });
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("um meet at 2 actually 3")),
            unlabeled(),
            UnreachableSink,
        )
        .cleaning(true)
        .backtracking(true)
        .with(layer);
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.5; 1600]).unwrap();
        let cancel = CancelToken::new();
        assert_eq!(
            pipeline
                .transcribe(&mut recording, &Language::Automatic, false, None, &cancel,)
                .unwrap()
                .0
                .as_str(),
            "Meet at 3."
        );
        cancel.cancel();
        assert!(matches!(
            pipeline.transcribe(&mut recording, &Language::Automatic, false, None, &cancel,),
            Err(DictationError::Cancelled)
        ));
    }

    #[test]
    fn completion_layer_cancellation_and_unrelated_errors_prevent_delivery() {
        for cancelled in [true, false] {
            let (_directory, capture) = capture();
            let log = CallLog::default();
            let layer = FinalLayer(move |text: &mut String, _: &CancelToken| {
                *text = "never deliver this".to_owned();
                Err(if cancelled {
                    TranscriptionError::Cancelled
                } else {
                    TranscriptionError::NoNativeStream
                })
            });
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::complete_samples(Answering("meet at 2 actually 3")),
                unlabeled(),
                (log.history(), log.cursor()),
            )
            .backtracking(true)
            .with(layer);
            let result = pipeline.run(settings(
                DeliveryMode::Complete,
                OutputMode::InsertAndSubmit,
            ));
            if cancelled {
                assert!(matches!(result, Err(DictationError::Cancelled)));
            } else {
                assert!(matches!(result, Err(DictationError::Transcription(_))));
            }
            assert!(log.entries().iter().all(|entry| {
                !entry.starts_with("completed:")
                    && !entry.starts_with("deliver:")
                    && entry != "submit"
            }));
        }
    }

    #[test]
    fn oversized_completion_keeps_text_without_contacting_the_model() {
        let home = tempfile::tempdir().unwrap();
        let cleanup = super::super::LlmCleanup::new(
            "http://127.0.0.1:1/v1/chat/completions",
            "local",
            home.path(),
        )
        .unwrap();
        let pipeline = DictationPipeline::new(
            (),
            Transcription::complete_samples(Answering("")),
            unlabeled(),
            UnreachableSink,
        )
        .backtracking(true)
        .with(Some(cleanup));
        for bytes in [4097, 65537] {
            let prefix = "a".repeat(bytes);
            let text = format!("{prefix} meet at 2 actually 3");
            let transcript = pipeline
                .finalize(Transcript::new(&text).unwrap(), None, &CancelToken::never())
                .unwrap();
            assert_eq!(transcript.as_str(), format!("{prefix} meet at 3"));
        }
    }

    #[test]
    fn cleanup_failure_retains_deterministic_text_and_reports_only_its_category() {
        #[derive(Clone)]
        struct LogWriter(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for LogWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(Arc::clone(&diagnostics));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        // With one dispatch, the first thread to reach a callsite caches its
        // interest for all. A second keeps tracing asking each one.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let (_directory, capture) = capture();
        let log = CallLog::default();
        let layer = FinalLayer(|text: &mut String, _: &CancelToken| {
            assert_eq!(text, "meet Alice at 3");
            *text = "untrusted partial response".to_owned();
            Err(super::super::CleanupError::Timeout.into())
        });
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("um meet Alice at 2 actually 3")),
            unlabeled(),
            (log.history(), log.cursor()),
        )
        .cleaning(true)
        .backtracking(true)
        .with(layer);
        let result = pipeline
            .run(settings(DeliveryMode::Complete, OutputMode::Insert))
            .unwrap();
        assert_eq!(result.text, "meet Alice at 3");
        let log = log.entries();
        assert!(log.contains(&format!("completed:{}", result.text)));
        assert!(log.contains(&format!("deliver:{}", result.text)));
        let diagnostics = String::from_utf8(diagnostics.lock().unwrap().clone()).unwrap();
        assert_eq!(diagnostics.lines().count(), 1);
        assert!(diagnostics.contains("category=\"timeout\""));
        assert!(!diagnostics.contains("Alice"));
        assert!(!diagnostics.contains("untrusted"));
    }

    #[test]
    fn cleanup_runs_before_snapshot_correction_and_retains_its_changes() {
        for (heard, expected_text) in [
            ("um run aria dash tts", "run aria-tts"),
            ("um run", "run"),
            ("um open aria dash kit", "open aria-kit"),
        ] {
            let (_directory, capture) = capture();
            let log = CallLog::default();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::stream(
                    Stream::Agreement(Cadence::from_millis(100, 20_000)),
                    Answering(heard),
                ),
                unlabeled(),
                (log.history(), log.cursor()),
            )
            .cleaning(true);
            let matcher = crate::vocabulary::Vocabulary::compile(
                &[crate::vocabulary::Term {
                    canonical: "aria-kit".into(),
                    priority: 1,
                }],
                crate::vocabulary::PersonalRules::new(
                    vec![],
                    vec![("aria dash tts".into(), "aria-tts".into())],
                ),
            )
            .unwrap();
            let control = ExecutionControl::for_test(StopMode::Manual);
            let settings = settings(DeliveryMode::Streaming, OutputMode::Insert);

            let result = pipeline.execute(
                settings,
                &control,
                &DictationEventEmitter::unsubscribed(),
                Some(&matcher),
            );

            assert_eq!(result.unwrap().text, expected_text);
        }
    }

    #[test]
    fn streaming_completion_keeps_segment_corrections_across_the_final_drain() {
        for (own_voice, fail) in [(false, false), (false, true), (true, false), (true, true)] {
            let directory = tempfile::tempdir().unwrap();
            let control = ExecutionControl::for_test(StopMode::Manual);
            let events = DictationEventEmitter::unsubscribed();
            let mut settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
            settings.own_voice_only = own_voice;
            let mut published = Published::new(&settings, &control, &events, Instant::now());
            let log = CallLog::default();
            let mut sinks = (
                log.history(),
                Cursor::new(RecordingDelivery {
                    log: log.clone(),
                    fail,
                }),
            );
            sinks.dictation_started(&settings, &control).unwrap();
            let capture = control.start_recording(&events).unwrap();
            let whole = Transcript::new(if own_voice {
                "db2 private run db2"
            } else {
                "db2 run db2"
            })
            .unwrap();
            for (text, corrected, stranger) in [
                ("d b", None, false),
                ("private", None, true),
                ("two", None, false),
                ("run d b two", Some("run db2"), false),
            ] {
                if stranger && !own_voice {
                    continue;
                }
                let mut heard = heard(directory.path(), text);
                if stranger {
                    heard.verdict = Verdict::Stranger;
                }
                published
                    .segment(heard, corrected.unwrap_or(text), None, &mut sinks)
                    .unwrap();
            }
            assert_eq!(capture.should_stop(), fail);
            let result = published.finish(whole, &mut sinks);
            if fail {
                assert!(matches!(result, Err(DictationError::Delivery(_))));
            } else {
                assert_eq!(result.unwrap().0.as_str(), "d b two run db2");
            }
            let log = log.entries();
            assert_eq!(
                log.iter()
                    .filter(|entry| entry.starts_with("completed:"))
                    .collect::<Vec<_>>(),
                [&"completed:d b two run db2".to_owned()]
            );
            if fail {
                assert_eq!(
                    log.iter()
                        .filter(|entry| entry.starts_with("deliver:"))
                        .count(),
                    1
                );
            } else {
                assert_eq!(delivered(&log), "d b two run db2");
            }
        }
    }

    #[test]
    fn cleaning_drops_hesitations_and_repeats_only_when_on() {
        for (enabled, expected) in [(false, "send the the um report"), (true, "send the report")] {
            let pipeline = DictationPipeline::new(
                (),
                Transcription::complete_samples(Answering("")),
                unlabeled(),
                UnreachableSink,
            )
            .cleaning(enabled);

            let transcript = pipeline
                .finalize(
                    Transcript::new("send the the um report").unwrap(),
                    None,
                    &CancelToken::never(),
                )
                .unwrap();

            assert_eq!(transcript.as_str(), expected);
        }
    }

    #[test]
    fn a_whole_transcript_of_only_fillers_is_no_speech() {
        let pipeline = DictationPipeline::new(
            (),
            Transcription::complete_samples(Answering("")),
            unlabeled(),
            UnreachableSink,
        )
        .cleaning(true);

        let result = pipeline.finalize(Transcript::new("um").unwrap(), None, &CancelToken::never());

        assert!(matches!(result, Err(TranscriptionError::NoSpeech)));
    }

    fn ghost_events() -> (
        DictationEventEmitter,
        std::sync::mpsc::Receiver<GhostSnapshot>,
    ) {
        let (send, receive) = std::sync::mpsc::channel();
        let events = DictationEventEmitter::forwarded_by(move |event| {
            if let super::super::DictationEvent::Ghost(ghost) = event {
                let _ = send.send(ghost);
            }
        });
        (events, receive)
    }

    #[test]
    fn streaming_ghosts_preview_at_the_caret_and_clear_once_typed() {
        let (events, ghosts) = ghost_events();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
        let mut published = Published::new(&settings, &control, &events, Instant::now());
        let mut sinks = (CallLog::default().history(), Cursor::new(PlacedDelivery));
        sinks.dictation_started(&settings, &control).unwrap();
        published.provisional("turn lift", &sinks);

        let directory = tempfile::tempdir().unwrap();
        let heard = heard(directory.path(), "turn left");
        published
            .segment(heard, "turn left", None, &mut sinks)
            .unwrap();
        published.provisional("", &sinks);
        drop(published);
        drop(events);

        let ghosts: Vec<_> = ghosts.iter().collect();
        assert_eq!(ghosts.len(), 3);
        assert_eq!(ghosts[0].text.as_ref(), "turn lift");
        assert_eq!(
            ghosts[0].placement.as_ref().map(|placed| placed.caret),
            Some(PLACED)
        );
        assert!(ghosts[1..].iter().all(|ghost| ghost.text.is_empty()));
        assert!(ghosts[1..].iter().all(|ghost| ghost.placement.is_none()));
    }

    #[test]
    fn a_segment_that_fails_to_type_clears_its_ghost_for_good() {
        let (events, ghosts) = ghost_events();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
        let mut published = Published::new(&settings, &control, &events, Instant::now());
        let mut sinks = (
            CallLog::default().history(),
            Cursor::new(FocusMovesAfter {
                typed: 0,
                delivered: std::sync::atomic::AtomicUsize::new(0),
            }),
        );
        sinks.dictation_started(&settings, &control).unwrap();
        published.provisional("one", &sinks);

        let directory = tempfile::tempdir().unwrap();
        let heard = heard(directory.path(), "one");
        published.segment(heard, "one", None, &mut sinks).unwrap();
        // The recording drains after the failure, so previews keep arriving.
        published.provisional("one two", &sinks);
        drop(published);
        drop(events);

        let texts: Vec<_> = ghosts.iter().map(|ghost| ghost.text.to_string()).collect();
        assert_eq!(texts, ["one", ""]);
    }

    #[test]
    fn own_voice_streaming_shows_no_ghost_words() {
        for own_voice in [false, true] {
            let pipeline = revising(CallLog::default().history());
            let (events, ghosts) = ghost_events();
            let mut settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
            settings.own_voice_only = own_voice;
            pipeline.run_observed(settings, &events).unwrap();
            drop(events);
            let ghosts: Vec<_> = ghosts.iter().collect();
            assert_eq!(
                ghosts.iter().any(|ghost| !ghost.text.is_empty()),
                !own_voice
            );
        }
    }

    /// Types the first `typed` segments, then refuses as if focus moved.
    struct FocusMovesAfter {
        typed: usize,
        delivered: std::sync::atomic::AtomicUsize,
    }

    impl TextDelivery for FocusMovesAfter {
        type Prepared = ();
        const DEFAULT_MODE: OutputMode = OutputMode::Insert;
        const OUTLIVES_PROCESS: bool = true;

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
            let delivered = self
                .delivered
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if delivered < self.typed {
                Ok(DeliveryOutcome::Inserted)
            } else {
                Err(DeliveryError::TargetChanged)
            }
        }
    }

    #[test]
    fn a_focus_change_after_typed_text_reads_as_stopped_partway() {
        for typed in [0, 1] {
            let directory = tempfile::tempdir().unwrap();
            let control = ExecutionControl::for_test(StopMode::Manual);
            let events = DictationEventEmitter::unsubscribed();
            let settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
            let mut published = Published::new(&settings, &control, &events, Instant::now());
            let log = CallLog::default();
            let mut sinks = (
                log.history(),
                Cursor::new(FocusMovesAfter {
                    typed,
                    delivered: std::sync::atomic::AtomicUsize::new(0),
                }),
            );
            sinks.dictation_started(&settings, &control).unwrap();
            for text in ["one", "two"] {
                let heard = heard(directory.path(), text);
                published.segment(heard, text, None, &mut sinks).unwrap();
            }

            let result = published.finish(Transcript::new("one two").unwrap(), &mut sinks);

            if typed == 0 {
                assert!(matches!(result, Err(DictationError::DeliveryTargetChanged)));
            } else {
                assert!(matches!(result, Err(DictationError::DeliveryInterrupted)));
            }
        }
    }

    #[test]
    fn a_streamed_segment_of_only_fillers_is_neither_typed_nor_kept() {
        let directory = tempfile::tempdir().unwrap();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let events = DictationEventEmitter::unsubscribed();
        let settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
        let mut published = Published::new(&settings, &control, &events, Instant::now());
        let log = CallLog::default();
        let mut sinks = (log.history(), log.cursor());
        sinks.dictation_started(&settings, &control).unwrap();
        for (text, corrected) in [("um", Some("")), ("run", None)] {
            let heard = heard(directory.path(), text);
            published
                .segment(heard, corrected.unwrap_or(text), None, &mut sinks)
                .unwrap();
        }
        let whole = Transcript::new("um run").unwrap();

        let result = published.finish(whole, &mut sinks).unwrap();

        assert_eq!(result.0.as_str(), "run");
        assert_eq!(delivered(&log.entries()), "run");
    }

    #[test]
    fn own_voice_only_types_nothing_said_in_another_voice() {
        let (_directory, capture) = judged_capture();
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            hearing(OTHER),
            (log.history(), log.cursor()),
        );
        let result = pipeline.run(own_voice_only(settings(
            DeliveryMode::Streaming,
            OutputMode::Insert,
        )));

        assert!(matches!(result, Err(DictationError::NoSpeech)));
        assert_eq!(log.entries(), ["prepare:Insert"]);
    }

    /// Runs one press and returns its result and the sink log.
    fn press<C: AudioCapture, T: Transcriber, M: Mode, V: VoiceActivityDetector>(
        capture: C,
        transcription: Transcription<T, M>,
        detector: Option<V>,
        delivery: DeliveryMode,
    ) -> (Result<DictationResult, DictationError>, Vec<String>) {
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            transcription,
            unlabeled(),
            (log.history(), log.cursor()),
        )
        .hearing_speech(detector);
        let result = pipeline.run(settings(delivery, OutputMode::Insert));
        (result, log.entries())
    }

    #[test]
    fn a_press_without_speech_types_nothing() {
        let gate = crate::speaker::speech_gate;
        let agreement =
            || Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("Mm-hmm."));
        let presses = [
            press(capture().1, agreement(), gate(), DeliveryMode::Streaming),
            press(capture().1, agreement(), gate(), DeliveryMode::Complete),
            press(
                capture().1,
                Transcription::complete_samples(Answering("Mm-hmm.")),
                gate(),
                DeliveryMode::Complete,
            ),
            press(
                capture().1,
                Transcription::complete_wav(Answering("Mm-hmm.")),
                gate(),
                DeliveryMode::Complete,
            ),
        ];

        for (index, (result, log)) in presses.into_iter().enumerate() {
            assert!(
                matches!(result, Err(DictationError::NoSpeech)),
                "press {index}"
            );
            assert_eq!(log, ["prepare:Insert"], "press {index}");
        }
    }

    /// A whole-recording WAV engine keeps none of the samples it decodes, so
    /// the speech check must hear the press on its own.
    #[test]
    fn a_press_with_speech_types_its_words() {
        let agreement =
            || Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello"));
        let presses = [
            press(
                capture().1,
                agreement(),
                Some(Loud),
                DeliveryMode::Streaming,
            ),
            press(capture().1, agreement(), Some(Loud), DeliveryMode::Complete),
            press(
                capture().1,
                Transcription::complete_samples(Answering("hello")),
                Some(Loud),
                DeliveryMode::Complete,
            ),
            press(
                capture().1,
                Transcription::complete_wav(Answering("hello")),
                Some(Loud),
                DeliveryMode::Complete,
            ),
        ];

        for (index, (result, log)) in presses.into_iter().enumerate() {
            assert_eq!(
                result.expect("the press is heard").text,
                "hello",
                "press {index}"
            );
            assert_eq!(delivered(&log), "hello", "press {index}");
        }
    }

    #[test]
    fn words_guessed_before_the_speech_are_not_delivered() {
        for delivery in [DeliveryMode::Streaming, DeliveryMode::Complete] {
            // Settles on a word during the opening silence, then hears the
            // speech after it.
            let (capture, engine) = scripted(
                &[0.0; 8000],
                &[
                    (&[0.0; 1600], "Mm-hmm."),
                    (SPEECH, "Mm-hmm."),
                    (&[0.5; 1600], "Mm-hmm. hello"),
                ],
                "Mm-hmm. hello",
            );
            let (result, log) = press(
                capture,
                Transcription::agreement(Cadence::from_millis(100, 20_000), engine),
                Some(Loud),
                delivery,
            );

            assert_eq!(
                result.expect("the press is heard").text,
                "hello",
                "{delivery:?}"
            );
            assert_eq!(delivered(&log), "hello", "{delivery:?}");
        }
    }

    #[test]
    fn a_relayed_recording_without_speech_is_refused() {
        let (directory, capture) = capture();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("Mm-hmm.")),
            unlabeled(),
            (),
        )
        .hearing_speech(Some(Loud));
        let mut silent =
            RecordingReader::from_samples_in(directory.path(), &[0.0; 16_000]).unwrap();
        let mut spoken =
            RecordingReader::from_samples_in(directory.path(), &[0.5; 16_000]).unwrap();

        let refused = pipeline.transcribe(
            &mut silent,
            &Language::Automatic,
            false,
            None,
            &CancelToken::never(),
        );
        let heard = pipeline.transcribe(
            &mut spoken,
            &Language::Automatic,
            false,
            None,
            &CancelToken::never(),
        );

        assert!(matches!(refused, Err(DictationError::NoSpeech)));
        assert_eq!(heard.expect("the recording is heard").0.as_str(), "Mm-hmm.");
    }

    /// The pill reads where the text went off this event, so the outcome
    /// has to survive the whole press, not only the delivery call.
    #[test]
    fn the_delivered_stage_names_where_the_text_went() {
        let (heard, stages) = std::sync::mpsc::sync_channel(8);
        let events = DictationEventEmitter::subscribed_by("aria-test-stages", move |event| {
            if let super::super::DictationEvent::Stage(stage) = event {
                let _ = heard.send(stage);
            }
        });
        let (_directory, capture) = capture();
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("hello")),
            unlabeled(),
            (log.history(), log.cursor()),
        );
        let settings = settings(DeliveryMode::Complete, OutputMode::Insert);

        pipeline
            .run_observed(settings, &events)
            .expect("the press succeeds");
        drop(events);

        assert!(
            stages.iter().any(|stage| stage
                == DictationStage::Delivered {
                    outcome: Some(DeliveryOutcome::Inserted),
                    words: 1,
                }),
            "the delivered stage should carry its outcome"
        );
    }

    /// A client reads the terminal stage as the end. The replay here never
    /// polls its stop, so it announces no capture end; what holds is that
    /// nothing follows the failure.
    #[test]
    fn a_failed_press_ends_on_its_failure() {
        let (heard, stages) = std::sync::mpsc::sync_channel(8);
        let events = DictationEventEmitter::subscribed_by("aria-test-stages", move |event| {
            if let super::super::DictationEvent::Stage(stage) = event {
                let _ = heard.send(stage);
            }
        });
        let (_directory, capture) = judged_capture();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            hearing(OTHER),
            (CallLog::default().history(), CallLog::default().cursor()),
        );
        let settings = own_voice_only(settings(DeliveryMode::Streaming, OutputMode::Insert));

        let result = pipeline.run_observed(settings, &events);
        drop(events);

        assert!(matches!(result, Err(DictationError::NoSpeech)));
        assert_eq!(
            stages.iter().collect::<Vec<_>>(),
            [
                DictationStage::Recording(RecordingId(0)),
                DictationStage::Failed(RpcErrorCode::NoSpeech),
            ]
        );
    }

    #[test]
    fn a_relayed_recording_in_another_voice_is_refused() {
        let (directory, capture) = judged_capture();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            hearing(OTHER),
            (),
        );
        let mut recording = RecordingReader::from_samples_in(directory.path(), &[0.5; 64_000])
            .expect("the test recording is written");

        let heard = pipeline.transcribe(
            &mut recording,
            &Language::Automatic,
            true,
            None,
            &CancelToken::never(),
        );

        assert!(matches!(heard, Err(DictationError::NoSpeech)));
    }

    #[test]
    fn a_relayed_recording_in_the_enrolled_voice_is_heard() {
        let (directory, capture) = judged_capture();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            hearing(OWN),
            (),
        );
        let mut recording = RecordingReader::from_samples_in(directory.path(), &[0.5; 64_000])
            .expect("the test recording is written");

        let heard = pipeline
            .transcribe(
                &mut recording,
                &Language::Automatic,
                true,
                None,
                &CancelToken::never(),
            )
            .expect("the enrolled voice is typed");

        assert_eq!(heard.0.as_str(), "hello");
    }

    #[test]
    fn retry_requires_voice_filtering_and_honors_cancellation() {
        let (directory, capture) = capture();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("must not escape")),
            unlabeled(),
            (),
        );
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.5; 1600]).unwrap();
        assert!(matches!(
            pipeline.transcribe(
                &mut recording,
                &Language::Automatic,
                true,
                None,
                &CancelToken::never()
            ),
            Err(DictationError::NoSpeech)
        ));
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            pipeline.transcribe(&mut recording, &Language::Automatic, false, None, &cancel),
            Err(DictationError::Cancelled)
        ));
    }

    /// Where each published segment sat in its recording.
    struct PositionSink(Arc<Mutex<Vec<u64>>>);

    impl TranscriptSink for PositionSink {
        type Error = DictationError;

        fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), DictationError> {
            self.0.lock().unwrap().push(utterance.at);
            Ok(())
        }

        fn completed(&mut self, _utterance: Utterance<'_>) -> Result<(), DictationError> {
            Ok(())
        }
    }

    /// The reader is one across dictations; its place in the recording is not.
    #[test]
    fn a_new_dictation_is_heard_from_the_start_of_its_recording() {
        let (_directory, capture) = judged_capture();
        let positions = Arc::new(Mutex::new(Vec::new()));
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Echoing),
            hearing(OWN),
            (
                PositionSink(Arc::clone(&positions)),
                CallLog::default().cursor(),
            ),
        );

        for _ in 0..2 {
            pipeline
                .run(settings(DeliveryMode::Streaming, OutputMode::Insert))
                .expect("the enrolled voice is delivered");
        }
        assert_eq!(*positions.lock().unwrap(), [0, 0]);
    }

    #[test]
    fn segments_name_the_enrolled_voice_they_were_heard_in() {
        let alex = SpeakerName::new("alex").unwrap();
        for mode in [DeliveryMode::Streaming, DeliveryMode::Complete] {
            for (voice, expected) in [(OWN, Some(alex.clone())), (OTHER, None)] {
                let (_directory, capture) = judged_capture();
                let log = CallLog::default();
                let pipeline = DictationPipeline::new(
                    capture,
                    Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
                    hearing(voice),
                    (log.history(), log.cursor()),
                );
                let (send, stages) = std::sync::mpsc::channel();
                let events = DictationEventEmitter::forwarded_by(move |event| {
                    if let super::super::DictationEvent::Stage(stage) = event {
                        let _ = send.send(stage);
                    }
                });

                pipeline
                    .run_observed(settings(mode, OutputMode::Insert), &events)
                    .unwrap();

                let speakers: Vec<_> = stages
                    .try_iter()
                    .filter_map(|stage| match stage {
                        DictationStage::SegmentTranscribed { speaker, .. } => Some(speaker),
                        _ => None,
                    })
                    .collect();
                assert_eq!(speakers, [expected], "{mode:?}");
            }
        }
    }

    #[test]
    fn own_voice_only_types_what_the_enrolled_voice_said() {
        let (_directory, capture) = judged_capture();
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            hearing(OWN),
            (log.history(), log.cursor()),
        );
        let result = pipeline.run(own_voice_only(settings(
            DeliveryMode::Streaming,
            OutputMode::Insert,
        )));

        let text = result.unwrap().text;
        assert_eq!(text, "hello");
        let log = log.entries();
        assert_eq!(
            *log,
            [
                "prepare:Insert",
                "segment:hello",
                "deliver:hello",
                "completed:hello"
            ]
        );
    }

    #[test]
    fn own_voice_only_hands_a_whole_decode_only_the_enrolled_voice() {
        for (voice, expected) in [(OWN, Some("hello")), (OTHER, None)] {
            let (_directory, capture) = judged_capture();
            let log = CallLog::default();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::complete_samples(Echoing),
                hearing(voice),
                (log.history(), log.cursor()),
            );
            let result = pipeline.run(own_voice_only(settings(
                DeliveryMode::Complete,
                OutputMode::Insert,
            )));

            match expected {
                Some(text) => {
                    assert_eq!(result.unwrap().text, text);
                    assert!(log.entries().contains(&"deliver:hello".to_owned()));
                }
                None => {
                    assert!(
                        matches!(result, Err(DictationError::NoSpeech)),
                        "{result:?}"
                    );
                    assert!(
                        !log.entries()
                            .iter()
                            .any(|entry| entry.starts_with("deliver:"))
                    );
                }
            }
        }
    }

    #[test]
    fn completed_stream_is_published_even_when_final_segment_delivery_fails() {
        let (_directory, capture) = capture();
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            unlabeled(),
            (log.history(), log.failing_cursor()),
        );
        let result = pipeline.run(settings(DeliveryMode::Streaming, OutputMode::Insert));

        assert!(matches!(result, Err(DictationError::Delivery(_))));
        assert_eq!(
            log.entries(),
            [
                "prepare:Insert",
                "segment:hello",
                "deliver:hello",
                "completed:hello"
            ]
        );
    }

    #[test]
    fn an_early_failure_stops_the_recording() {
        let directory = tempfile::tempdir().expect("test directory is created");
        // Realtime and longer than the press's 5 s budget, so only a stop
        // ends capture before the bound.
        let capture = WavReplayCapture::new(
            directory.path(),
            Duration::from_secs(20),
            &fixture_of(directory.path(), 160_000),
            ReplayPace::Realtime,
        )
        .expect("fixture is replayable");
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            unlabeled(),
            CallLog::default().failing_cursor(),
        );
        let control = ExecutionControl::for_test(StopMode::Manual);
        let started = std::time::Instant::now();

        let result = pipeline.execute(
            settings(DeliveryMode::Streaming, OutputMode::Insert),
            &control,
            &DictationEventEmitter::unsubscribed(),
            None,
        );

        assert!(matches!(result, Err(DictationError::Delivery(_))));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn complete_transcript_is_published_before_delivery_can_fail() {
        let (_directory, capture) = capture();
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("hello")),
            unlabeled(),
            (log.history(), log.failing_cursor()),
        );
        let result = pipeline.run(settings(DeliveryMode::Complete, OutputMode::Insert));

        assert!(matches!(result, Err(DictationError::Delivery(_))));
        assert_eq!(
            log.entries(),
            [
                "prepare:Insert",
                "segment:hello",
                "completed:hello",
                "deliver:hello"
            ]
        );
    }
}
