//! Hears one recording. The decoder runs on one thread; the speech detector
//! and the tracker run on another, ahead of it. The gate decides on the
//! caller's thread whenever either moves, so a word is released as soon as
//! its turn is judged rather than at the decoder's next step.

mod decoder;
mod speech;
mod turns;

use std::borrow::Borrow;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::Scope;
use std::time::Duration;

use aria_kit::audio::{RecordingReader, canonical_samples};
use aria_kit::cancel::CancelToken;
use aria_kit::speaker::{Admission, Gate, Released, SpeakerTracker, Timeline, TurnFeed};
use aria_kit::transcription::HeardUpdate;
use aria_kit::voice_activity::{SpeechMap, VoiceActivityDetector};

use crate::transcription::{Operation, Transcriber, Transcript, TranscriptionError};
use decoder::DecodeThread;

use speech::held_speech;
use turns::TurnThread;
pub(crate) use turns::{Snapshot, TrackingError};

/// Updates in flight before the decoder waits for the caller's thread.
const WAKE_CAPACITY: usize = 64;

/// How often hearing without a decoder checks cancellation and the timeline.
const POLL: Duration = Duration::from_millis(100);

enum Wake {
    Decoded {
        update: HeardUpdate,
        read_to: u64,
    },
    DecoderDone,
    /// The turn thread published a snapshot.
    Heard,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HearingError {
    #[error(transparent)]
    Decoder(TranscriptionError),
    #[error("the decoder panicked")]
    DecoderPanicked,
    #[error("could not start the decoder")]
    DecoderSpawn(#[source] std::io::Error),
    #[error(transparent)]
    Tracking(TrackingError),
}

impl HearingError {
    /// Decoder errors pass through. A decoder that panicked or never
    /// started becomes an inference failure of `backend`.
    pub(crate) fn into_transcription(self, backend: &'static str) -> TranscriptionError {
        match self {
            Self::Decoder(error) => error,
            error => TranscriptionError::Inference {
                backend,
                source: aria_kit::backend::BackendError::engine(error),
            },
        }
    }
}

pub(crate) enum Event {
    Released(Released),
    /// The provisional text to show, empty to hide it.
    Preview(String),
}

pub(crate) struct Ended {
    pub(crate) transcript: Transcript,
    /// What the gate let out once hearing ended.
    pub(crate) released: Vec<Released>,
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
pub(crate) fn hear_to_end<V, K>(
    feed: TurnFeed<'_, V, K>,
    recording: &RecordingReader,
    cancel: &CancelToken,
    progress: impl FnMut(&Snapshot) + Send,
) -> Result<Arc<Timeline>, HearingError>
where
    V: VoiceActivityDetector + Send,
    K: SpeakerTracker + Send,
{
    std::thread::scope(|scope| {
        let walk = TurnThread::spawn(scope, feed, cancel, progress, || {})
            .map_err(|error| HearingError::Tracking(owner_walk_failed(error)))?;
        while !recording.wait_for_end(POLL) {
            if cancel.is_cancelled() {
                return Err(HearingError::Tracking(TrackingError::Cancelled));
            }
            if walk.snapshot().tracking_failed() {
                return Err(HearingError::Tracking(TrackingError::Failed));
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
    mut feed: TurnFeed<'_, V, K>,
    cancel: &CancelToken,
) -> Result<Option<Timeline>, HearingError> {
    let stopped = || {
        cancel
            .is_cancelled()
            .then_some(HearingError::Tracking(TrackingError::Cancelled))
    };
    let piece = canonical_samples(turns::PIECE);
    while feed.spoken_to() < feed.written() {
        if let Some(error) = stopped() {
            return Err(error);
        }
        let through = feed.written().min(feed.spoken_to() + piece);
        feed.hear_through(through);
    }
    if let Some(error) = stopped() {
        return Err(error);
    }
    feed.hear_rest();
    let timeline = feed.voices().map(SpeakerTracker::timeline);
    Ok(verdict(Some(feed.speech()), timeline, feed.admission())?.cloned())
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
        return Err(HearingError::Decoder(TranscriptionError::NoSpeech));
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

pub(crate) struct Hearing<'scope> {
    // Dropped first: a decoder blocked on a full queue sees the receiver
    // gone and exits, so joining it cannot hang.
    woken: Receiver<Wake>,
    decoder: Option<DecodeThread<'scope>>,
    turns: Option<TurnThread<'scope>>,
    gate: Gate,
    abandon: Abandon,
    /// A turn-thread wake waits in the queue.
    pending: Arc<AtomicBool>,
    events: VecDeque<Event>,
    /// Whether speech was heard by where the decoder has read. Words
    /// finalized before it were decoded from silence.
    spoken: bool,
    unspoken: usize,
    trusted: Vec<String>,
    /// The decoder has sent its last update.
    done: bool,
}

/// Stops a hearing's decoder when the hearing is dropped unfinished.
pub(crate) struct Abandon {
    abandoned: Arc<AtomicBool>,
    token: CancelToken,
}

impl Abandon {
    /// The token stops once the hearing is abandoned, or whenever `stop`
    /// says so.
    pub(crate) fn linked(stop: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        let abandoned = Arc::new(AtomicBool::new(false));
        let token = {
            let abandoned = Arc::clone(&abandoned);
            CancelToken::from_fn(move || stop() || abandoned.load(Ordering::Relaxed))
        };
        Self { abandoned, token }
    }

    /// The token to start the decoder's operation with.
    pub(crate) fn token(&self) -> &CancelToken {
        &self.token
    }
}

impl<'scope> Hearing<'scope> {
    /// Starts both threads. `abandon`'s token started the decoder's
    /// operation. `progress` sees every snapshot the turn thread publishes.
    pub(crate) fn start<'env, T, V, K>(
        scope: &'scope Scope<'scope, 'env>,
        decoding: Decoding<'scope, T>,
        feed: Option<TurnFeed<'scope, V, K>>,
        abandon: Abandon,
        progress: impl FnMut(&Snapshot) + Send + 'scope,
    ) -> Result<Self, HearingError>
    where
        T: Transcriber,
        V: VoiceActivityDetector + Send,
        K: SpeakerTracker + Send,
    {
        let (wake, woken) = sync_channel(WAKE_CAPACITY);
        let pending = Arc::new(AtomicBool::new(false));
        let admission = feed
            .as_ref()
            .map_or(Admission::Everyone, TurnFeed::admission);
        let from = feed
            .as_ref()
            .map_or(decoding.recording.held_from(), TurnFeed::held_from);
        let turns = match feed {
            Some(feed) => {
                let heard = wake.clone();
                let pending = Arc::clone(&pending);
                let spawned =
                    TurnThread::spawn(scope, feed, abandon.token(), progress, move || {
                        wake_for(&heard, &pending);
                    });
                match spawned {
                    Ok(turns) => Some(turns),
                    Err(error) if admission == Admission::OwnerOnly => {
                        return Err(HearingError::Tracking(owner_walk_failed(error)));
                    }
                    // Naming is a courtesy, and the speech check fails open:
                    // words whose voices cannot be heard still go out.
                    Err(error) => {
                        tracing::warn!(%error, "the turn thread did not start; voices go unnamed");
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
            wake,
        )
        .map_err(HearingError::DecoderSpawn)?;
        Ok(Self {
            decoder: Some(decoder),
            spoken: false,
            turns,
            woken,
            gate: Gate::new(admission, from),
            abandon,
            pending,
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
            match self.woken.recv().unwrap_or(Wake::DecoderDone) {
                Wake::Decoded { update, read_to } => self.take(update, read_to)?,
                // Cleared before the snapshot is read, so a later publish
                // queues a fresh wake.
                Wake::Heard => self.pending.store(false, Ordering::Release),
                Wake::DecoderDone => self.done = true,
            }
            let snapshot = self.turns.as_ref().map(TurnThread::snapshot);
            if self.gate.admission() == Admission::OwnerOnly
                && snapshot.as_ref().is_some_and(Snapshot::tracking_failed)
            {
                return Err(HearingError::Tracking(TrackingError::Failed));
            }
            self.decide(snapshot.as_ref());
        }
    }

    /// Waits for both threads, and decides what is still held.
    pub(crate) fn finish(mut self) -> Result<Ended, HearingError> {
        let decoder = self.decoder.take().expect("a hearing finishes once");
        let transcript = decoder
            .finish()
            .ok_or(HearingError::DecoderPanicked)?
            .map_err(HearingError::Decoder)?;
        let admission = self.gate.admission();
        let snapshot = match self.turns.take() {
            Some(turns) if admission == Admission::OwnerOnly => Some(turns.finish()),
            Some(turns) => Some(turns.finish_speech()),
            None => None,
        }
        .transpose()
        .map_err(HearingError::Tracking)?;
        let timeline = verdict(
            snapshot.as_ref().map(|snapshot| &*snapshot.speech),
            snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.timeline.as_deref()),
            admission,
        )?;
        let (released, own_voice) = match timeline {
            Some(timeline) => (
                self.gate.decide(timeline).released,
                timeline.admitted_is_owner(),
            ),
            None => (Vec::new(), false),
        };
        let transcript = if self.unspoken > 0 {
            tracing::debug!(
                segments = self.unspoken,
                "segments decoded before speech was heard dropped"
            );
            Transcript::new(self.trusted.join(" ")).map_err(HearingError::Decoder)?
        } else {
            transcript
        };
        Ok(Ended {
            transcript,
            released,
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
        Ok(speech::spoken_by(&speech, read_to))
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

/// A hearing dropped unfinished stops its decoder; the turn thread stops
/// itself when dropped.
impl Drop for Hearing<'_> {
    fn drop(&mut self) {
        if self.decoder.is_some() {
            self.abandon.abandoned.store(true, Ordering::Relaxed);
        }
    }
}

/// Queues at most one turn-thread wake: the caller's thread reads the
/// latest snapshot whenever it wakes, and more would take the slots the
/// decoder's updates need. A full queue already holds a wake.
fn wake_for(wake: &SyncSender<Wake>, pending: &AtomicBool) {
    if pending.swap(true, Ordering::AcqRel) {
        return;
    }
    match wake.try_send(Wake::Heard) {
        Ok(()) => {}
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
            pending.store(false, Ordering::Release);
        }
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
    use crate::speaker::testing::{OWN, Voice, tracker};
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
        let mut speech = SpeechDetector::new(Loud);
        let mut voices = tracker(Voice(OWN));
        let abandon = Abandon::linked(|| false);
        let feed = TurnFeed::start(
            &mut speech,
            Some(&mut voices),
            &mut recording,
            Admission::OwnerOnly,
        )
        .unwrap();
        let operation = transcription
            .start_with(&Language::Automatic, abandon.token())
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
                abandon,
                |_: &Snapshot| {},
            )
            .unwrap();
            let unblock = scope.spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                let _ = go_on.send(());
            });

            let Some(Event::Released(released)) = hearing.next().unwrap() else {
                panic!("the word is released");
            };
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
            let mut speech = SpeechDetector::new(Loud);
            let feed = TurnFeed::start(
                &mut speech,
                None::<&mut VoiceprintTracker<Voice>>,
                &mut recording,
                Admission::Everyone,
            )
            .unwrap();
            let abandon = Abandon::linked(|| false);
            let operation = transcription
                .start_with(&Language::Automatic, abandon.token())
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
                    abandon,
                    |_: &Snapshot| {},
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
                    assert!(matches!(ended, Err(HearingError::DecoderPanicked)));
                } else {
                    assert!(matches!(
                        ended,
                        Err(HearingError::Decoder(TranscriptionError::Inference { .. }))
                    ));
                }
                capture.join().unwrap();
            });
        }
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
                let mut speech = SpeechDetector::new(Loud);
                let mut voices = tracker(Voice(OWN));
                let feed = TurnFeed::start(
                    &mut speech,
                    Some(&mut voices),
                    &mut recording,
                    Admission::OwnerOnly,
                )
                .unwrap();
                let cancel = CancelToken::never();

                let heard = if saved {
                    hear_saved(feed, &cancel).map(drop)
                } else {
                    hear_to_end(feed, &recording, &cancel, |_: &Snapshot| {}).map(drop)
                };

                assert!(voices.timeline().is_broken(), "spoke={spoke} saved={saved}");
                if spoke {
                    assert!(
                        matches!(heard, Err(HearingError::Tracking(TrackingError::Failed))),
                        "saved={saved}"
                    );
                } else {
                    assert!(
                        matches!(
                            heard,
                            Err(HearingError::Decoder(TranscriptionError::NoSpeech))
                        ),
                        "saved={saved}"
                    );
                }
            }
        }
    }

    #[test]
    fn turn_thread_wakes_never_queue_behind_each_other() {
        let (wake, woken) = sync_channel(WAKE_CAPACITY);
        let pending = AtomicBool::new(false);
        for _ in 0..3 {
            wake_for(&wake, &pending);
        }

        assert!(matches!(woken.try_recv(), Ok(Wake::Heard)));
        assert!(woken.try_recv().is_err());
    }
}
