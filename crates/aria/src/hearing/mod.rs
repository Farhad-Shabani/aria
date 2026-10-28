//! Hears one recording. The decoder runs on one thread; the speech detector
//! and the tracker run on another, ahead of it. The gate decides on the
//! caller's thread whenever either moves, so a word is released as soon as
//! its turn is judged rather than at the decoder's next step.

mod board;
mod decoder;
mod turns;

use std::borrow::Borrow;
use std::collections::VecDeque;
use std::sync::Arc;
use std::thread::Scope;

use aria_kit::audio::{RecordingReader, canonical_samples};
use aria_kit::backend::BackendError;
use aria_kit::cancel::CancelToken;
use aria_kit::speaker::{Admission, Gate, Released, SpeakerTracker, Timeline, TurnFeed};
use aria_kit::transcription::HeardUpdate;
use aria_kit::voice_activity::{SpeechMap, VoiceActivityDetector};

use crate::transcription::{Operation, Transcriber, Transcript, TranscriptionError};
pub(crate) use board::Board;
use board::Wake;
use decoder::DecodeThread;
use turns::TurnThread;
pub(crate) use turns::{Snapshot, TrackingError};

/// Names the hearing as the backend when its own decoder thread fails.
const BACKEND: &str = "dictation";

#[derive(Debug, thiserror::Error)]
pub(crate) enum HearingError {
    #[error(transparent)]
    Transcription(TranscriptionError),
    #[error(transparent)]
    Tracking(TrackingError),
}

pub(crate) enum Event {
    /// The walk judged more of the recording. Only a hearing with a
    /// tracker has one.
    Timeline(Arc<Timeline>),
    Released(Released),
    /// The provisional text to show, empty to hide it.
    Preview(String),
}

pub(crate) struct Ended {
    pub(crate) transcript: Transcript,
    /// What hearing still owed once it ended: the walk's last timeline,
    /// which may land after the decoder's last update, then what the gate
    /// let out.
    pub(crate) events: Vec<Event>,
    /// Every word typed was the owner's, judged on a whole recording.
    pub(crate) own_voice: bool,
}

pub(crate) struct Decoding<'scope, T> {
    pub(crate) operation: Operation<'scope, T>,
    pub(crate) recording: &'scope mut RecordingReader,
    pub(crate) previews: bool,
}

/// Hears the whole of the recording `feed` reads on the turn thread while
/// capture runs, with no decoder, for a press decoded only once it ends.
/// Only the owner's words pass, so it returns the timeline to judge them by.
/// `heard` sees each timeline as the walk publishes it.
pub(crate) fn hear_to_end<V, K>(
    feed: TurnFeed<V, K>,
    cancel: &CancelToken,
    mut heard: impl FnMut(&Timeline),
) -> Result<Arc<Timeline>, HearingError>
where
    V: VoiceActivityDetector + Send,
    K: SpeakerTracker + Send,
{
    std::thread::scope(|scope| {
        let board = Board::new();
        let walk = TurnThread::spawn(scope, feed, cancel, &board)
            .map_err(|error| HearingError::Tracking(owner_walk_failed(error)))?;
        // The walk ends with capture. A tracker that fails once someone
        // spoke ends the press early; before that, it may still be NoSpeech.
        loop {
            let snapshot = board.next_snapshot();
            if let Some(timeline) = &snapshot.timeline {
                heard(timeline);
            }
            if snapshot.tracking_failed() && held_speech(&snapshot.speech) {
                return Err(HearingError::Tracking(TrackingError::Failed));
            }
            if board.walk_ending().is_some() {
                break;
            }
        }
        let snapshot = walk.finish().map_err(HearingError::Tracking)?;
        let timeline = verdict(
            Some(&snapshot.speech),
            snapshot.timeline,
            Admission::OwnerOnly,
        )?;
        Ok(timeline.expect("an owner-only verdict keeps its timeline"))
    })
}

/// Hears a saved recording on the caller's thread, start to end. Returns
/// the timeline to judge the owner's words by, under owner-only admission.
pub(crate) fn hear_saved<V: VoiceActivityDetector, K: SpeakerTracker>(
    mut feed: TurnFeed<V, K>,
    admission: Admission,
    cancel: &CancelToken,
) -> Result<Option<Timeline>, HearingError> {
    let stopped = || {
        cancel
            .is_cancelled()
            .then_some(HearingError::Tracking(TrackingError::Cancelled))
    };
    // Paced in pieces, so a long recording is never scored in one step
    // between two cancel checks.
    let piece = canonical_samples(turns::PIECE);
    let mut through = feed.held_from();
    loop {
        if let Some(error) = stopped() {
            return Err(error);
        }
        through += piece;
        if !feed.hear_through(through) {
            break;
        }
    }
    if let Some(error) = stopped() {
        return Err(error);
    }
    feed.hear_rest();
    Ok(verdict(Some(feed.speech()), feed.timeline(), admission)?.cloned())
}

/// How a hearing ends once all of it is heard. A recording without speech
/// is `NoSpeech`, whatever became of its voices. Then, under owner-only
/// admission, a missing or broken timeline fails it: none of the owner's
/// words can be judged. `speech` is `None` when no detector ran, which
/// counts as speech. The timeline comes back only under owner-only
/// admission.
fn verdict<T: Borrow<Timeline>>(
    speech: Option<&SpeechMap>,
    timeline: Option<T>,
    admission: Admission,
) -> Result<Option<T>, HearingError> {
    if speech.is_some_and(|speech| !held_speech(speech)) {
        return Err(HearingError::Transcription(TranscriptionError::NoSpeech));
    }
    match admission {
        Admission::Everyone => Ok(None),
        Admission::OwnerOnly => match timeline {
            Some(timeline) if !timeline.borrow().is_broken() => Ok(Some(timeline)),
            _ => Err(HearingError::Tracking(TrackingError::Failed)),
        },
    }
}

/// Without its thread nothing can be judged, so none of the owner's words
/// pass.
fn owner_walk_failed(error: std::io::Error) -> TrackingError {
    tracing::error!(%error, "the turn thread did not start");
    TrackingError::Spawn(error)
}

/// Whether `speech` holds speech before `at`. A decoder can name words in
/// silence that look like real ones, so the audio decides. A map that could
/// not be judged counts as speech: losing words is worse than letting words
/// decoded from silence through.
fn held_speech_before(speech: &SpeechMap, at: u64) -> bool {
    if speech.is_broken() {
        tracing::warn!("the recording could not be judged for speech; its words pass");
        return true;
    }
    speech.speech_before(at) > 0
}

fn held_speech(speech: &SpeechMap) -> bool {
    if held_speech_before(speech, speech.reached()) {
        return true;
    }
    tracing::info!("no speech detected in the recording; its words are dropped");
    false
}

pub(crate) struct Hearing<'scope> {
    board: Arc<Board>,
    decoder: DecodeThread<'scope>,
    turns: Option<TurnThread<'scope>>,
    gate: Gate,
    events: VecDeque<Event>,
    /// Whether speech was heard by where the decoder has read. Words
    /// finalized before it were decoded from silence.
    spoken: bool,
    unspoken: usize,
    trusted: Vec<String>,
    /// The decoder has sent its last update.
    done: bool,
}

impl<'scope> Hearing<'scope> {
    /// Starts both threads on `board`. `admission` decides which words the
    /// gate releases. `cancel` is the board's token the decoder's operation
    /// started with.
    pub(crate) fn start<'env, T, V, K>(
        scope: &'scope Scope<'scope, 'env>,
        decoding: Decoding<'scope, T>,
        feed: Option<TurnFeed<V, K>>,
        admission: Admission,
        board: &Arc<Board>,
        cancel: &CancelToken,
    ) -> Result<Self, HearingError>
    where
        T: Transcriber,
        V: VoiceActivityDetector + Send + 'scope,
        K: SpeakerTracker + Send + 'scope,
    {
        let from = feed
            .as_ref()
            .map_or(decoding.recording.held_from(), TurnFeed::held_from);
        let turns = match feed {
            Some(feed) => {
                match TurnThread::spawn(scope, feed, cancel, board) {
                    Ok(turns) => Some(turns),
                    Err(error) if admission == Admission::OwnerOnly => {
                        return Err(HearingError::Tracking(owner_walk_failed(error)));
                    }
                    // The speech check fails open: every word still goes out.
                    Err(error) => {
                        tracing::warn!(%error, "the turn thread did not start; speech goes unchecked");
                        None
                    }
                }
            }
            None => None,
        };
        let decoder = DecodeThread::spawn(
            scope,
            decoding.operation,
            decoding.recording,
            decoding.previews,
            Arc::clone(board),
        )
        .map_err(|error| {
            tracing::error!(%error, "the decoder thread did not start");
            HearingError::Transcription(TranscriptionError::Inference {
                backend: BACKEND,
                source: BackendError::engine(error),
            })
        })?;
        Ok(Self {
            board: Arc::clone(board),
            decoder,
            spoken: false,
            turns,
            gate: Gate::new(admission, from),
            events: VecDeque::new(),
            unspoken: 0,
            trusted: Vec::new(),
            done: false,
        })
    }

    /// The next thing to act on, waiting until something happens. `None`
    /// once the decoder is done: what is still held comes with [`Self::finish`].
    pub(crate) fn next(&mut self) -> Result<Option<Event>, HearingError> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            if self.done {
                return Ok(None);
            }
            let wake = self.board.next_wake();
            let heard = matches!(wake, Wake::Heard);
            match wake {
                Wake::Decoded(decoded) => self.take(decoded.update, decoded.read_to)?,
                Wake::Heard => {}
                Wake::DecoderDone => self.done = true,
            }
            let snapshot = self.turns.is_some().then(|| self.board.snapshot());
            // Queued ahead of what the gate releases, so whose voice it is
            // shows before their words do.
            if heard
                && let Some(timeline) = snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.timeline.clone())
            {
                self.events.push_back(Event::Timeline(timeline));
            }
            if self.gate.admission() == Admission::OwnerOnly
                && snapshot.as_ref().is_some_and(Snapshot::tracking_failed)
            {
                return Err(HearingError::Tracking(TrackingError::Failed));
            }
            self.decide(snapshot.as_ref());
        }
    }

    /// Waits for both threads, and decides what is still held.
    pub(crate) fn finish(self) -> Result<Ended, HearingError> {
        let Self {
            decoder,
            turns,
            mut gate,
            unspoken,
            trusted,
            ..
        } = self;
        let transcript = decoder
            .finish()
            .unwrap_or_else(|| {
                Err(TranscriptionError::Inference {
                    backend: BACKEND,
                    source: BackendError::Engine("the decoder panicked".into()),
                })
            })
            .map_err(HearingError::Transcription)?;
        let admission = gate.admission();
        let snapshot = match turns {
            Some(turns) if admission == Admission::OwnerOnly => Some(turns.finish()),
            Some(turns) => Some(turns.finish_speech()),
            None => None,
        }
        .transpose()
        .map_err(HearingError::Tracking)?;
        let last = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.timeline.clone());
        let timeline = verdict(
            snapshot.as_ref().map(|snapshot| &*snapshot.speech),
            last.as_deref(),
            admission,
        )?;
        let (released, own_voice) = match timeline {
            Some(timeline) => (gate.decide(timeline).released, timeline.admitted_is_owner()),
            None => (Vec::new(), false),
        };
        let transcript = if unspoken > 0 {
            tracing::debug!(
                segments = unspoken,
                "segments decoded before speech was heard dropped"
            );
            Transcript::new(trusted.join(" ")).map_err(HearingError::Transcription)?
        } else {
            transcript
        };
        let events = last
            .map(Event::Timeline)
            .into_iter()
            .chain(released.into_iter().map(Event::Released))
            .collect();
        Ok(Ended {
            transcript,
            events,
            own_voice,
        })
    }

    fn take(&mut self, update: HeardUpdate, read_to: u64) -> Result<(), HearingError> {
        // The owner's words are decided against turns heard at least as far
        // as the decoder read, so provisional text is never judged on stale turns.
        // Names never wait.
        if self.gate.admission() == Admission::OwnerOnly
            && let Some(turns) = &self.turns
        {
            turns
                .heard_through(read_to)
                .map_err(HearingError::Tracking)?;
        }
        if !self.spoken {
            self.spoken = self.spoken_by(read_to)?;
        }
        match update {
            HeardUpdate::Finalized(_) if !self.spoken => self.unspoken += 1,
            HeardUpdate::Provisional { .. } if !self.spoken => {}
            HeardUpdate::Finalized(heard) => {
                // Read only to rebuild a transcript that started with words decoded from silence.
                if self.unspoken > 0 {
                    self.trusted.push(heard.segment.as_str().to_owned());
                }
                self.gate.hold(heard);
            }
            HeardUpdate::Provisional {
                text,
                source_window,
            } => self.gate.propose(text, source_window, read_to),
        }
        Ok(())
    }

    /// Whether the recording held speech by `read_to`. Without a detector
    /// every recording counts as speech.
    fn spoken_by(&self, read_to: u64) -> Result<bool, HearingError> {
        let Some(turns) = &self.turns else {
            return Ok(true);
        };
        let speech = turns
            .speech_through(read_to)
            .map_err(HearingError::Tracking)?;
        Ok(held_speech_before(&speech, read_to))
    }

    /// Without a tracker, an empty timeline names nobody.
    fn decide(&mut self, snapshot: Option<&Snapshot>) {
        let unheard = Timeline::default();
        let timeline = snapshot
            .and_then(|snapshot| snapshot.timeline.as_deref())
            .unwrap_or(&unheard);
        let decision = self.gate.decide(timeline);
        if let Some(preview) = decision.preview {
            self.events.push_back(Event::Preview(preview));
        }
        self.events
            .extend(decision.released.into_iter().map(Event::Released));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    use aria_kit::audio::Recording;
    use aria_kit::speaker::VoiceprintTracker;
    use aria_kit::transcription::{
        OperationSource, OperationStep, TranscriptSegment, TranscriptUpdate,
    };
    use aria_kit::voice_activity::SpeechDetector;

    use super::*;
    use crate::dictation::testing::{Loud, Operations};
    use crate::speaker::testing::{OWN, Voice, diarizer, tracker};
    use crate::transcription::{Language, Streaming, Transcription};

    const SECOND: u64 = 16_000;

    /// Finalizes "hello" at once, then stalls until `go_on` says to.
    fn stalling(go_on: Receiver<()>) -> impl Transcriber {
        let go_on = Mutex::new(Some(go_on));
        Operations(move |_: &CancelToken| {
            let go_on = go_on.lock().unwrap().take().expect("one operation");
            let mut step = 0;
            OperationSource::new("stalling", move |_, _| {
                step += 1;
                if step == 1 {
                    // Read up to the word's end: its turn is not stable there.
                    let word = SECOND..3 * SECOND / 2;
                    let hello = TranscriptSegment::new("hello", Some(word.clone()))?
                        .with_word_spans(vec![word.clone()])?;
                    return Ok(OperationStep::Update {
                        update: TranscriptUpdate::Finalized(hello),
                        retained_from: 0,
                        read_to: word.end,
                        completed: None,
                    });
                }
                let _ = go_on.recv();
                Ok(OperationStep::Complete {
                    transcript: Transcript::new("hello")?,
                    retained_from: 0,
                    read_to: 3 * SECOND,
                })
            })
        })
    }

    #[test]
    fn an_owner_word_is_released_while_the_decoder_stalls() {
        let (go_on, waiting) = channel();
        let transcription = Transcription::<_, Streaming>::delegated(stalling(waiting));
        let (mut recording, mut writer) = Recording::open();
        writer.push(&[0.5; 3 * SECOND as usize]);
        let speech = SpeechDetector::new(Loud, 0);
        let mut diarizer = diarizer(Voice(OWN));
        let board = Board::new();
        let cancel = board.token(|| false);
        let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));
        let operation = transcription
            .start_with(&Language::Automatic, &cancel)
            .unwrap();

        std::thread::scope(|scope| {
            let started = Instant::now();
            let mut hearing = Hearing::start(
                scope,
                Decoding {
                    operation,
                    recording: &mut recording,
                    previews: false,
                },
                Some(feed),
                Admission::OwnerOnly,
                &board,
                &cancel,
            )
            .unwrap();
            let unblock = scope.spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                let _ = go_on.send(());
            });

            let mut ring_first = false;
            let released = loop {
                match hearing.next().unwrap() {
                    Some(Event::Timeline(_)) => ring_first = true,
                    Some(Event::Released(released)) => break released,
                    Some(Event::Preview(_)) | None => panic!("the word is released"),
                }
            };
            assert!(ring_first, "whose voice it is shows before the words");
            assert_eq!(released.heard.segment.as_str(), "hello");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "released before the decoder's next step"
            );

            writer.finish();
            while hearing.next().unwrap().is_some() {}
            let ended = hearing.finish().unwrap();
            assert!(ended.own_voice);
            unblock.join().unwrap();
        });
    }

    #[test]
    fn a_broken_decoder_ends_the_hearing_while_capture_runs() {
        for panics in [true, false] {
            // Fails at its first step, by panicking or by an error.
            let breaking = Operations(move |_: &CancelToken| {
                OperationSource::new("breaking", move |_, _| {
                    assert!(!panics, "the decoder broke");
                    Err(TranscriptionError::Inference {
                        backend: "breaking",
                        source: aria_kit::backend::BackendError::Unusable(
                            "the decoder broke".into(),
                        ),
                    })
                })
            });
            let transcription = Transcription::<_, Streaming>::delegated(breaking);
            let (mut recording, mut writer) = Recording::open();
            writer.push(&[0.5; SECOND as usize]);
            let speech = SpeechDetector::new(Loud, 0);
            let feed =
                TurnFeed::start(&mut recording, speech, None::<VoiceprintTracker<'_, Voice>>);
            let board = Board::new();
            let cancel = board.token(|| false);
            let operation = transcription
                .start_with(&Language::Automatic, &cancel)
                .unwrap();

            std::thread::scope(|scope| {
                let started = Instant::now();
                let mut hearing = Hearing::start(
                    scope,
                    Decoding {
                        operation,
                        recording: &mut recording,
                        previews: false,
                    },
                    Some(feed),
                    Admission::Everyone,
                    &board,
                    &cancel,
                )
                .unwrap();
                let capture = scope.spawn(move || {
                    std::thread::sleep(Duration::from_secs(3));
                    writer.finish();
                });

                while hearing.next().unwrap().is_some() {}
                let ended = hearing.finish();
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "the hearing waited for capture to end, panics={panics}"
                );
                if panics {
                    assert!(matches!(
                        ended,
                        Err(HearingError::Transcription(TranscriptionError::Inference {
                            backend: BACKEND,
                            ..
                        }))
                    ));
                } else {
                    assert!(matches!(
                        ended,
                        Err(HearingError::Transcription(
                            TranscriptionError::Inference { .. }
                        ))
                    ));
                }
                capture.join().unwrap();
            });
        }
    }

    #[test]
    fn hearing_to_the_end_ends_when_cancelled_while_capture_runs() {
        let (mut recording, mut writer) = Recording::open();
        writer.push(&[0.5; SECOND as usize]);
        let speech = SpeechDetector::new(Loud, 0);
        let mut diarizer = diarizer(Voice(OWN));
        let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));
        let cancel = CancelToken::new();

        let started = Instant::now();
        let heard = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                cancel.cancel();
            });
            hear_to_end(feed, &cancel, |_: &Timeline| {})
        });

        assert!(matches!(
            heard,
            Err(HearingError::Tracking(TrackingError::Cancelled))
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer);
    }

    #[test]
    fn a_broken_timeline_fails_a_press_only_when_someone_spoke() {
        // The speech detector hears NaN as silence; the tracker cannot keep
        // it as evidence, so its timeline breaks before anyone speaks.
        let silent = vec![f32::NAN; SECOND as usize];
        let spoken_later = [silent.clone(), vec![0.5; SECOND as usize]].concat();
        for (audio, spoke) in [(silent, false), (spoken_later, true)] {
            for saved in [true, false] {
                let mut recording = RecordingReader::from_samples(&audio);
                let speech = SpeechDetector::new(Loud, 0);
                let mut diarizer = diarizer(Voice(OWN));
                let feed = TurnFeed::start(&mut recording, speech, Some(tracker(&mut diarizer, 0)));
                let cancel = CancelToken::never();

                let heard = if saved {
                    hear_saved(feed, Admission::OwnerOnly, &cancel).map(drop)
                } else {
                    hear_to_end(feed, &cancel, |_: &Timeline| {}).map(drop)
                };

                if spoke {
                    assert!(
                        matches!(heard, Err(HearingError::Tracking(TrackingError::Failed))),
                        "saved={saved}"
                    );
                } else {
                    assert!(
                        matches!(
                            heard,
                            Err(HearingError::Transcription(TranscriptionError::NoSpeech))
                        ),
                        "saved={saved}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_map_that_could_not_be_judged_counts_as_speech() {
        let mut detector = SpeechDetector::new(Loud, 0);
        detector.hear(&[0.0; 16_000]).unwrap();
        let mut speech = detector.map().clone();
        speech.break_off();

        assert!(held_speech(&speech));
    }

    #[test]
    fn cancelling_recorded_filtering_stops_further_checks() {
        struct CancelAfterEmbedding {
            cancel: CancelToken,
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        impl aria_kit::speaker::Embedder for CancelAfterEmbedding {
            fn embed(
                &mut self,
                _audio: &[f32],
            ) -> Result<Option<Vec<f32>>, aria_kit::speaker::SpeakerError> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.cancel.cancel();
                Ok(Some(OWN.to_vec()))
            }

            fn dimension(&self) -> usize {
                OWN.len()
            }
        }

        let cancel = CancelToken::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut diarizer = diarizer(CancelAfterEmbedding {
            cancel: cancel.clone(),
            calls: Arc::clone(&calls),
        });
        let mut recording = RecordingReader::from_samples(&vec![0.5; 5 * 16_000]);
        let feed = TurnFeed::start(
            &mut recording,
            SpeechDetector::new(Loud, 0),
            Some(tracker(&mut diarizer, 0)),
        );
        let result = hear_saved(feed, Admission::OwnerOnly, &cancel);
        assert!(matches!(
            result,
            Err(HearingError::Tracking(TrackingError::Cancelled))
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
