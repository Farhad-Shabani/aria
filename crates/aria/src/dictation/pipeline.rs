use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use aria_kit::cancel::CancelToken;
use aria_kit::segmentation::Segmenter;
use aria_kit::speaker::Embedder;
use aria_kit::speaker::{Admission, Gate, Released, Speaker, Timeline, TurnFeed};
use aria_kit::transcription::Heard;
use aria_kit::transcription::layer::Layer;
use aria_kit::transcription::{Punctuated, SentenceEnds, check_cancelled};
use aria_kit::voice_activity::{SileroVad, SpeechDetector, VoiceActivityDetector};

use crate::audio::{AudioCapture, CaptureError, RecordingMode, RecordingReader, StopSignal};
use crate::delivery::{Caret, DeliveryOutcome, Placement};
use crate::hearing::{
    Board, Decoding, Event, Hearing, HearingError, TrackingError, hear_saved, hear_to_end,
};
use crate::primitives::error_chain;
use crate::speaker::{Diarizer, Label, Profile, SpeakerName, VoiceprintTracker};
use crate::transcription::{
    Language, Mode, Operation, Transcriber, Transcript, TranscriptSegment, Transcription,
    TranscriptionError,
};
use crate::vocabulary::Vocabulary;
use aria_segmentation::pyannote::Pyannote;
use aria_speaker::wespeaker::WeSpeaker;

use super::{
    CaptionSnapshot, DeliveryMode, DictationError, DictationEventEmitter, DictationResult,
    DictationSettings, DictationStage, ExecutionControl, GhostSnapshot, HeardSpeaker, StopMode,
    Transcribed, TranscriptSink, Utterance,
};
use super::{Meter, Rewrites};

/// Records a press, hears it, and publishes each segment to its sinks.
/// Where the text goes — history, the cursor — is the sinks' business.
/// The parameters follow a press through: capture, VAD, diarization,
/// segmentation, transcription, rewrites, sinks.
pub(crate) struct DictationPipeline<C, V, E, G, T, M: Mode, L, S> {
    capture: C,
    /// Hears where anyone spoke and whose voice it was.
    listening: Mutex<Listening<V, E, G>>,
    transcription: Transcription<T, M>,
    rewrites: Rewrites<L>,
    sinks: Mutex<S>,
}

struct Listening<V, E, G> {
    /// Finds where anyone spoke. `None` counts every recording as speech.
    vad: Option<V>,
    /// Tells whose voice a turn is. `None` refuses owner-only presses.
    diarizer: Option<Diarizer<E>>,
    /// Finds speaker changes. `None` refuses owner-only presses.
    segmenter: Option<G>,
}

impl<V: VoiceActivityDetector, E: Embedder, G: Segmenter> Listening<V, E, G> {
    /// The press's speech detector, and its owner tracker when only the
    /// owner's voice passes, both starting at `from`. The tracker is refused
    /// unless speech, an enrolled owner, and a segmenter are all there.
    #[allow(
        clippy::type_complexity,
        reason = "a detector, and a tracker that may be refused"
    )]
    fn start(
        &mut self,
        own_voice_only: bool,
        from: u64,
    ) -> (
        Option<SpeechDetector<&mut V>>,
        Result<Option<VoiceprintTracker<'_, E, &mut G>>, DictationError>,
    ) {
        let speech = self.vad.as_mut().map(|vad| SpeechDetector::new(vad, from));
        if !own_voice_only {
            return (speech, Ok(None));
        }
        let unavailable = || DictationError::Speaker(TrackingError::Unavailable);
        let hears_speech = speech.is_some();
        let (has_diarizer, has_segmenter) = (self.diarizer.is_some(), self.segmenter.is_some());
        let (true, Some(diarizer), Some(segmenter)) = (
            hears_speech,
            self.diarizer.as_mut(),
            self.segmenter.as_mut(),
        ) else {
            tracing::warn!(
                speech = hears_speech,
                diarizer = has_diarizer,
                segmenter = has_segmenter,
                "own-voice filtering lacks a model"
            );
            return (speech, Err(unavailable()));
        };
        let tracker = VoiceprintTracker::owner_only(diarizer, segmenter, from).map_err(|error| {
            tracing::warn!(error = %error_chain(&error), "own-voice filtering is unavailable");
            unavailable()
        });
        (speech, tracker.map(Some))
    }
}

struct CompleteDecode {
    segments: Vec<Heard>,
    transcript: Transcript,
    served_by: Option<&'static str>,
}

impl<C, T, M: Mode, S> DictationPipeline<C, SileroVad, WeSpeaker, Pyannote, T, M, (), S> {
    /// Hears no speech or voices and changes no text until told to.
    pub(crate) fn new(capture: C, transcription: Transcription<T, M>, sinks: S) -> Self {
        Self {
            capture,
            listening: Mutex::new(Listening {
                vad: None,
                diarizer: None,
                segmenter: None,
            }),
            transcription,
            rewrites: Rewrites::new(),
            sinks: Mutex::new(sinks),
        }
    }
}

impl<C, V, E, G, T, M: Mode, L, S> DictationPipeline<C, V, E, G, T, M, L, S> {
    /// Types no words from a recording until `vad` hears speech in it, and
    /// gives the tracker where anyone spoke. With `None`, every recording
    /// counts as speech and no voice is tracked.
    pub(crate) fn hearing_speech<W: VoiceActivityDetector>(
        self,
        vad: Option<W>,
    ) -> DictationPipeline<C, W, E, G, T, M, L, S> {
        let listening = self
            .listening
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        DictationPipeline {
            capture: self.capture,
            listening: Mutex::new(Listening {
                vad,
                diarizer: listening.diarizer,
                segmenter: listening.segmenter,
            }),
            transcription: self.transcription,
            rewrites: self.rewrites,
            sinks: self.sinks,
        }
    }

    /// Tracks voices with `diarizer` and `segmenter`, so an owner-only
    /// press keeps out the words of other voices.
    pub(crate) fn tracking_voices<F, H>(
        self,
        diarizer: Option<Diarizer<F>>,
        segmenter: Option<H>,
    ) -> DictationPipeline<C, V, F, H, T, M, L, S> {
        let listening = self
            .listening
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        DictationPipeline {
            capture: self.capture,
            listening: Mutex::new(Listening {
                vad: listening.vad,
                diarizer,
                segmenter,
            }),
            transcription: self.transcription,
            rewrites: self.rewrites,
            sinks: self.sinks,
        }
    }

    /// Runs `rewrites` on what the gate lets out.
    pub(crate) fn rewriting<N: Layer>(
        self,
        rewrites: Rewrites<N>,
    ) -> DictationPipeline<C, V, E, G, T, M, N, S> {
        DictationPipeline {
            capture: self.capture,
            listening: self.listening,
            transcription: self.transcription,
            rewrites,
            sinks: self.sinks,
        }
    }

    fn listening(&self) -> MutexGuard<'_, Listening<V, E, G>> {
        self.listening
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn sinks(&self) -> MutexGuard<'_, S> {
        self.sinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<C, V, E: Embedder, G, T, M: Mode, L, S> DictationPipeline<C, V, E, G, T, M, L, S> {
    /// Replaces the enrolled profiles the next press is judged against. A
    /// voice enrolled while the daemon ran is invisible until this is called.
    pub(crate) fn refresh_enrolled(&self, enrolled: Vec<Profile>) {
        if let Some(diarizer) = self.listening().diarizer.as_mut() {
            diarizer.set_enrolled(enrolled);
        }
    }
}

impl<C, V, E, G, T, M: Mode, L: Layer, S> DictationPipeline<C, V, E, G, T, M, L, S>
where
    V: VoiceActivityDetector,
    E: Embedder,
    G: Segmenter,
    T: Transcriber,
{
    /// Transcribes caller-provided audio with the shared engine and speaker
    /// filter, without publishing it to sinks. The result names `engine`, or
    /// the remote engine when the operation was delegated.
    pub(crate) fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        own_voice_only: bool,
        vocabulary: Option<&Vocabulary>,
        engine: &'static str,
        cancel: &CancelToken,
    ) -> Result<Transcribed, DictationError> {
        if cancel.is_cancelled() {
            return Err(DictationError::Cancelled);
        }
        let from = recording.held_from();
        let mut listening = self.listening();
        let (speech, tracker) = listening.start(own_voice_only, from);
        let (tracker, refused) = match tracker {
            Ok(tracker) => (tracker, None),
            Err(error) => (None, Some(error)),
        };
        let admission = match tracker {
            Some(_) => Admission::OwnerOnly,
            None => Admission::Everyone,
        };
        let timeline = match speech {
            Some(speech) => {
                check_cancelled(cancel)?;
                recording.fill().map_err(DictationError::Capture)?;
                let feed = TurnFeed::start(recording, speech, tracker);
                hear_saved(feed, admission, cancel)?
            }
            None => None,
        };
        // Silence is "no speech" even when the voices could not be judged.
        if let Some(error) = refused {
            return Err(error);
        }
        drop(listening);
        if cancel.is_cancelled() {
            return Err(DictationError::Cancelled);
        }
        let mut operation = self.transcription.start_with(language, cancel)?;
        let (transcript, served_by) = match timeline {
            Some(timeline) => {
                let decoded =
                    self.filtered_complete(recording, &timeline, operation, language, cancel)?;
                (decoded.transcript, decoded.served_by)
            }
            None => {
                let served_by = operation.served_by();
                (operation.finish(recording)?, served_by)
            }
        };
        Ok(Transcribed {
            transcript: self.rewrites.finish(transcript, vocabulary, cancel)?,
            engine: served_by.unwrap_or(engine),
        })
    }

    fn collect_completed(
        &self,
        mut operation: Operation<'_, T>,
        recording: &mut RecordingReader,
    ) -> Result<CompleteDecode, DictationError> {
        let served_by = operation.served_by();
        recording.fill().map_err(DictationError::Capture)?;
        let source = recording.held_from()..recording.held_to();
        let mut segments = Vec::new();
        while let Some(mut heard) = aria_kit::transcription::next_heard(&mut operation, recording)?
        {
            if [heard.segment.span(), heard.segment.source_window()]
                .into_iter()
                .flatten()
                .any(|span| {
                    span.start < source.start || span.start > span.end || span.end > source.end
                })
            {
                return Err(TranscriptionError::InvalidSegment.into());
            }
            if heard.segment.word_spans().is_none() && heard.segment.source_window().is_none() {
                heard.segment = heard.segment.with_source_window(source.clone())?;
                heard.window = source.clone();
            }
            segments.push(heard);
        }
        let transcript = operation.finish(recording)?;
        if segments.is_empty() {
            return Err(TranscriptionError::InvalidSegment.into());
        }
        Ok(CompleteDecode {
            segments,
            transcript,
            served_by,
        })
    }

    fn filtered_complete(
        &self,
        recording: &mut RecordingReader,
        timeline: &Timeline,
        operation: Operation<'_, T>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<CompleteDecode, DictationError> {
        check_cancelled(cancel)?;
        recording.fill().map_err(DictationError::Capture)?;
        let retained = recording.fork_window();
        let source = retained.held_from()..retained.held_to();
        let wholly_admitted = timeline
            .wholly_admitted(&source)
            .ok_or(DictationError::Speaker(TrackingError::Failed))?;
        let kept = timeline.admitted_audio(retained.samples(), source.start);
        if kept.is_empty() {
            return Err(DictationError::NoSpeech);
        }
        // A delegated engine runs off this computer. The privacy guide promises
        // that Complete checks the speaker before upload, so it hears only
        // turns judged the owner's: a reply too short to judge stays here.
        if operation.served_by().is_some() {
            let wholly_owner = timeline
                .wholly_owner(&source)
                .ok_or(DictationError::Speaker(TrackingError::Failed))?;
            if !wholly_owner {
                let owner = timeline.owner_audio(retained.samples(), source.start);
                let mut gated = kept_recording(recording, &owner)?;
                return self.decode_admitted(operation, &mut gated, source);
            }
        }
        let decoded = self.collect_completed(operation, recording)?;
        if decoded
            .segments
            .iter()
            .all(|heard| heard.segment.word_spans().is_some())
        {
            return gate_timed(decoded, timeline, source.start);
        }
        if wholly_admitted {
            return Ok(decoded);
        }
        check_cancelled(cancel)?;
        let mut gated = kept_recording(recording, &kept)?;
        let operation = self.transcription.start_with(language, cancel)?;
        self.decode_admitted(operation, &mut gated, source)
    }

    /// Decodes audio already cut to the owner's turns. Its timing is relative
    /// to the cut, so only the original source bounds survive.
    fn decode_admitted(
        &self,
        operation: Operation<'_, T>,
        gated: &mut RecordingReader,
        source: std::ops::Range<u64>,
    ) -> Result<CompleteDecode, DictationError> {
        let decoded = self.collect_completed(operation, gated)?;
        let segment = TranscriptSegment::new(decoded.transcript.as_str(), None)?
            .with_source_window(source.clone())?;
        Ok(CompleteDecode {
            segments: vec![Heard {
                segment,
                span: None,
                window: source,
            }],
            transcript: decoded.transcript,
            served_by: decoded.served_by,
        })
    }
}

impl<C, V, E, G, T, M, L: Layer, S> DictationPipeline<C, V, E, G, T, M, L, S>
where
    C: AudioCapture,
    V: VoiceActivityDetector + Send,
    E: Embedder + Send,
    G: Segmenter + Send,
    T: Transcriber,
    M: Mode,
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
            let Dictated {
                transcript,
                mut completion,
                own_voice,
            } = heard?;
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
                own_voice,
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
            StopMode::Manual => match settings.auto_stop {
                Some(auto_stop) => RecordingMode::UntilQuiet { auto_stop },
                None => RecordingMode::UntilStopped,
            },
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
    ) -> Result<Dictated, DictationError> {
        let started_at = Instant::now();
        let started = |error| log_transcription_error(error, control, started_at);
        let board = Board::new();
        let cancel = {
            let control = Arc::clone(control);
            board.token(move || control.should_stop())
        };
        let operation = self
            .transcription
            .start_with(&settings.recognition_language, &cancel)
            .map_err(started)?;
        let streaming = settings.delivery_mode == DeliveryMode::Streaming;
        if (!settings.own_voice_only || streaming)
            && let Some(engine) = operation.served_by()
        {
            sinks.served_by(engine);
        }
        let from = recording.held_from();
        let mut listening = self.listening();
        let (speech, tracker) = listening.start(settings.own_voice_only, from);
        let tracker = tracker?;
        let admission = if settings.own_voice_only {
            Admission::OwnerOnly
        } else {
            Admission::Everyone
        };
        let feed = speech.map(|speech| TurnFeed::start(recording, speech, tracker));
        let mut announced = Announced::default();
        let mut published = Published::new(settings, control, events, started_at);
        if admission == Admission::OwnerOnly && !streaming {
            let Some(feed) = feed else {
                return Err(DictationError::Speaker(TrackingError::Unavailable));
            };
            return self.hear_owner_complete(
                settings,
                recording,
                feed,
                operation,
                published,
                sinks,
                vocabulary,
                &cancel,
                |timeline| announced.update(timeline, events),
            );
        }
        let previews = (published.caption.is_some() || published.streaming) && M::INCREMENTAL;
        let deliver = |published: &mut Published<'_>, sinks: &mut S, released: Released| {
            let Released { heard, speaker, .. } = released;
            let mut text = heard.segment.as_str().to_owned();
            self.rewrites
                .segment(&mut text, vocabulary, &cancel)
                .map_err(|error| published.failure.take().unwrap_or_else(|| started(error)))?;
            let speaker = speaker.as_ref().and_then(Label::enrolled).cloned();
            published.segment(heard, &text, speaker, sinks)
        };
        let failed = |error: HearingError, published: &mut Published<'_>| match error {
            HearingError::Transcription(_) => published
                .failure
                .take()
                .unwrap_or_else(|| hearing_failed(error, control, started_at)),
            HearingError::Tracking(_) => hearing_failed(error, control, started_at),
        };
        let mut act = |event: Event,
                       published: &mut Published<'_>,
                       sinks: &mut S|
         -> Result<(), DictationError> {
            match event {
                Event::Timeline(timeline) => announced.update(&timeline, events),
                Event::Preview(shown) => published.provisional(&shown, sinks),
                Event::Released(released) => {
                    deliver(published, sinks, released)?;
                }
            }
            Ok(())
        };
        let ended = std::thread::scope(|scope| {
            let decoding = Decoding {
                operation,
                recording: &mut *recording,
                previews,
            };
            let mut hearing = Hearing::start(scope, decoding, feed, admission, &board, &cancel)
                .map_err(|error| failed(error, &mut published))?;
            while let Some(event) = hearing
                .next()
                .map_err(|error| failed(error, &mut published))?
            {
                act(event, &mut published, sinks)?;
            }
            let mut ended = hearing
                .finish()
                .map_err(|error| failed(error, &mut published))?;
            for event in std::mem::take(&mut ended.events) {
                act(event, &mut published, sinks)?;
            }
            Ok::<_, DictationError>(ended)
        });
        // A press that ends early still owes the last segment its mark, and
        // the field the space after it.
        let ended = match ended {
            Ok(ended) => ended,
            Err(error) => {
                published.close_last_sentence(sinks);
                return Err(error);
            }
        };
        let transcript = match settings.delivery_mode {
            DeliveryMode::Complete => self
                .rewrites
                .finish(ended.transcript, vocabulary, &cancel)
                .map_err(started)?,
            DeliveryMode::Streaming => ended.transcript,
        };
        published.finish(transcript, ended.own_voice, sinks)
    }

    /// Complete delivery with only the owner's words: the turn thread hears
    /// the press while it records; the decoder waits for the end, then the
    /// whole recording is decided at once.
    #[allow(clippy::too_many_arguments)]
    fn hear_owner_complete(
        &self,
        settings: &DictationSettings,
        recording: &mut RecordingReader,
        feed: TurnFeed<&mut V, VoiceprintTracker<'_, E, &mut G>>,
        operation: Operation<'_, T>,
        mut published: Published<'_>,
        sinks: &mut S,
        vocabulary: Option<&Vocabulary>,
        cancel: &CancelToken,
        heard: impl FnMut(&Timeline),
    ) -> Result<Dictated, DictationError> {
        let (control, started_at) = (published.control, published.started_at);
        let started = |error| log_transcription_error(error, control, started_at);
        let timeline = hear_to_end(feed, cancel, heard)
            .map_err(|error| hearing_failed(error, control, started_at))?;
        recording.fill().map_err(DictationError::Capture)?;
        let decoded = self
            .filtered_complete(
                recording,
                &timeline,
                operation,
                &settings.recognition_language,
                cancel,
            )
            .map_err(|error| match error {
                DictationError::Transcription(error) => started(error),
                error => check_execution(control).err().unwrap_or(error),
            })?;
        let transcript = self
            .rewrites
            .finish(decoded.transcript, vocabulary, cancel)
            .map_err(started)?;
        if let Some(engine) = decoded.served_by {
            sinks.served_by(engine);
        }
        for heard in decoded.segments {
            // Untimed text came from the admitted audio alone, so it
            // is the owner's whole or not at all.
            let speaker = match &heard.span {
                Some(span) => timeline.speaker(span),
                None => timeline.owner_name(),
            }
            .cloned();
            let mut text = heard.segment.as_str().to_owned();
            self.rewrites
                .segment(&mut text, vocabulary, cancel)
                .map_err(started)?;
            published.segment(heard, &text, speaker, sinks)?;
        }
        published.finish(transcript, timeline.admitted_is_owner(), sinks)
    }
}

/// What one dictation has put out so far, with the events and logs each
/// step owes.
struct Published<'a> {
    streaming: bool,
    /// Every segment's text so far, joined with the cursor's spacing. Text
    /// that failed to type still counts, so the transcript keeps every word.
    kept: String,
    /// The ghost text shown, until the next segment lands.
    ghost: Option<Arc<str>>,
    sentences: SentenceEnds,
    /// Where the last segment's audio ends: where a deferred period lands.
    heard_to: u64,
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
            kept: String::new(),
            ghost: None,
            sentences: SentenceEnds::default(),
            heard_to: 0,
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
        self.ghost = None;
        // A streamed segment of only fillers is nothing to type.
        if corrected.is_empty() {
            return Ok(());
        }
        let first = self.kept.is_empty();
        // Complete delivery types the decoder's own transcript, so joining
        // here would only make the caption disagree with it.
        let punctuated = if self.streaming {
            self.sentences
                .punctuate(corrected, heard.segment.sentence_ended_before())
        } else {
            Punctuated {
                text: corrected.to_owned(),
                defers_ending: false,
            }
        };
        self.heard_to = heard.window.end;
        self.keep(&punctuated.text);
        if self.failure.is_some() {
            return Ok(());
        }
        // History keeps no names for dictation: the ring shows them.
        let utterance = Utterance {
            text: &punctuated.text,
            speaker: None,
            at: heard.window.start,
            span: heard.span,
            defers_ending: punctuated.defers_ending,
        };
        if let Err(error) = sinks.segment(utterance) {
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
        self.announce(&punctuated.text, speaker);
        Ok(())
    }

    /// Types the period the last segment still owes, once nothing follows
    /// it. Every word has landed by then, so a failure here is logged
    /// rather than failing the press.
    fn close_last_sentence<S>(&mut self, sinks: &mut S)
    where
        S: TranscriptSink,
        S::Error: Into<DictationError>,
    {
        let Some(closing) = self.sentences.finish() else {
            return;
        };
        self.keep(&closing);
        if self.failure.is_some() {
            return;
        }
        let utterance = Utterance {
            text: &closing,
            speaker: None,
            at: self.heard_to,
            span: None,
            defers_ending: false,
        };
        if let Err(error) = sinks.segment(utterance) {
            let error: DictationError = error.into();
            tracing::warn!(
                error = %error_chain(&error),
                "closing period not typed"
            );
            return;
        }
        self.announce(&closing, None);
    }

    fn keep(&mut self, text: &str) {
        let spaced = Caret::between(&self.kept, "").insert(text);
        self.kept.push_str(&spaced);
    }

    /// Tells clients about published text: the caption and the text, and
    /// when streaming, that it was typed.
    fn announce(&mut self, text: &str, speaker: Option<SpeakerName>) {
        if let Some(caption) = &mut self.caption {
            let spaced = Caret::between(&caption.committed, "").insert(text);
            caption.committed = caption_suffix(&format!("{}{spaced}", caption.committed));
            self.events.emit(caption.clone());
        }
        self.events.emit(DictationStage::SegmentTranscribed {
            text: Arc::from(text),
            speaker,
        });
        if self.streaming {
            self.events.emit(DictationStage::SegmentDelivered);
            self.events
                .emit(GhostSnapshot::empty(self.control.recording_id()));
        }
    }

    fn finish<S>(
        mut self,
        transcript: Transcript,
        own_voice: bool,
        sinks: &mut S,
    ) -> Result<Dictated, DictationError>
    where
        S: TranscriptSink,
        S::Error: Into<DictationError>,
    {
        if !self.streaming {
            return Ok(Dictated {
                transcript,
                completion: self.completion,
                own_voice,
            });
        }
        self.close_last_sentence(sinks);
        let transcript = Transcript::new(std::mem::take(&mut self.kept))
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
        Ok(Dictated {
            transcript,
            completion: self.completion,
            own_voice,
        })
    }

    /// Hearing decides the preview again whenever it wakes; clients hear of
    /// a change once.
    fn provisional<S: TranscriptSink>(&mut self, text: &str, sinks: &S) {
        let shown = caption_suffix(text);
        if let Some(caption) = &mut self.caption
            && caption.provisional != shown
        {
            caption.provisional = Arc::clone(&shown);
            self.events.emit(caption.clone());
        }
        if self.streaming && self.failure.is_none() && self.ghost.as_ref() != Some(&shown) {
            self.ghost = Some(Arc::clone(&shown));
            self.events.emit(GhostSnapshot {
                recording: self.control.recording_id(),
                text: shown,
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

/// The voice last announced as speaking, so each change is
/// announced once.
#[derive(Default)]
struct Announced(Option<HeardSpeaker>);

impl Announced {
    /// Announces whose voice the walk last heard, if that changed.
    fn update(&mut self, timeline: &Timeline, events: &DictationEventEmitter) {
        let speaking = timeline.speaking();
        if self.0.is_none() && speaking.is_none() {
            return;
        }
        let voice = match speaking {
            None | Some(Speaker::Pending(_)) => HeardSpeaker::Undecided,
            Some(Speaker::Other(_)) => HeardSpeaker::KeptOut,
            Some(Speaker::Unverified) => HeardSpeaker::Typed(None),
            Some(Speaker::Owner(name)) => HeardSpeaker::Typed(name),
        };
        if self.0.as_ref() != Some(&voice) {
            events.emit(DictationStage::SpeakerChanged(voice.clone()));
            self.0 = Some(voice);
        }
    }
}

/// A tracking failure on a press already cancelled or out of time ends it
/// as cancelled or timed out.
fn hearing_failed(
    error: HearingError,
    control: &ExecutionControl,
    started_at: Instant,
) -> DictationError {
    match error {
        HearingError::Transcription(error) => log_transcription_error(error, control, started_at),
        HearingError::Tracking(error) => check_execution(control)
            .err()
            .unwrap_or_else(|| error.into()),
    }
}

/// Keeps the owner's words of a decode timed word by word, read from
/// stream position `from`.
fn gate_timed(
    decoded: CompleteDecode,
    timeline: &Timeline,
    from: u64,
) -> Result<CompleteDecode, DictationError> {
    let mut gate = Gate::new(Admission::OwnerOnly, from);
    for heard in decoded.segments {
        gate.hold(heard);
    }
    let segments: Vec<Heard> = gate
        .decide(timeline, None)
        .released
        .into_iter()
        .map(|released| released.heard)
        .collect();
    let text = segments
        .iter()
        .map(|heard| heard.segment.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(CompleteDecode {
        segments,
        transcript: Transcript::new(text)?,
        served_by: decoded.served_by,
    })
}

/// `kept` as a recording of its own, saved beside the original when that one
/// is a file, so a decoder that reads files can take it. `NoSpeech` when it
/// holds nothing.
fn kept_recording(
    recording: &mut RecordingReader,
    kept: &[f32],
) -> Result<RecordingReader, DictationError> {
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
        Some(directory) => {
            RecordingReader::from_samples_in(&directory, kept).map_err(DictationError::Capture)
        }
        None => Ok(RecordingReader::from_samples(kept)),
    }
}

/// A press's text once hearing has ended.
struct Dictated {
    transcript: Transcript,
    /// Whether the sinks still owe the completed transcript.
    completion: Completion,
    /// Every word typed was the owner's, judged on the whole recording.
    own_voice: bool,
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
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use aria_kit::cancel::CancelToken;
    use aria_kit::speaker::{Diarizer, SpeakerError};

    use crate::audio::{
        Recording, RecordingWriter, ReplayPace, WavReplayCapture, canonical_fixture, fixture_of,
    };
    use crate::delivery::{DeliveryError, OutputMode, TextDelivery};
    use crate::dictation::testing::{
        CallLog, Hypotheses, Loud, Operations, PLACED, PlacedDelivery, RecordingDelivery, settings,
        timed,
    };
    use crate::dictation::{Cursor, RecordingId};
    use crate::rpc::RpcErrorCode;
    use crate::speaker::testing::{OWN, Voice, diarizer, tracker};
    use crate::transcription::{
        Cadence, ChunkStream, Emitted, Hypothesis, Language, Stream, Streaming, Transcription,
        TranscriptionAudio,
    };
    use aria_kit::speaker::LevelSegmenter;

    use super::*;
    use aria_kit::speaker::Embedder;

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

    /// Enough audio for the tracker to judge a voice: 4 s.
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

    const OTHER: [f32; 2] = [0.0, 1.0];

    /// A pipeline whose speech gate hears any sound, tracking voices with
    /// `diarizer` and a level segmenter.
    fn filtered_pipeline<C, E, T, M: Mode, S>(
        capture: C,
        diarizer: Diarizer<E>,
        transcription: Transcription<T, M>,
        sinks: S,
    ) -> DictationPipeline<C, Loud, E, LevelSegmenter, T, M, (), S> {
        DictationPipeline::new(capture, transcription, sinks)
            .hearing_speech(Some(Loud))
            .tracking_voices(Some(diarizer), Some(LevelSegmenter))
    }

    /// A case name, and models an owner-only press cannot run with.
    type UnavailableFilter = (
        &'static str,
        Option<Loud>,
        Option<Diarizer<Voice>>,
        Option<LevelSegmenter>,
    );

    fn unavailable_filters() -> [UnavailableFilter; 4] {
        let guest =
            aria_kit::speaker::enrolled_voice("sam", crate::speaker::Role::Guest, OWN.to_vec());
        [
            (
                "no speech detector",
                None,
                Some(diarizer(Voice(OWN))),
                Some(LevelSegmenter),
            ),
            ("no diarizer", Some(Loud), None, Some(LevelSegmenter)),
            (
                "guest profile only",
                Some(Loud),
                Some(Diarizer::new(Voice(OWN), vec![guest])),
                Some(LevelSegmenter),
            ),
            (
                "owner without segmentation",
                Some(Loud),
                Some(diarizer(Voice(OWN))),
                None,
            ),
        ]
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

    fn stage_events() -> (
        DictationEventEmitter,
        std::sync::mpsc::Receiver<DictationStage>,
    ) {
        let (send, receive) = std::sync::mpsc::channel();
        let events = DictationEventEmitter::forwarded_by(move |event| {
            if let super::super::DictationEvent::Stage(stage) = event {
                let _ = send.send(stage);
            }
        });
        (events, receive)
    }

    impl<C, V, E, G, T, M, L, S> DictationPipeline<C, V, E, G, T, M, L, S>
    where
        C: AudioCapture,
        V: VoiceActivityDetector + Send,
        E: Embedder + Send,
        G: Segmenter + Send,
        T: Transcriber,
        M: Mode,
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

    /// What is heard in a short loud recording when the engine answers
    /// `text`.
    fn heard(directory: &std::path::Path, text: &'static str) -> Heard {
        let transcription = Transcription::complete_samples(Answering(text));
        let mut operation = transcription.start(&Language::Automatic).unwrap();
        let mut recording = RecordingReader::from_samples_in(directory, &[0.5; 1600]).unwrap();
        aria_kit::transcription::next_heard(&mut operation, &mut recording)
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
        last: Hypothesis,
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
                return Ok(Hypothesis::with_words(
                    self.last.as_str(),
                    self.last.words().to_vec(),
                ));
            };
            writer.as_mut().unwrap().push(audio);
            Ok(Hypothesis::new(text))
        }
    }

    fn scripted(
        opening: &'static [f32],
        steps: &'static [(&'static [f32], &'static str)],
        last: Hypothesis,
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
        SileroVad,
        aria_speaker::wespeaker::WeSpeaker,
        Pyannote,
        Scripted,
        Streaming,
        (),
        S,
    > {
        let (capture, engine) = scripted(
            SPEECH,
            &[(SPEECH, "turn lift")],
            Hypothesis::new("turn left"),
        );
        DictationPipeline::new(
            capture,
            Transcription::agreement(Cadence::from_millis(500, 20_000), engine),
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
        let heard = aria_kit::transcription::next_heard(&mut operation, &mut recording)
            .unwrap()
            .unwrap();
        published.provisional("uncommitted guess", &sinks);
        published
            .segment(heard, "turn left", None, &mut sinks)
            .unwrap();
        assert_eq!(log.entries(), ["prepare:Insert", "segment:turn left"]);
        let transcript = operation.finish(&mut recording).unwrap();
        let Dictated {
            transcript,
            mut completion,
            ..
        } = published.finish(transcript, false, &mut sinks).unwrap();
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
            let pipeline = filtered_pipeline(
                capture,
                diarizer(Voice(voice)),
                Transcription::agreement(Cadence::from_millis(500, 20_000), Answering("hello")),
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
            (UnreachableSink, log.cursor()),
        );
        let result = pipeline.run(settings(DeliveryMode::Streaming, OutputMode::Insert));

        assert!(matches!(
            result,
            Err(DictationError::StreamingDeliveryUnavailable)
        ));
        assert!(log.entries().is_empty());
    }

    /// Records the mode each press asks for, then hands back a finished
    /// recording of speech.
    #[derive(Default)]
    struct ModeSpy(Mutex<Vec<RecordingMode>>);

    impl AudioCapture for ModeSpy {
        fn start(
            &self,
            mode: RecordingMode,
            stop: StopSignal,
        ) -> Result<RecordingReader, CaptureError> {
            self.0.lock().unwrap().push(mode);
            Finished(SPEECH.to_vec()).start(mode, stop)
        }
    }

    #[test]
    fn a_manual_press_records_until_quiet_only_with_auto_stop_on() {
        let ten_seconds = crate::audio::AutoStop::new(Duration::from_secs(10)).unwrap();
        for (auto_stop, expected) in [
            (
                Some(ten_seconds),
                RecordingMode::UntilQuiet {
                    auto_stop: ten_seconds,
                },
            ),
            (None, RecordingMode::UntilStopped),
        ] {
            let capture = Arc::new(ModeSpy::default());
            let log = CallLog::default();
            let pipeline = DictationPipeline::new(
                Arc::clone(&capture),
                Transcription::complete_samples(Answering("hello")),
                (log.history(), log.cursor()),
            );
            let mut settings = settings(DeliveryMode::Complete, OutputMode::Insert);
            settings.auto_stop = auto_stop;

            pipeline.run(settings).expect("the press completes");

            assert_eq!(*capture.0.lock().unwrap(), [expected]);
        }
    }

    #[test]
    fn a_transcript_only_press_hands_the_text_back_and_types_nothing() {
        let (_directory, capture) = capture();
        let typed = CallLog::default();
        let history = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("hello")),
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
                        (log.history(), log.cursor()),
                    )
                    .rewriting(Rewrites::new().backtracking(enabled).with(()));
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
                Hypothesis::new("meet at 2 actually 3"),
            );
            let log = CallLog::default();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::agreement(Cadence::from_millis(100, 20_000), engine),
                (log.history(), log.cursor()),
            )
            .rewriting(Rewrites::new().backtracking(true));
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
            UnreachableSink,
        )
        .rewriting(Rewrites::new().backtracking(true));
        for text in ["meet at 2", "actually 3", "scratch", "that to Sara"] {
            assert_eq!(
                pipeline
                    .rewrites
                    .finish(Transcript::new(text).unwrap(), None, &CancelToken::never())
                    .unwrap()
                    .as_str(),
                text
            );
        }
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            pipeline.rewrites.finish(
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
                    (log.history(), log.cursor()),
                )
                .rewriting(
                    Rewrites::new()
                        .removing_disfluencies(true)
                        .with(enabled.then_some(layer))
                        .with(enabled.then_some(punctuation)),
                );
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
            UnreachableSink,
        )
        .rewriting(
            Rewrites::new()
                .removing_disfluencies(true)
                .backtracking(true)
                .with(layer),
        );
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.5; 1600]).unwrap();
        let cancel = CancelToken::new();
        assert_eq!(
            pipeline
                .transcribe(
                    &mut recording,
                    &Language::Automatic,
                    false,
                    None,
                    "test",
                    &cancel,
                )
                .unwrap()
                .transcript
                .as_str(),
            "Meet at 3."
        );
        cancel.cancel();
        assert!(matches!(
            pipeline.transcribe(
                &mut recording,
                &Language::Automatic,
                false,
                None,
                "test",
                &cancel,
            ),
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
                (log.history(), log.cursor()),
            )
            .rewriting(Rewrites::new().backtracking(true).with(layer));
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
            UnreachableSink,
        )
        .rewriting(Rewrites::new().backtracking(true).with(Some(cleanup)));
        for bytes in [4097, 65537] {
            let prefix = "a".repeat(bytes);
            let text = format!("{prefix} meet at 2 actually 3");
            let transcript = pipeline
                .rewrites
                .finish(Transcript::new(&text).unwrap(), None, &CancelToken::never())
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
            (log.history(), log.cursor()),
        )
        .rewriting(
            Rewrites::new()
                .removing_disfluencies(true)
                .backtracking(true)
                .with(layer),
        );
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
                (log.history(), log.cursor()),
            )
            .rewriting(Rewrites::new().removing_disfluencies(true));
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
        for fail in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let control = ExecutionControl::for_test(StopMode::Manual);
            let events = DictationEventEmitter::unsubscribed();
            let settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
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
            let whole = Transcript::new("db2 run db2").unwrap();
            for (text, corrected) in [
                ("d b", None),
                ("two", None),
                ("run d b two", Some("run db2")),
            ] {
                let heard = heard(directory.path(), text);
                published
                    .segment(heard, corrected.unwrap_or(text), None, &mut sinks)
                    .unwrap();
            }
            assert_eq!(capture.should_stop(), fail);
            let result = published.finish(whole, false, &mut sinks);
            if fail {
                assert!(matches!(result, Err(DictationError::Delivery(_))));
            } else {
                assert_eq!(result.unwrap().transcript.as_str(), "d b two run db2");
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
                UnreachableSink,
            )
            .rewriting(Rewrites::new().removing_disfluencies(enabled));

            let transcript = pipeline
                .rewrites
                .finish(
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
            UnreachableSink,
        )
        .rewriting(Rewrites::new().removing_disfluencies(true));

        let result =
            pipeline
                .rewrites
                .finish(Transcript::new("um").unwrap(), None, &CancelToken::never());

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
    fn own_voice_shows_ghost_words_once_the_owner_is_heard() {
        for (voice, shown) in [(OWN, true), (OTHER, false)] {
            let (capture, engine) = scripted(
                SPEECH,
                &[(SPEECH, "turn lift")],
                Hypothesis::new("turn left"),
            );
            let pipeline = filtered_pipeline(
                capture,
                diarizer(Voice(voice)),
                Transcription::agreement(Cadence::from_millis(500, 20_000), engine),
                CallLog::default().history(),
            );
            let (events, ghosts) = ghost_events();
            let settings = own_voice_only(settings(DeliveryMode::Streaming, OutputMode::Insert));

            let _ = pipeline.run_observed(settings, &events);
            drop(events);

            let ghosts: Vec<_> = ghosts.iter().collect();
            assert_eq!(
                ghosts.iter().any(|ghost| !ghost.text.is_empty()),
                shown,
                "{voice:?}"
            );
        }
    }

    const OTHER_LEVEL: f32 = 0.2;
    const OTHER_SPEECH: &[f32] = &[OTHER_LEVEL; 32_000];
    const OWN_SPEECH: &[f32] = &[0.5; 48_000];
    const PAUSE: &[f32] = &[0.0; 16_000];

    /// Hears the owner in loud samples and another voice in softer ones, so
    /// a window that mixes both scores in between, as a real one does.
    struct VoiceByLevel;

    impl Embedder for VoiceByLevel {
        fn embed(&mut self, window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
            let own = window.iter().filter(|sample| **sample >= 0.4).count();
            let other = window
                .iter()
                .filter(|sample| (0.1..0.4).contains(*sample))
                .count();
            if own + other < 8_000 {
                return Ok(None);
            }
            Ok(crate::speaker::normalized(vec![own as f32, other as f32]))
        }

        fn dimension(&self) -> usize {
            2
        }
    }

    /// A press whose decoder hears nothing while `steps` play, then
    /// finalizes `last` all at once: a decoder that fell behind.
    fn late_press(
        opening: &'static [f32],
        steps: &'static [(&'static [f32], &'static str)],
        last: Hypothesis,
    ) -> (
        Result<DictationResult, DictationError>,
        Vec<String>,
        Vec<String>,
    ) {
        let (capture, engine) = scripted(opening, steps, last);
        let log = CallLog::default();
        let pipeline = filtered_pipeline(
            capture,
            diarizer(VoiceByLevel),
            Transcription::agreement(Cadence::from_millis(100, 20_000), engine),
            (log.history(), log.cursor()),
        );
        let (events, ghosts) = ghost_events();
        let settings = own_voice_only(settings(DeliveryMode::Streaming, OutputMode::Insert));
        let result = pipeline.run_observed(settings, &events);
        drop(events);
        let ghosts = ghosts.iter().map(|ghost| ghost.text.to_string()).collect();
        (result, log.entries(), ghosts)
    }

    #[test]
    fn a_late_segment_types_only_the_owners_words_after_a_pause() {
        let (result, log, ghosts) = late_press(
            OTHER_SPEECH,
            &[(PAUSE, ""), (OWN_SPEECH, "")],
            timed(
                "hey there hello world",
                &[(0.2, 0.8), (1.0, 1.8), (3.2, 4.0), (4.5, 5.5)],
            ),
        );

        assert_eq!(result.unwrap().text, "hello world");
        assert_eq!(delivered(&log), "hello world");
        assert!(ghosts.iter().all(|ghost| !ghost.contains("hey")));
    }

    #[test]
    fn a_late_untimed_segment_over_two_voices_is_kept_out() {
        let (result, log, _) = late_press(
            OTHER_SPEECH,
            &[(PAUSE, ""), (OWN_SPEECH, "")],
            Hypothesis::new("hey hello"),
        );

        assert!(matches!(result, Err(DictationError::NoSpeech)));
        assert_eq!(delivered(&log), "");
    }

    /// A recording of `samples`, finished before the press reads it.
    struct Finished(Vec<f32>);

    impl AudioCapture for Finished {
        fn start(
            &self,
            _mode: RecordingMode,
            _stop: StopSignal,
        ) -> Result<RecordingReader, CaptureError> {
            let (recording, mut writer) = Recording::open();
            writer.push(&self.0);
            writer.finish();
            Ok(recording)
        }
    }

    /// What a decoder hears in `samples`: "own" in the owner's loud
    /// samples and "other" in softer ones. `inputs` records whether each
    /// decode held the other voice.
    fn hear_levels(samples: &[f32], inputs: &Mutex<Vec<bool>>) -> Hypothesis {
        let other = samples.iter().any(|sample| (0.1..0.4).contains(sample));
        inputs.lock().unwrap().push(other);
        let own = samples.iter().any(|sample| *sample >= 0.4);
        let heard: Vec<&str> = [(other, "other"), (own, "own")]
            .into_iter()
            .filter_map(|(heard, word)| heard.then_some(word))
            .collect();
        Hypothesis::new(heard.join(" "))
    }

    /// Hears levels in samples.
    fn levels(inputs: Arc<Mutex<Vec<bool>>>) -> impl Transcriber {
        Hypotheses(move |audio| Ok(hear_levels(audio.require_samples("levels")?, &inputs)))
    }

    /// Hears levels in a saved WAV.
    fn wav_levels(inputs: Arc<Mutex<Vec<bool>>>) -> impl Transcriber {
        Hypotheses(move |audio| {
            let path = audio
                .require_wav("test")?
                .path()
                .expect("the input is a saved WAV");
            let clip = aria_kit::audio::read_wav(path).unwrap();
            Ok(hear_levels(clip.samples(), &inputs))
        })
    }

    #[test]
    fn complete_context_preserves_timed_owner_words_before_filtering() {
        let source = [OTHER_SPEECH, PAUSE, OWN_SPEECH].concat();
        let full_inputs = Arc::new(Mutex::new(Vec::new()));
        let log = CallLog::default();
        // Times words differently once it hears the other voice too.
        let context_sensitive = Hypotheses({
            let full_inputs = Arc::clone(&full_inputs);
            move |audio| {
                let samples = audio.require_samples("context-sensitive test")?;
                let full = samples.iter().any(|sample| (0.1..0.4).contains(sample));
                full_inputs.lock().unwrap().push(full);
                Ok(if full {
                    timed("other first second", &[(0.2, 1.2), (3.2, 3.8), (4.2, 5.0)])
                } else {
                    timed("second", &[(0.1, 0.5)])
                })
            }
        });
        let pipeline = filtered_pipeline(
            Finished(source),
            diarizer(VoiceByLevel),
            Transcription::complete_samples(context_sensitive),
            (log.history(), log.cursor()),
        );

        let result = pipeline
            .run(own_voice_only(settings(
                DeliveryMode::Complete,
                OutputMode::Insert,
            )))
            .unwrap();

        assert_eq!(result.text, "first second");
        assert_eq!(delivered(&log.entries()), "first second");
        assert_eq!(*full_inputs.lock().unwrap(), [true]);
        assert!(log.entries().iter().all(|entry| !entry.contains("other")));
    }

    #[test]
    fn complete_context_segments_are_cleaned_before_history_sees_them() {
        let log = CallLog::default();
        let pipeline = filtered_pipeline(
            Finished([OTHER_SPEECH, PAUSE, OWN_SPEECH].concat()),
            diarizer(VoiceByLevel),
            // A filler inside the owner's turn.
            Transcription::complete_samples(Hypotheses(|_| {
                Ok(timed(
                    "other first um second",
                    &[(0.2, 1.2), (3.2, 3.6), (3.7, 3.9), (4.2, 5.0)],
                ))
            })),
            (log.history(), log.cursor()),
        )
        .rewriting(Rewrites::new().removing_disfluencies(true));

        let result = pipeline
            .run(own_voice_only(settings(
                DeliveryMode::Complete,
                OutputMode::Insert,
            )))
            .unwrap();

        assert_eq!(result.text, "first second");
        assert!(log.entries().contains(&"segment:first second".to_owned()));
        assert!(log.entries().iter().all(|entry| !entry.contains("um")));
    }

    #[test]
    fn complete_context_does_not_decode_wholly_admitted_audio_twice() {
        let source = OWN_SPEECH.to_vec();
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let pipeline = filtered_pipeline(
            Finished(source.clone()),
            diarizer(VoiceByLevel),
            Transcription::complete_samples(levels(Arc::clone(&inputs))),
            CallLog::default().history(),
        );
        assert_eq!(
            pipeline
                .run(own_voice_only(settings(
                    DeliveryMode::Complete,
                    OutputMode::Insert
                )))
                .unwrap()
                .text,
            "own"
        );
        assert_eq!(
            pipeline
                .transcribe(
                    &mut RecordingReader::from_samples(&source),
                    &Language::Automatic,
                    true,
                    None,
                    "test",
                    &CancelToken::never()
                )
                .unwrap()
                .transcript
                .as_str(),
            "own"
        );
        assert_eq!(*inputs.lock().unwrap(), [false, false]);
    }

    fn source_for_segments(
        engine: &'static str,
        segments: Vec<TranscriptSegment>,
        text: &'static str,
        inputs: Arc<Mutex<Vec<Vec<f32>>>>,
    ) -> aria_kit::transcription::OperationSource {
        use aria_kit::transcription::{OperationSource, OperationStep, TranscriptUpdate};

        let mut segments = segments.into_iter();
        let mut first = true;
        OperationSource::new(engine, move |recording, _| {
            recording.fill().map_err(TranscriptionError::Capture)?;
            if first {
                inputs.lock().unwrap().push(recording.samples().to_vec());
                first = false;
            }
            let end = recording.held_to();
            Ok(match segments.next() {
                Some(segment) => OperationStep::Update {
                    update: TranscriptUpdate::Finalized(segment),
                    retained_from: end,
                    read_to: end,
                    completed: None,
                },
                None => OperationStep::Complete {
                    transcript: Transcript::new(text).unwrap(),
                    retained_from: end,
                    read_to: end,
                },
            })
        })
    }

    fn timed_segment(text: &str, span: std::ops::Range<u64>) -> TranscriptSegment {
        TranscriptSegment::new(text, Some(span.clone()))
            .unwrap()
            .with_word_spans(vec![span])
            .unwrap()
    }

    #[test]
    fn a_delegated_engine_hears_only_the_owners_turns() {
        for live in [false, true] {
            let inputs = Arc::new(Mutex::new(Vec::new()));
            let model = Operations({
                let inputs = Arc::clone(&inputs);
                move |_: &CancelToken| {
                    source_for_segments(
                        "remote",
                        vec![
                            timed_segment("first", 1_600..8_000),
                            TranscriptSegment::new("second", None).unwrap(),
                        ],
                        "first second",
                        Arc::clone(&inputs),
                    )
                }
            });
            let source = [OTHER_SPEECH, PAUSE, OWN_SPEECH].concat();
            let log = CallLog::default();
            let pipeline = filtered_pipeline(
                Finished(source.clone()),
                diarizer(VoiceByLevel),
                Transcription::<_, crate::transcription::Whole>::delegated(model),
                (log.history(), log.cursor()),
            );

            let text = if live {
                pipeline
                    .run(own_voice_only(settings(
                        DeliveryMode::Complete,
                        OutputMode::Insert,
                    )))
                    .unwrap()
                    .text
            } else {
                pipeline
                    .transcribe(
                        &mut RecordingReader::from_samples(&source),
                        &Language::Automatic,
                        true,
                        None,
                        "test",
                        &CancelToken::never(),
                    )
                    .unwrap()
                    .transcript
                    .into_text()
            };

            assert_eq!(text, "first second");
            let uploaded = inputs.lock().unwrap();
            assert_eq!(uploaded.len(), 1);
            assert!(uploaded[0].iter().all(|sample| *sample != OTHER_LEVEL));
            assert_eq!(
                uploaded[0].iter().filter(|sample| **sample >= 0.4).count(),
                OWN_SPEECH.len()
            );
        }
    }

    #[test]
    fn complete_context_reuses_mixed_timing_when_the_whole_source_is_admitted() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let openings = Arc::new(AtomicUsize::new(0));
        let model = Operations({
            let openings = Arc::clone(&openings);
            move |_: &CancelToken| {
                assert_eq!(openings.fetch_add(1, Ordering::Relaxed), 0);
                source_for_segments(
                    "only-engine",
                    vec![
                        timed_segment("first", 1_600..8_000),
                        TranscriptSegment::new("second", None).unwrap(),
                    ],
                    "first second",
                    Arc::default(),
                )
            }
        });
        let log = CallLog::default();
        let pipeline = filtered_pipeline(
            Finished([OWN_SPEECH, PAUSE, OWN_SPEECH].concat()),
            diarizer(VoiceByLevel),
            Transcription::<_, crate::transcription::Whole>::delegated(model),
            (log.history(), log.cursor()),
        );

        let result = pipeline
            .run(own_voice_only(settings(
                DeliveryMode::Complete,
                OutputMode::Insert,
            )))
            .unwrap();

        assert_eq!(result.text, "first second");
        assert_eq!(openings.load(Ordering::Relaxed), 1);
        assert_eq!(
            log.entries(),
            [
                "prepare:Insert",
                "engine:only-engine",
                "segment:first",
                "segment:second",
                "completed:first second",
                "deliver:first second"
            ]
        );
    }

    #[test]
    fn complete_context_refuses_unresolved_admission_before_decoding() {
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let pipeline = DictationPipeline::new(
            (),
            Transcription::complete_samples(levels(Arc::clone(&inputs))),
            (),
        )
        .hearing_speech(None::<Loud>);
        let mut recording = RecordingReader::from_samples(&OWN_SPEECH[..4_000]);
        recording.fill().unwrap();
        let mut diarizer = diarizer(VoiceByLevel);
        let mut feed = TurnFeed::start(
            &mut recording,
            SpeechDetector::new(Loud, 0),
            Some(tracker(&mut diarizer, 0)),
        );
        feed.hear_through(4_000);
        let timeline = feed.timeline().expect("a tracker runs").clone();
        let language = Language::Automatic;
        let operation = pipeline.transcription.start(&language).unwrap();

        let result = pipeline.filtered_complete(
            &mut recording,
            &timeline,
            operation,
            &language,
            &CancelToken::never(),
        );

        assert!(matches!(
            result,
            Err(DictationError::Speaker(TrackingError::Failed))
        ));
        assert!(inputs.lock().unwrap().is_empty());
    }

    #[test]
    fn complete_context_rejects_explicit_bounds_outside_the_decoded_source() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for source_bounds in [false, true] {
            let openings = Arc::new(AtomicUsize::new(0));
            let model = Operations({
                let openings = Arc::clone(&openings);
                move |_: &CancelToken| {
                    assert_eq!(openings.fetch_add(1, Ordering::Relaxed), 0);
                    let bad = if source_bounds {
                        TranscriptSegment::new("bad", None)
                            .unwrap()
                            .with_source_window(0..96_001)
                            .unwrap()
                    } else {
                        timed_segment("bad", 96_000..96_001)
                    };
                    source_for_segments(
                        "discarded-engine",
                        vec![TranscriptSegment::new("discarded", None).unwrap(), bad],
                        "discarded bad",
                        Arc::default(),
                    )
                }
            });
            let log = CallLog::default();
            let pipeline = filtered_pipeline(
                Finished([OTHER_SPEECH, PAUSE, OWN_SPEECH].concat()),
                diarizer(VoiceByLevel),
                Transcription::<_, crate::transcription::Whole>::delegated(model),
                (log.history(), log.cursor()),
            );

            let result = pipeline.run(own_voice_only(settings(
                DeliveryMode::Complete,
                OutputMode::Insert,
            )));

            assert!(matches!(
                result,
                Err(DictationError::Transcription(
                    TranscriptionError::InvalidSegment
                ))
            ));
            assert_eq!(openings.load(Ordering::Relaxed), 1);
            assert_eq!(log.entries(), ["prepare:Insert"]);
        }
    }

    /// Decodes a saved `source` held from stream position `origin`,
    /// keeping only the owner's words.
    fn decode_from<T: Transcriber, M: Mode>(
        transcription: Transcription<T, M>,
        source: &[f32],
        origin: u64,
    ) -> CompleteDecode {
        let pipeline = DictationPipeline::new((), transcription, ()).hearing_speech(None::<Loud>);
        let mut recording = RecordingReader::from_samples(source);
        recording.fill().unwrap();
        recording.release_before(origin);
        let mut diarizer = diarizer(VoiceByLevel);
        let feed = TurnFeed::start(
            &mut recording,
            SpeechDetector::new(Loud, origin),
            Some(tracker(&mut diarizer, origin)),
        );
        let timeline = hear_saved(feed, Admission::OwnerOnly, &CancelToken::never())
            .unwrap()
            .unwrap();
        let language = Language::Automatic;
        let operation = pipeline.transcription.start(&language).unwrap();
        pipeline
            .filtered_complete(
                &mut recording,
                &timeline,
                operation,
                &language,
                &CancelToken::never(),
            )
            .unwrap()
    }

    #[test]
    fn complete_context_bounds_an_untimed_delegate_at_a_nonzero_origin() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let openings = Arc::new(AtomicUsize::new(0));
        let model = Operations({
            let openings = Arc::clone(&openings);
            move |_: &CancelToken| {
                assert_eq!(openings.fetch_add(1, Ordering::Relaxed), 0);
                source_for_segments(
                    "only-engine",
                    vec![TranscriptSegment::new("own", None).unwrap()],
                    "own",
                    Arc::default(),
                )
            }
        });

        let decoded = decode_from(
            Transcription::<_, crate::transcription::Whole>::delegated(model),
            OWN_SPEECH,
            8_000,
        );

        assert_eq!(openings.load(Ordering::Relaxed), 1);
        assert_eq!(decoded.transcript.as_str(), "own");
        assert_eq!(decoded.segments[0].window, 8_000..48_000);
        assert_eq!(
            decoded.segments[0].segment.source_window(),
            Some(8_000..48_000)
        );
        assert_eq!(decoded.segments[0].span, None);
    }

    #[test]
    fn complete_context_fallback_drops_clipped_coordinates_and_identity_metadata() {
        let source = [OTHER_SPEECH, PAUSE, OWN_SPEECH].concat();

        let decoded = decode_from(
            Transcription::complete_samples(levels(Arc::default())),
            &source,
            8_000,
        );

        assert_eq!(decoded.transcript.as_str(), "own");
        assert_eq!(decoded.segments.len(), 1);
        let heard = &decoded.segments[0];
        assert_eq!(heard.window, 8_000..source.len() as u64);
        assert_eq!(heard.span, None);
        assert_eq!(heard.segment.span(), None);
        assert_eq!(heard.segment.word_spans(), None);
        assert_eq!(
            heard.segment.source_window(),
            Some(8_000..source.len() as u64)
        );
    }

    #[test]
    fn complete_context_late_failures_map_to_errors_and_publish_nothing() {
        use aria_kit::transcription::{OperationSource, OperationStep, TranscriptUpdate};

        for failure in ["inference", "cancel", "no-speech", "no-segments"] {
            for live in [false, true] {
                if failure == "cancel" && live {
                    continue;
                }
                let model = Operations({
                    move |cancel: &CancelToken| {
                        let cancel = cancel.clone();
                        let mut first = true;
                        OperationSource::new("discarded-engine", move |recording, _| {
                            recording.fill().map_err(TranscriptionError::Capture)?;
                            let end = recording.held_to();
                            if failure == "no-speech" {
                                return Err(TranscriptionError::NoSpeech);
                            }
                            if failure == "no-segments" {
                                return Ok(OperationStep::Complete {
                                    transcript: Transcript::new("unlocated").unwrap(),
                                    retained_from: end,
                                    read_to: end,
                                });
                            }
                            if std::mem::take(&mut first) {
                                return Ok(OperationStep::Update {
                                    update: TranscriptUpdate::Finalized(
                                        TranscriptSegment::new("discarded", None).unwrap(),
                                    ),
                                    retained_from: end,
                                    read_to: end,
                                    completed: Some(Transcript::new("discarded").unwrap()),
                                });
                            }
                            match failure {
                                "inference" => Err(TranscriptionError::Inference {
                                    backend: "test",
                                    source: aria_kit::backend::BackendError::Unusable(
                                        "test failure".into(),
                                    ),
                                }),
                                "cancel" => {
                                    cancel.cancel();
                                    Ok(OperationStep::Complete {
                                        transcript: Transcript::new("discarded").unwrap(),
                                        retained_from: end,
                                        read_to: end,
                                    })
                                }
                                _ => unreachable!(),
                            }
                        })
                    }
                });
                let source = [OTHER_SPEECH, PAUSE, OWN_SPEECH].concat();
                let log = CallLog::default();
                let pipeline = filtered_pipeline(
                    Finished(source.clone()),
                    diarizer(VoiceByLevel),
                    Transcription::<_, crate::transcription::Whole>::delegated(model),
                    (log.history(), log.cursor()),
                );
                let cancel = CancelToken::new();
                let result = if live {
                    pipeline
                        .run(own_voice_only(settings(
                            DeliveryMode::Complete,
                            OutputMode::Insert,
                        )))
                        .map(|result| result.text)
                } else {
                    pipeline
                        .transcribe(
                            &mut RecordingReader::from_samples(&source),
                            &Language::Automatic,
                            true,
                            None,
                            "test",
                            &cancel,
                        )
                        .map(|heard| heard.transcript.into_text())
                };
                match failure {
                    "inference" => assert!(matches!(
                        result,
                        Err(DictationError::Transcription(
                            TranscriptionError::Inference { .. }
                        ))
                    )),
                    "no-segments" => assert!(matches!(
                        result,
                        Err(DictationError::Transcription(
                            TranscriptionError::InvalidSegment
                        ))
                    )),
                    "cancel" => assert!(matches!(result, Err(DictationError::Cancelled))),
                    "no-speech" => assert!(matches!(result, Err(DictationError::NoSpeech))),
                    _ => unreachable!(),
                }
                assert!(log.entries().iter().all(|entry| entry == "prepare:Insert"));
            }
        }
    }

    #[test]
    fn complete_context_timed_rejection_does_not_retry_the_decoder() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for live in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let source = [OTHER_SPEECH, PAUSE, OWN_SPEECH].concat();
            let log = CallLog::default();
            let pipeline = filtered_pipeline(
                Finished(source.clone()),
                diarizer(VoiceByLevel),
                // Hears only the guest, timed inside the guest's turn.
                Transcription::complete_samples(Hypotheses({
                    let calls = Arc::clone(&calls);
                    move |_| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        Ok(timed("other", &[(0.2, 1.2)]))
                    }
                })),
                (log.history(), log.cursor()),
            );
            let result = if live {
                pipeline
                    .run(own_voice_only(settings(
                        DeliveryMode::Complete,
                        OutputMode::Insert,
                    )))
                    .map(|result| result.text)
            } else {
                pipeline
                    .transcribe(
                        &mut RecordingReader::from_samples(&source),
                        &Language::Automatic,
                        true,
                        None,
                        "test",
                        &CancelToken::never(),
                    )
                    .map(|heard| heard.transcript.into_text())
            };
            assert!(matches!(result, Err(DictationError::NoSpeech)));
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert!(log.entries().iter().all(|entry| entry == "prepare:Insert"));
        }
    }

    #[test]
    fn complete_context_wav_fallback_keeps_owned_recordings_and_original_samples() {
        let directory = tempfile::tempdir().unwrap();
        let source = [OTHER_SPEECH, PAUSE, OWN_SPEECH].concat();
        let mut original = RecordingReader::from_samples_in(directory.path(), &source).unwrap();
        let path = original
            .finished_audio()
            .unwrap()
            .path()
            .unwrap()
            .to_owned();
        let capture = WavReplayCapture::new(
            directory.path(),
            Duration::from_secs(20),
            &path,
            ReplayPace::Immediate,
        )
        .unwrap();
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let log = CallLog::default();
        let pipeline = filtered_pipeline(
            capture,
            diarizer(VoiceByLevel),
            Transcription::complete_wav(wav_levels(Arc::clone(&inputs))),
            (log.history(), log.cursor()),
        );

        assert_eq!(
            pipeline
                .run(own_voice_only(settings(
                    DeliveryMode::Complete,
                    OutputMode::Insert
                )))
                .unwrap()
                .text,
            "own"
        );
        let mut recording = RecordingReader::from_wav_file(&path).unwrap();
        assert_eq!(
            pipeline
                .transcribe(
                    &mut recording,
                    &Language::Automatic,
                    true,
                    None,
                    "test",
                    &CancelToken::never()
                )
                .unwrap()
                .transcript
                .as_str(),
            "own"
        );
        assert_eq!(*inputs.lock().unwrap(), [true, false, true, false]);
        assert_eq!(delivered(&log.entries()), "own");
    }

    #[test]
    fn complete_context_finalization_runs_once_before_selected_text_is_published() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for fail in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let layer = FinalLayer({
                let calls = Arc::clone(&calls);
                move |text: &mut String, _: &CancelToken| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(text, "own");
                    if fail {
                        return Err(TranscriptionError::InvalidSegment);
                    }
                    text.push_str(" processed");
                    Ok(())
                }
            });
            let log = CallLog::default();
            let pipeline = filtered_pipeline(
                Finished([OTHER_SPEECH, PAUSE, OWN_SPEECH].concat()),
                diarizer(VoiceByLevel),
                Transcription::complete_samples(levels(Arc::default())),
                (log.history(), log.cursor()),
            )
            .rewriting(
                Rewrites::new()
                    .backtracking(true)
                    .removing_disfluencies(true)
                    .with(layer),
            );
            let (events, captions) = caption_events();
            let result = pipeline.run_observed(
                own_voice_only(settings(DeliveryMode::Complete, OutputMode::Insert)),
                &events,
            );
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            if fail {
                assert!(matches!(
                    result,
                    Err(DictationError::Transcription(
                        TranscriptionError::InvalidSegment
                    ))
                ));
                assert_eq!(log.entries(), ["prepare:Insert"]);
                assert!(
                    captions
                        .try_iter()
                        .all(|caption| caption.committed.is_empty()
                            && caption.provisional.is_empty())
                );
            } else {
                assert_eq!(result.unwrap().text, "own processed");
                assert_eq!(
                    log.entries(),
                    [
                        "prepare:Insert",
                        "segment:own",
                        "completed:own processed",
                        "deliver:own processed"
                    ]
                );
            }
        }
    }

    /// A speech gate that fails on every frame.
    struct Deaf;

    impl VoiceActivityDetector for Deaf {
        const FRAME_SAMPLES: usize = 4;

        fn probability(
            &mut self,
            _frame: &[i16],
        ) -> Result<f32, aria_kit::voice_activity::VoiceActivityError> {
            Err(aria_kit::voice_activity::VoiceActivityError::InvalidProbability(f32::NAN))
        }
    }

    #[test]
    fn a_failing_speech_gate_keeps_every_voice_out() {
        for delivery in [DeliveryMode::Streaming, DeliveryMode::Complete] {
            let (_directory, capture) = judged_capture();
            let log = CallLog::default();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
                (log.history(), log.cursor()),
            )
            .hearing_speech(Some(Deaf))
            .tracking_voices(Some(diarizer(Voice(OWN))), Some(LevelSegmenter));

            let result = pipeline.run(own_voice_only(settings(delivery, OutputMode::Insert)));

            assert!(
                matches!(result, Err(DictationError::Speaker(_))),
                "{delivery:?}"
            );
            assert_eq!(delivered(&log.entries()), "", "{delivery:?}");
        }
    }

    #[test]
    fn live_owner_filtering_requires_filter_inputs() {
        for delivery in [DeliveryMode::Streaming, DeliveryMode::Complete] {
            for (case, speech, diarizer, segmenter) in unavailable_filters() {
                let log = CallLog::default();
                let pipeline = DictationPipeline::new(
                    Finished(OWN_SPEECH.to_vec()),
                    Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
                    (log.history(), log.cursor()),
                )
                .hearing_speech(speech)
                .tracking_voices(diarizer, segmenter);

                let result = pipeline.run(own_voice_only(settings(delivery, OutputMode::Insert)));

                assert!(
                    matches!(
                        result,
                        Err(DictationError::Speaker(TrackingError::Unavailable))
                    ),
                    "{case}, {delivery:?}"
                );
                assert_eq!(delivered(&log.entries()), "", "{case}, {delivery:?}");
            }
        }
    }

    #[test]
    fn saved_owner_filtering_requires_filter_inputs() {
        for (case, speech, diarizer, segmenter) in unavailable_filters() {
            let pipeline = DictationPipeline::new(
                Finished(OWN_SPEECH.to_vec()),
                Transcription::complete_samples(Answering("hello")),
                UnreachableSink,
            )
            .hearing_speech(speech)
            .tracking_voices(diarizer, segmenter);
            let mut recording = RecordingReader::from_samples(OWN_SPEECH);

            let result = pipeline.transcribe(
                &mut recording,
                &Language::Automatic,
                true,
                None,
                "test",
                &CancelToken::never(),
            );

            assert!(
                matches!(
                    result,
                    Err(DictationError::Speaker(TrackingError::Unavailable))
                ),
                "{case}"
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

            let result = published.finish(Transcript::new("one two").unwrap(), false, &mut sinks);

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

        let result = published.finish(whole, false, &mut sinks).unwrap();

        assert_eq!(result.transcript.as_str(), "run");
        assert_eq!(delivered(&log.entries()), "run");
    }

    #[test]
    fn one_speech_frame_preserves_a_short_word_in_each_delivery_mode() {
        struct OneFrame(bool);
        impl VoiceActivityDetector for OneFrame {
            const FRAME_SAMPLES: usize = 512;
            fn probability(
                &mut self,
                _: &[i16],
            ) -> Result<f32, aria_kit::voice_activity::VoiceActivityError> {
                Ok(if std::mem::replace(&mut self.0, false) {
                    1.0
                } else {
                    0.0
                })
            }
            fn reset(&mut self) {
                self.0 = true;
            }
        }
        for mode in [DeliveryMode::Streaming, DeliveryMode::Complete] {
            let (_directory, capture) = replay(|directory| fixture_of(directory, 4_000));
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("yeah")),
                CallLog::default().history(),
            )
            .hearing_speech(Some(OneFrame(true)));
            assert_eq!(
                pipeline
                    .run(settings(mode, OutputMode::Insert))
                    .unwrap()
                    .text,
                "yeah"
            );
        }
    }

    #[test]
    fn a_short_silence_does_not_end_a_streamed_sentence() {
        let directory = tempfile::tempdir().unwrap();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let (events, stages) = stage_events();
        let settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
        let mut published = Published::new(&settings, &control, &events, Instant::now());
        let log = CallLog::default();
        let mut sinks = (log.history(), log.cursor());
        sinks.dictation_started(&settings, &control).unwrap();
        // Each segment after the first was decoded with the audio before
        // its cut, so its text and its reading of the boundary come from a
        // model that heard the sentence so far.
        for (text, ended_before) in [
            ("with the capital character.", None),
            ("while it shouldn't.", Some(false)),
            ("Ship it.", Some(true)),
        ] {
            let mut heard = heard(directory.path(), text);
            heard.segment = heard.segment.with_sentence_ended_before(ended_before);
            published.segment(heard, text, None, &mut sinks).unwrap();
        }
        let whole = Transcript::new("unused").unwrap();

        let result = published.finish(whole, false, &mut sinks).unwrap();

        let said = "with the capital character while it shouldn't. Ship it.";
        assert_eq!(result.transcript.as_str(), said);
        assert_eq!(delivered(&log.entries()), said);
        let announced: Vec<_> = stages
            .try_iter()
            .filter_map(|stage| match stage {
                DictationStage::SegmentTranscribed { text, .. } => Some(text.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            announced,
            [
                "with the capital character",
                "while it shouldn't",
                ". Ship it",
                "."
            ]
        );
    }

    #[test]
    fn a_closing_period_that_fails_to_type_does_not_fail_the_press() {
        /// Rejects the closing period alone, as the cursor does once focus
        /// moved after the last segment.
        struct FailingPeriod;

        impl TranscriptSink for FailingPeriod {
            type Error = DictationError;

            fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), DictationError> {
                if utterance.text == "." {
                    return Err(DictationError::DeliveryTargetChanged);
                }
                Ok(())
            }

            fn completed(&mut self, _utterance: Utterance<'_>) -> Result<(), DictationError> {
                Ok(())
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let events = DictationEventEmitter::unsubscribed();
        let settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
        let mut published = Published::new(&settings, &control, &events, Instant::now());
        let heard = heard(directory.path(), "we should.");
        published
            .segment(heard, "we should.", None, &mut FailingPeriod)
            .unwrap();
        let whole = Transcript::new("unused").unwrap();

        let result = published.finish(whole, false, &mut FailingPeriod).unwrap();

        assert_eq!(result.transcript.as_str(), "we should.");
    }

    #[test]
    fn complete_delivery_publishes_segments_unjoined() {
        let directory = tempfile::tempdir().unwrap();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let events = DictationEventEmitter::unsubscribed();
        let settings = settings(DeliveryMode::Complete, OutputMode::Insert);
        let mut published = Published::new(&settings, &control, &events, Instant::now());
        let log = CallLog::default();
        let mut sinks = (log.history(), log.cursor());
        for text in ["we should.", "And ship it."] {
            let heard = heard(directory.path(), text);
            published.segment(heard, text, None, &mut sinks).unwrap();
        }

        assert_eq!(
            log.entries(),
            ["segment:we should.", "segment:And ship it."]
        );
    }

    #[test]
    fn unverified_words_are_typed_without_owner_provenance() {
        for mode in [DeliveryMode::Streaming, DeliveryMode::Complete] {
            let (_directory, capture) = replay(|directory| fixture_of(directory, 4_000));
            let log = CallLog::default();
            let pipeline = filtered_pipeline(
                capture,
                diarizer(VoiceByLevel),
                Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("yeah")),
                (log.history(), log.cursor()),
            );
            let (send, receive) = std::sync::mpsc::channel();
            let events = DictationEventEmitter::forwarded_by(move |event| {
                if let super::super::DictationEvent::Stage(DictationStage::SpeakerChanged(voice)) =
                    event
                {
                    send.send(voice).unwrap();
                }
            });
            let result = pipeline
                .run_observed(own_voice_only(settings(mode, OutputMode::Insert)), &events)
                .unwrap();
            assert_eq!(result.text, "yeah");
            assert!(
                !result.own_voice,
                "admission is not owner verification: {mode:?}"
            );
            let voices: Vec<_> = receive.try_iter().collect();
            assert!(voices.contains(&HeardSpeaker::Typed(None)));
            assert!(!voices.contains(&HeardSpeaker::KeptOut));
        }
    }

    /// Runs one press and returns its result and the sink log.
    fn press<C: AudioCapture, T: Transcriber, M: Mode, V: VoiceActivityDetector + Send>(
        capture: C,
        transcription: Transcription<T, M>,
        detector: Option<V>,
        delivery: DeliveryMode,
    ) -> (Result<DictationResult, DictationError>, Vec<String>) {
        let log = CallLog::default();
        let pipeline =
            DictationPipeline::new(capture, transcription, (log.history(), log.cursor()))
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
                Hypothesis::new("Mm-hmm. hello"),
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
            "test",
            &CancelToken::never(),
        );
        let heard = pipeline.transcribe(
            &mut spoken,
            &Language::Automatic,
            false,
            None,
            "test",
            &CancelToken::never(),
        );

        assert!(matches!(refused, Err(DictationError::NoSpeech)));
        assert_eq!(
            heard.expect("the recording is heard").transcript.as_str(),
            "Mm-hmm."
        );
    }

    /// The pill reads where the text went off this event, so the outcome
    /// has to survive the whole press, not only the delivery call.
    #[test]
    fn the_delivered_stage_names_where_the_text_went() {
        let (events, stages) = stage_events();
        let (_directory, capture) = capture();
        let log = CallLog::default();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("hello")),
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
        let (events, stages) = stage_events();
        let (_directory, capture) = judged_capture();
        let log = CallLog::default();
        let pipeline = filtered_pipeline(
            capture,
            diarizer(Voice(OTHER)),
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            (log.history(), log.cursor()),
        );
        let settings = own_voice_only(settings(DeliveryMode::Streaming, OutputMode::Insert));

        let result = pipeline.run_observed(settings, &events);
        drop(events);

        assert!(
            matches!(result, Err(DictationError::NoSpeech)),
            "{result:?}"
        );
        assert_eq!(log.entries(), ["prepare:Insert"]);
        // The ring may skip a state the walk passed through, so only its
        // last word before the failure is pinned.
        let stages: Vec<_> = stages.iter().collect();
        assert_eq!(
            stages.first(),
            Some(&DictationStage::Recording(RecordingId(0)))
        );
        assert_eq!(
            stages[stages.len().saturating_sub(2)..],
            [
                DictationStage::SpeakerChanged(HeardSpeaker::KeptOut),
                DictationStage::Failed(RpcErrorCode::NoSpeech),
            ]
        );
    }

    #[test]
    fn a_relayed_recording_is_heard_only_in_the_enrolled_voice() {
        for voice in [OWN, OTHER] {
            let (directory, capture) = judged_capture();
            let pipeline = filtered_pipeline(
                capture,
                diarizer(Voice(voice)),
                Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
                (),
            );
            let mut recording = RecordingReader::from_samples_in(directory.path(), &[0.5; 64_000])
                .expect("the test recording is written");

            let heard = pipeline.transcribe(
                &mut recording,
                &Language::Automatic,
                true,
                None,
                "test",
                &CancelToken::never(),
            );

            if voice == OWN {
                let heard = heard.expect("the enrolled voice is typed");
                assert_eq!(heard.transcript.as_str(), "hello");
            } else {
                assert!(matches!(heard, Err(DictationError::NoSpeech)));
            }
        }
    }

    #[test]
    fn retry_honors_cancellation() {
        let (directory, capture) = capture();
        let pipeline = DictationPipeline::new(
            capture,
            Transcription::complete_samples(Answering("must not escape")),
            (),
        );
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.5; 1600]).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            pipeline.transcribe(
                &mut recording,
                &Language::Automatic,
                false,
                None,
                "test",
                &cancel
            ),
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
    fn without_the_filter_no_segment_is_named() {
        for mode in [DeliveryMode::Streaming, DeliveryMode::Complete] {
            let (_directory, capture) = judged_capture();
            let log = CallLog::default();
            let pipeline = filtered_pipeline(
                capture,
                diarizer(Voice(OWN)),
                Transcription::agreement(Cadence::from_millis(1_000, 20_000), Answering("hello")),
                (log.history(), log.cursor()),
            );
            let (events, stages) = stage_events();

            pipeline
                .run_observed(settings(mode, OutputMode::Insert), &events)
                .unwrap();

            let stages: Vec<_> = stages.try_iter().collect();
            let speakers: Vec<_> = stages
                .iter()
                .filter_map(|stage| match stage {
                    DictationStage::SegmentTranscribed { speaker, .. } => Some(speaker),
                    _ => None,
                })
                .collect();
            assert_eq!(speakers, [&None], "{mode:?}");
            assert!(
                !stages
                    .iter()
                    .any(|stage| matches!(stage, DictationStage::SpeakerChanged(_))),
                "the ring stays silent, {mode:?}"
            );
        }
    }

    /// Each press builds its own tracker, so nothing one press heard
    /// carries into the next.
    #[test]
    fn a_second_owner_only_press_hears_as_the_first_did() {
        let (_directory, capture) = judged_capture();
        let log = CallLog::default();
        let pipeline = filtered_pipeline(
            capture,
            diarizer(Voice(OWN)),
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            (log.history(), log.cursor()),
        );
        let press = || {
            let (events, stages) = stage_events();
            let settings = own_voice_only(settings(DeliveryMode::Streaming, OutputMode::Insert));
            let text = pipeline.run_observed(settings, &events).unwrap().text;
            let ring: Vec<_> = stages
                .try_iter()
                .filter_map(|stage| match stage {
                    DictationStage::SpeakerChanged(voice) => Some(voice),
                    _ => None,
                })
                .collect();
            (text, ring)
        };

        let first = press();
        let second = press();

        assert_eq!(first.0, "hello");
        assert_eq!(second.0, first.0);
        // How many steps the ring took to name the owner depends on when
        // the walk published; where it settled does not.
        assert_eq!(second.1.last(), first.1.last());
        assert!(first.1.last().is_some(), "the ring named the owner");
    }

    #[test]
    fn whole_decoding_names_the_voice_while_capture_is_open() {
        let writer = Writer::default();
        let pipeline = filtered_pipeline(
            Live {
                writer: Arc::clone(&writer),
                opening: OWN_SPEECH,
            },
            diarizer(Voice(OWN)),
            Transcription::complete_samples(Answering("hello")),
            CallLog::default().history(),
        );
        let (send, receive) = std::sync::mpsc::channel();
        let events = DictationEventEmitter::forwarded_by(move |event| {
            if let super::super::DictationEvent::Stage(DictationStage::SpeakerChanged(voice)) =
                event
            {
                send.send(voice).unwrap();
            }
        });
        let heard = std::thread::scope(|scope| {
            let press = scope.spawn(|| {
                pipeline.run_observed(
                    own_voice_only(settings(DeliveryMode::Complete, OutputMode::Insert)),
                    &events,
                )
            });
            let heard = loop {
                match receive.recv_timeout(Duration::from_secs(2)) {
                    Ok(HeardSpeaker::Undecided | HeardSpeaker::Typed(None)) => continue,
                    heard => break heard,
                }
            };
            writer.lock().unwrap().take().unwrap().finish();
            press.join().unwrap().unwrap();
            heard
        });
        assert!(matches!(heard, Ok(HeardSpeaker::Typed(Some(_)))));
    }

    #[test]
    fn own_voice_only_types_what_the_enrolled_voice_said() {
        let (_directory, capture) = judged_capture();
        let log = CallLog::default();
        let pipeline = filtered_pipeline(
            capture,
            diarizer(Voice(OWN)),
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello")),
            (log.history(), log.cursor()),
        );
        let result = pipeline.run(own_voice_only(settings(
            DeliveryMode::Streaming,
            OutputMode::Insert,
        )));

        let result = result.unwrap();
        assert!(result.own_voice);
        let text = result.text;
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
            let pipeline = filtered_pipeline(
                capture,
                diarizer(Voice(voice)),
                Transcription::complete_samples(Echoing),
                (log.history(), log.cursor()),
            );
            let (send, stages) = std::sync::mpsc::channel();
            let events = DictationEventEmitter::forwarded_by(move |event| {
                if let super::super::DictationEvent::Stage(DictationStage::SegmentTranscribed {
                    speaker,
                    ..
                }) = event
                {
                    let _ = send.send(speaker);
                }
            });
            let result = pipeline.run_observed(
                own_voice_only(settings(DeliveryMode::Complete, OutputMode::Insert)),
                &events,
            );

            match expected {
                Some(text) => {
                    assert_eq!(result.unwrap().text, text);
                    assert!(log.entries().contains(&"deliver:hello".to_owned()));
                    let alex = SpeakerName::new("alex").unwrap();
                    assert_eq!(stages.try_iter().collect::<Vec<_>>(), [Some(alex)]);
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
