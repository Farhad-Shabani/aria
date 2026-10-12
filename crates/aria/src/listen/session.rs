//! The listen session: a thread carrying system audio through streaming
//! transcription into the transcript file, with the voice track beside it.

use std::ops::Range;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::audio::canonical_duration;
use aria_kit::cancel::CancelToken;
use aria_stt::moonshine::{Moonshine, MoonshineConfig};
use aria_stt::nemotron::{Nemotron, NemotronConfig};
use aria_stt::parakeet::{Parakeet, ParakeetConfig};

use crate::audio::RecordingReader;
use crate::config::{ListenConfig, ListenTranscription, SpeakerConfig};
use crate::transcription::{
    Language, Streaming, StreamingEngine, Transcriber, Transcription, TranscriptionError,
};

use super::event::{ListenEventEmitter, ListenStage};
use super::{ListenError, SystemAudioCapture, TranscriptFile};
use aria_kit::speaker::Embedder;

use crate::dictation::{TranscriptSink, Utterance};
use crate::speaker::{self, SegmentReader, Verdict};

use super::track::VoiceThread;

/// Loads the engine and transcribes until capture ends, on its own thread —
/// Start answers before the model finishes loading.
pub(crate) fn spawn_session(
    config: ListenConfig,
    speaker_config: SpeakerConfig,
    events: ListenEventEmitter,
    capture: SystemAudioCapture,
    recording: RecordingReader,
    sink: TranscriptFile,
    cancel: CancelToken,
) -> std::io::Result<JoinHandle<Result<(), ListenError>>> {
    thread::Builder::new()
        .name("aria-listen".into())
        .spawn(move || {
            let result = match config.transcription.clone() {
                ListenTranscription::Parakeet {
                    model_directory,
                    device,
                } => run_with::<Parakeet>(
                    ParakeetConfig::new(model_directory, device),
                    &speaker_config,
                    &events,
                    capture,
                    recording,
                    sink,
                    &cancel,
                ),
                ListenTranscription::Moonshine { model_directory } => run_with::<Moonshine>(
                    MoonshineConfig::new(model_directory),
                    &speaker_config,
                    &events,
                    capture,
                    recording,
                    sink,
                    &cancel,
                ),
                ListenTranscription::Nemotron { model_directory } => run_with::<Nemotron>(
                    NemotronConfig::new(model_directory),
                    &speaker_config,
                    &events,
                    capture,
                    recording,
                    sink,
                    &cancel,
                ),
            };
            match &result {
                Ok(()) => events.emit(ListenStage::Stopped),
                Err(error) => events.emit(ListenStage::Failed(error.code())),
            }
            result
        })
}

fn run_with<E: StreamingEngine>(
    config: E::Config,
    speaker_config: &SpeakerConfig,
    events: &ListenEventEmitter,
    capture: SystemAudioCapture,
    recording: RecordingReader,
    mut sink: TranscriptFile,
    cancel: &CancelToken,
) -> Result<(), ListenError> {
    // Forked before the model loads, because loading is seconds of audio the
    // track would otherwise never hear.
    let voices_audio = recording.fork();
    let result = E::load_from(config, cancel)
        .map_err(ListenError::Transcription)
        .and_then(|engine| {
            let track = voice_thread(speaker_config, voices_audio);
            let mut reader = speaker::reader(speaker_config);
            transcribe_all(
                recording,
                &Transcription::streaming(engine),
                events,
                &mut reader,
                &mut sink,
                track,
                || capture.stopper().stop(),
            )
        });
    if result.is_err() {
        capture.stopper().stop();
    }
    result.and(capture.finish())
}

/// The track on its own thread, or none.
///
/// Two embedders: one for the live per-segment guess, one for the track's
/// masked windows. They cluster different populations and must not share
/// running state, and a second graph is cheaper than teaching one to hold
/// both. A thread that will not start is an environment failure, so it
/// degrades to the live guess the way a missing model does.
fn voice_thread(config: &SpeakerConfig, audio: RecordingReader) -> Option<VoiceThread> {
    let diarizer = speaker::diarizer(config)?;
    let track = speaker::voice_track(
        &config.segmentation_model,
        diarizer,
        speaker::Grouping::default(),
    )?;
    VoiceThread::spawn(track, audio)
        .inspect_err(|error| {
            tracing::warn!(%error, "the voice track could not start; speakers stay as guessed");
        })
        .ok()
}

fn transcribe_all<T: Transcriber, E: Embedder>(
    mut recording: RecordingReader,
    transcription: &Transcription<T, Streaming>,
    events: &ListenEventEmitter,
    reader: &mut SegmentReader<E>,
    sink: &mut TranscriptFile,
    mut track: Option<VoiceThread>,
    // Ends capture. The track's thread is woken by capture's own end, so a
    // loop that stopped for any other reason has to end capture before the
    // join — otherwise the join waits for audio nobody will write.
    end_capture: impl FnOnce(),
) -> Result<(), ListenError> {
    let mut operation = match transcription.start(&Language::Automatic) {
        Ok(operation) => operation,
        // Nothing was transcribed, but the track is already walking. End
        // capture first: its own read is what the join waits on.
        Err(error) => {
            end_capture();
            if let Some(track) = &mut track {
                track.finish();
            }
            return Err(ListenError::Transcription(error));
        }
    };
    // Whatever ends the loop, the paragraphs already on disk carry live
    // guesses. Settling them is the last thing this session owes the reader,
    // so a failure must not skip it.
    let heard_all = transcribe_into(
        &mut recording,
        &mut operation,
        events,
        reader,
        sink,
        track.as_ref(),
    );
    if heard_all.is_err() {
        end_capture();
    }
    if let Some(track) = &mut track {
        // The loop ends on the last segment; the track may still be short of
        // it. The track's drain limit bounds this wait.
        let merged = track.finish();
        emit_merges(events, &merged);
    }
    let settled = match &track {
        // The track read the audio itself, on its own clock, so its labels
        // replace the live guesses. Paragraphs past its reach lose their
        // live label.
        Some(track) => sink.rewrite(|spans| attribute_reached(track, spans)),
        None if reader.labeler().diarizes() => {
            sink.finish(|paragraphs| reader.labeler().relabel(paragraphs))
        }
        None => Ok(()),
    };
    heard_all?;
    settled.map_err(ListenError::Transcript)
}

fn attribute_reached(track: &VoiceThread, spans: &[Option<Range<u64>>]) -> Vec<Option<String>> {
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
fn emit_merges(events: &ListenEventEmitter, merged: &[crate::speaker::Merged]) {
    for merge in merged {
        events.emit(ListenStage::SpeakerMerged {
            from: Arc::from(merge.from.as_str()),
            into: Arc::from(merge.into.as_str()),
        });
    }
}

/// The loop itself: segments in, paragraphs out, until capture ends.
fn transcribe_into<T: Transcriber, E: Embedder>(
    recording: &mut RecordingReader,
    operation: &mut crate::transcription::Operation<'_, T>,
    events: &ListenEventEmitter,
    reader: &mut SegmentReader<E>,
    sink: &mut TranscriptFile,
    track: Option<&VoiceThread>,
) -> Result<(), ListenError> {
    loop {
        let heard = match reader.next(operation, recording) {
            Ok(Some(heard)) => heard,
            // A capture error is the recording ending abnormally; capture's
            // result in finish() names the real cause.
            Ok(None) | Err(TranscriptionError::Capture(_)) => break,
            Err(error) => return Err(ListenError::Transcription(error)),
        };
        if let Verdict::Keep { label, embedding } = &heard.verdict {
            events.emit(ListenStage::SegmentTranscribed {
                text: Arc::from(heard.segment.as_str()),
                label: label.as_deref().map(Arc::from),
                at_seconds: canonical_duration(heard.at).as_secs(),
            });
            sink.segment(Utterance {
                text: heard.segment.as_str(),
                speaker: label.as_deref(),
                embedding: embedding.as_deref(),
                at: heard.at,
                span: heard.span.clone(),
            })
            .map_err(ListenError::Transcript)?;
        }
        // The track walks on its own thread. Only what it decided has to
        // come back here, where the file and the emitter live.
        if let Some(track) = track {
            let merged = track.merged();
            emit_merges(events, &merged);
            // Only a merge changes a name already written. The flush it
            // forces can split a paragraph, which the next rewrite's
            // coalescing joins back up.
            if !merged.is_empty() {
                sink.rewrite(|spans| attribute_reached(track, spans))
                    .map_err(ListenError::Transcript)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::transcription::{Cadence, Hypothesis, TranscriptionAudio};

    use super::*;
    use crate::speaker::SegmentLabeler;

    struct Answering(&'static str);

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
    }

    #[test]
    fn only_paragraphs_inside_the_reach_are_offered_to_the_track() {
        let spans = [Some(0..10), None, Some(10..20), Some(15..30), None];
        assert_eq!(reached(&spans, 20), 3);
        assert_eq!(reached(&spans, 5), 0);
    }

    #[test]
    fn a_session_carries_the_tap_recording_into_the_transcript_file() {
        let directory = tempfile::tempdir().expect("test directory is created");
        let (mut sink, path) =
            TranscriptFile::create(directory.path()).expect("the transcript file is created");
        let recording = RecordingReader::from_samples(&vec![0.01_f32; 32_000]);
        let transcription =
            Transcription::agreement(Cadence::from_millis(100, 20_000), Answering("hello"));

        transcribe_all(
            recording,
            &transcription,
            &ListenEventEmitter::unsubscribed(),
            &mut SegmentReader::new(SegmentLabeler::new(
                None,
                None::<crate::speaker::Diarizer<aria_speaker::wespeaker::WeSpeaker>>,
            )),
            &mut sink,
            None,
            || {},
        )
        .expect("the session should complete");
        drop(sink);

        let written = std::fs::read_to_string(path).expect("the transcript file is readable");
        assert!(written.contains("hello"), "transcript was: {written:?}");
    }
}
