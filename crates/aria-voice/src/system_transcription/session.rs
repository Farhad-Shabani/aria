//! The system transcription: a thread carrying system audio through streaming
//! transcription into the transcript file. Hearing decodes it and judges its
//! speech and voices; the voice track settles who spoke afterwards.

use std::ops::Range;
use std::sync::Arc;
use std::thread;

use aria_kit::audio::canonical_duration;
use aria_kit::cancel::CancelToken;
use aria_kit::speaker::{Admission, Released, TurnFeed};
use aria_kit::voice_activity::{SpeechDetector, VoiceActivityDetector};
use aria_stt::EngineConfig;

use crate::config::{SpeakerConfig, SystemTranscriptionConfig, SystemTranscriptionEngine};
use crate::hearing::{Board, Decoding, Event, Hearing, HearingError};
use crate::stt::{Language, Recognizer, Stream, Streaming, Transcription, TranscriptionError};
use crate::vocabulary::Vocabulary;
use aria_kit::audio::RecordingReader;

use super::event::{SystemTranscriptionEventEmitter, SystemTranscriptionStage};
use super::{SystemAudioCapture, SystemTranscriptionError, TranscriptFile};

use crate::dictation::{Spacing, Utterance};
use crate::speaker::{self, Diarizer, Label, VoiceTrack, VoiceprintTracker};
use aria_speaker::wespeaker::WeSpeaker;

use super::track::VoiceThread;

/// Loads the engine and transcribes until capture ends, then returns how the session ended. The
/// transcript file is final by then. The caller announces the end with `Stopped` or `Failed`.
/// `vocabulary` corrects each segment before it is written.
#[allow(
    clippy::too_many_arguments,
    reason = "the daemon hands over each part a session owns"
)]
pub fn run_session(
    config: &SystemTranscriptionConfig,
    speaker_config: &SpeakerConfig,
    vocabulary: &Vocabulary,
    events: &SystemTranscriptionEventEmitter,
    capture: SystemAudioCapture,
    recording: RecordingReader,
    sink: TranscriptFile,
    cancel: &CancelToken,
) -> Result<(), SystemTranscriptionError> {
    events.emit(SystemTranscriptionStage::Started);
    let session = Session { vocabulary, events };
    session.run(
        &config.engine,
        cancel,
        speaker_config,
        capture,
        recording,
        sink,
    )
}

struct Session<'a> {
    vocabulary: &'a Vocabulary,
    events: &'a SystemTranscriptionEventEmitter,
}

/// The models that system transcription tells speech and voices apart with: VAD, the diarizer
/// that labels voices live, and the voice track that settles them. The
/// diarizer and the track each load the speaker model, so neither waits on
/// the other's inference.
struct Listening<V> {
    vad: Option<V>,
    diarizer: Option<Diarizer<WeSpeaker>>,
    track: Option<VoiceTrack>,
}

impl Listening<aria_kit::voice_activity::SileroVad> {
    fn load(config: &SpeakerConfig) -> Self {
        let enrolled = speaker::enrolled(config);
        let vad = speaker::speech_gate();
        // The diarizer labels the speech detector's turns. Without one, it
        // would load and never run.
        let diarizer = vad
            .as_ref()
            .and_then(|_| speaker::embedder(config))
            .map(|embedder| Diarizer::new(embedder, enrolled.clone()));
        let track = speaker::embedder(config).and_then(|embedder| {
            speaker::voice_track(
                &config.segmentation_model,
                embedder,
                enrolled,
                speaker::Grouping::default(),
            )
        });
        Self {
            vad,
            diarizer,
            track,
        }
    }
}

/// `SystemTranscriptionEngine` admits only an engine that streams.
fn load(
    transcription: &SystemTranscriptionEngine,
    cancel: &CancelToken,
) -> Result<(Stream, Box<dyn Recognizer>), TranscriptionError> {
    let engine = EngineConfig::try_from(transcription.config().clone())?;
    let stream = engine.stream().ok_or(TranscriptionError::NoNativeStream)?;
    Ok((stream, engine.load(cancel)?))
}

impl Session<'_> {
    fn run(
        &self,
        transcription: &SystemTranscriptionEngine,
        cancel: &CancelToken,
        speaker: &SpeakerConfig,
        capture: SystemAudioCapture,
        recording: RecordingReader,
        mut sink: TranscriptFile,
    ) -> Result<(), SystemTranscriptionError> {
        // Forked before the model loads, because loading is seconds of audio the
        // track would otherwise never hear.
        let track_audio = recording.fork();
        let result = match load(transcription, cancel) {
            Ok((stream, engine)) => self.transcribe_all(
                recording,
                &Transcription::stream(stream, engine),
                Listening::load(speaker),
                track_audio,
                &mut sink,
                || capture.stopper().stop(),
            ),
            Err(TranscriptionError::Cancelled) if cancel.is_cancelled() => Ok(()),
            Err(error) => Err(SystemTranscriptionError::Transcription(error)),
        };
        capture.stopper().stop();
        result.and(capture.finish())
    }

    fn transcribe_all<T, V>(
        &self,
        mut recording: RecordingReader,
        transcription: &Transcription<T, Streaming>,
        mut listening: Listening<V>,
        track_audio: RecordingReader,
        sink: &mut TranscriptFile,
        // Ends capture. The threads that hear the recording end with it, so
        // a session that stopped for any other reason has to end capture
        // before they are joined, or the join waits for audio nobody will
        // write.
        end_capture: impl Fn(),
    ) -> Result<(), SystemTranscriptionError>
    where
        T: Recognizer,
        V: VoiceActivityDetector + Send,
    {
        // A stop cancels the session before it ends capture, and the decoder
        // still owes the words it holds. So hearing ends with capture, and
        // stops early only when abandoned.
        let board = Board::new();
        let cancel = board.token(|| false);
        let operation = match transcription.start_with(&Language::Automatic, &cancel) {
            Ok(operation) => operation,
            Err(error) => {
                end_capture();
                return Err(SystemTranscriptionError::Transcription(error));
            }
        };
        let from = recording.held_from();
        let speech = listening
            .vad
            .as_mut()
            .map(|vad| SpeechDetector::new(vad, from));
        let tracker = listening
            .diarizer
            .as_mut()
            .map(|diarizer| VoiceprintTracker::numbering(diarizer, from));
        let feed = speech.map(|speech| TurnFeed::start(&mut recording, speech, tracker));
        let (heard_all, settled) = thread::scope(|scope| {
            // The scope joins the voice track, which reads until capture
            // ends. Ending it here also frees that join when a panic unwinds.
            let _ends_capture = EndsCapture(&end_capture);
            let mut track = listening.track.and_then(|track| {
                VoiceThread::spawn(scope, track, track_audio)
                    .inspect_err(|error| {
                        tracing::warn!(%error, "the voice track could not start; speakers stay as heard");
                    })
                    .ok()
            });
            let decoding = Decoding {
                operation,
                recording: &mut recording,
                previews: false,
            };
            // Whatever ends hearing, the paragraphs already on disk carry
            // live labels. Settling them is the last thing this session owes
            // the reader, so a failure must not skip it.
            let hearing = Hearing::start(scope, decoding, feed, Admission::Spoken, &board, &cancel);
            let heard_all = match hearing {
                Ok(hearing) => self.hear(hearing, sink, track.as_ref(), &end_capture),
                Err(error) => {
                    end_capture();
                    Err(system_transcription_error(error))
                }
            };
            let settled = track.as_mut().map(|track| {
                // Hearing ends on the last segment; the track may still be
                // short of it. The track's drain limit bounds this wait.
                let merged = track.finish();
                emit_merges(self.events, &merged);
                // The track read the audio itself, on its own clock, so
                // its labels replace the live ones. Paragraphs past its
                // reach lose their live label.
                sink.rewrite(|spans| attribute_reached(track, spans))
            });
            (heard_all, settled)
        });
        // Without the voice track, the live labels were given in the order
        // voices spoke. Grouping the whole session at once mends them.
        let settled = settled.unwrap_or_else(|| match &listening.diarizer {
            Some(diarizer) => sink.relabel(|embeddings| diarizer.relabel(embeddings)),
            None => Ok(()),
        });
        heard_all?;
        settled.map_err(SystemTranscriptionError::Transcript)
    }

    /// Writes what `hearing` releases until it ends. Capture ends on any
    /// failure, before the hearing drops: dropping it joins threads that
    /// wait on capture.
    fn hear(
        &self,
        mut hearing: Hearing<'_>,
        sink: &mut TranscriptFile,
        track: Option<&VoiceThread<'_>>,
        end_capture: &impl Fn(),
    ) -> Result<(), SystemTranscriptionError> {
        let mut heard = self.follow(&mut hearing, sink, track);
        if heard.is_ok() {
            heard = match hearing.finish() {
                Ok(ended) => ended.events.into_iter().try_for_each(|event| match event {
                    Event::Released(released) => self.write(released, sink),
                    Event::Timeline(_) | Event::Preview(_) | Event::Idle => Ok(()),
                }),
                Err(error) => system_transcription_ended(error),
            };
        }
        if heard.is_err() {
            end_capture();
        }
        heard
    }

    /// Writes what the hearing releases until the decoder is done.
    fn follow(
        &self,
        hearing: &mut Hearing<'_>,
        sink: &mut TranscriptFile,
        track: Option<&VoiceThread<'_>>,
    ) -> Result<(), SystemTranscriptionError> {
        loop {
            match hearing.next() {
                Ok(Some(Event::Released(released))) => self.write(released, sink)?,
                Ok(Some(Event::Timeline(_) | Event::Preview(_) | Event::Idle)) => {}
                Ok(None) => return Ok(()),
                Err(error) => return Err(system_transcription_error(error)),
            }
            // The track walks on its own thread. Only what it decided has to
            // come back here, where the file and the emitter live.
            if let Some(track) = track {
                let merged = track.merged();
                emit_merges(self.events, &merged);
                // Only a merge changes a label already written. The flush it
                // forces can split a paragraph, which the next rewrite's
                // coalescing joins back up.
                if !merged.is_empty() {
                    sink.rewrite(|spans| attribute_reached(track, spans))
                        .map_err(SystemTranscriptionError::Transcript)?;
                }
            }
        }
    }

    /// Corrects a released segment and writes it under its live label.
    fn write(
        &self,
        released: Released,
        sink: &mut TranscriptFile,
    ) -> Result<(), SystemTranscriptionError> {
        let heard = released.heard;
        let mut text = heard.segment.as_str().to_owned();
        if let Some(corrected) = self.vocabulary.correct(&text) {
            text = corrected;
        }
        let label = released.speaker;
        self.events
            .emit(SystemTranscriptionStage::SegmentTranscribed {
                text: Arc::from(text.as_str()),
                label: label.as_ref().map(|label| Arc::from(label.to_string())),
                at_seconds: canonical_duration(heard.window.start).as_secs(),
            });
        sink.voiced_segment(
            Utterance {
                text: &text,
                speaker: label.as_ref(),
                at: heard.window.start,
                span: heard.span,
                spacing: Spacing::default(),
            },
            released.embedding.as_deref(),
        )
        .map_err(SystemTranscriptionError::Transcript)
    }
}

struct EndsCapture<F: Fn()>(F);

impl<F: Fn()> Drop for EndsCapture<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// A capture error is the recording ending abnormally; capture's result in
/// `finish()` names the real cause. A session nobody spoke in is an empty
/// transcript, not a failure.
fn system_transcription_ended(error: HearingError) -> Result<(), SystemTranscriptionError> {
    match error {
        HearingError::Transcription(
            TranscriptionError::Capture(_) | TranscriptionError::NoSpeech,
        ) => Ok(()),
        error => Err(system_transcription_error(error)),
    }
}

fn system_transcription_error(error: HearingError) -> SystemTranscriptionError {
    match error {
        HearingError::Transcription(error) => SystemTranscriptionError::Transcription(error),
        HearingError::Tracking(error) => {
            SystemTranscriptionError::Transcription(TranscriptionError::Inference {
                backend: "voice tracking",
                source: aria_kit::backend::BackendError::engine(error),
            })
        }
    }
}

fn attribute_reached(track: &VoiceThread<'_>, spans: &[Option<Range<u64>>]) -> Vec<Option<Label>> {
    let voices = track.attribution();
    let reached = reached(spans, voices.retained_from());
    crate::speaker::attribute_spans(&*voices, &spans[..reached])
}

/// How many paragraphs the track has walked past. Untimed paragraphs count
/// only when a timed one after them was reached.
fn reached(spans: &[Option<Range<u64>>], reach: u64) -> usize {
    spans
        .iter()
        .rposition(|span| span.as_ref().is_some_and(|span| span.end <= reach))
        .map_or(0, |last| last + 1)
}

/// Tells whoever is watching which voices turned out to be one.
fn emit_merges(events: &SystemTranscriptionEventEmitter, merged: &[crate::speaker::Merged]) {
    for merge in merged {
        events.emit(SystemTranscriptionStage::SpeakerMerged {
            from: Arc::from(merge.from.to_string()),
            into: Arc::from(merge.into.to_string()),
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{SttConfig, SttDevice};
    use crate::dictation::testing::Loud;
    use crate::stt::{Cadence, Hypothesis, RecognitionAudio};
    use crate::vocabulary::{PersonalRules, Term};

    use super::*;

    #[test]
    fn stopping_during_model_loading_finishes_capture() {
        for (cancelled, capture_failed) in [(true, false), (true, true), (false, false)] {
            let directory = tempfile::tempdir().unwrap();
            let (sink, _) = TranscriptFile::create(directory.path()).unwrap();
            let capture_outcome = if capture_failed {
                Err(SystemTranscriptionError::PermissionDenied)
            } else {
                Ok(())
            };
            let (capture, recording) = SystemAudioCapture::spawn_fake(capture_outcome).unwrap();
            let stopped = capture.stopper();
            let cancel = CancelToken::new();
            if cancelled {
                cancel.cancel();
            }
            let speaker = SpeakerConfig {
                model: directory.path().join("speaker"),
                segmentation_model: directory.path().join("segmentation"),
                profiles_dir: directory.path().join("profiles"),
            };
            let vocabulary = Vocabulary::default();
            let events = SystemTranscriptionEventEmitter::unsubscribed();
            let session = Session {
                vocabulary: &vocabulary,
                events: &events,
            };

            let parakeet = SystemTranscriptionEngine::new(SttConfig::Parakeet {
                model_directory: directory.path().to_path_buf(),
                device: SttDevice::Cpu,
            })
            .unwrap();
            let result = session.run(&parakeet, &cancel, &speaker, capture, recording, sink);

            assert!(stopped.is_stopped());
            if !cancelled {
                assert!(matches!(
                    result,
                    Err(SystemTranscriptionError::Transcription(_))
                ));
            } else if capture_failed {
                assert!(matches!(
                    result,
                    Err(SystemTranscriptionError::PermissionDenied)
                ));
            } else {
                assert!(result.is_ok(), "stop returned {result:?}");
            }
        }
    }

    struct Answering(&'static str);

    impl Recognizer for Answering {
        fn validate_language(
            &self,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn recognize_with(
            &self,
            _audio: RecognitionAudio<'_>,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            Ok(Hypothesis::new(self.0))
        }
    }

    /// Transcribes `audio`, answered with `text`, into a fresh transcript
    /// file, and returns what the file holds.
    fn transcribed(
        audio: &[f32],
        text: &'static str,
        vad: Option<Loud>,
        vocabulary: &Vocabulary,
    ) -> String {
        let directory = tempfile::tempdir().expect("test directory is created");
        let (mut sink, path) =
            TranscriptFile::create(directory.path()).expect("the transcript file is created");
        let recording = RecordingReader::from_samples(audio);
        let track_audio = recording.fork();
        let transcription =
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering(text));
        let session = Session {
            vocabulary,
            events: &SystemTranscriptionEventEmitter::unsubscribed(),
        };
        let listening = Listening {
            vad,
            diarizer: None,
            track: None,
        };

        session
            .transcribe_all(
                recording,
                &transcription,
                listening,
                track_audio,
                &mut sink,
                || {},
            )
            .expect("the session should complete");
        drop(sink);
        std::fs::read_to_string(path).expect("the transcript file is readable")
    }

    #[test]
    fn only_paragraphs_inside_the_reach_are_offered_to_the_track() {
        let spans = [Some(0..10), None, Some(10..20), Some(15..30), None];
        assert_eq!(reached(&spans, 20), 3);
        assert_eq!(reached(&spans, 5), 0);
    }

    #[test]
    fn words_with_no_speech_under_them_are_not_written() {
        let vocabulary = Vocabulary::default();
        assert!(!transcribed(&[0.0; 32_000], "hello", Some(Loud), &vocabulary).contains("hello"));
        assert!(transcribed(&[0.5; 32_000], "hello", Some(Loud), &vocabulary).contains("hello"));
    }

    #[test]
    fn a_session_spells_terms_as_the_vocabulary_does() {
        let vocabulary = Vocabulary::compile(
            &[Term {
                canonical: "aria-stt".into(),
                priority: 1,
            }],
            PersonalRules::default(),
        )
        .unwrap();

        let written = transcribed(&[0.5; 32_000], "aria s t t", None, &vocabulary);

        assert!(written.contains("aria-stt"), "transcript was: {written:?}");
    }
}
