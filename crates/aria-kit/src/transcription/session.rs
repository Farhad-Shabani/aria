//! One transcription run: a recording the session owns, the transcription
//! over it, and the lifecycle between them. A [`Streaming`] session yields
//! words as they are committed; a `Whole` session has only its end.

use std::marker::PhantomData;

use super::{
    Language, Layer, Mode, Operation, Streaming, Transcriber, Transcript, TranscriptSegment,
    Transcription, TranscriptionError,
};
use crate::audio::RecordingReader;

#[cfg(all(feature = "silero", not(target_family = "wasm")))]
use super::next_heard;
#[cfg(not(target_family = "wasm"))]
use crate::audio::Stopper;
#[cfg(all(feature = "microphone", not(target_family = "wasm")))]
use crate::audio::{Microphone, RecordingMode};
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
use crate::primitives::error_chain;
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
use crate::segmentation::Segmenter;
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
use crate::speaker::{
    Admission, Diarizer, Embedder, Gate, Released, SpeakerTracker, Timeline, TurnFeed,
    VoiceprintTracker,
};
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
use crate::voice_activity::{SpeechDetector, VoiceActivityDetector};
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
use std::collections::VecDeque;

/// One transcription run over audio that arrives while the run is live.
///
/// Constructed over a recording the caller feeds, or straight from the
/// microphone where one exists. `finish` waits for input to end, drains the
/// tail, and returns the completed transcript.
pub struct Session<'a, T, M: Mode, L = ()> {
    operation: Operation<'a, T, L>,
    recording: RecordingReader,
    /// Held so the capture worker outlives the run.
    #[cfg(all(feature = "microphone", not(target_family = "wasm")))]
    _microphone: Option<Microphone>,
    #[cfg(not(target_family = "wasm"))]
    stopper: Option<Stopper>,
    mode: PhantomData<M>,
}

impl<'a, T: Transcriber, M: Mode, L: Layer> Session<'a, T, M, L> {
    /// Runs against a recording the caller already has. Whoever feeds it also
    /// ends it; `finish` only waits for that end and finishes the transcript.
    pub fn over(
        transcription: &'a Transcription<T, M, L>,
        language: &'a Language,
        recording: RecordingReader,
    ) -> Result<Self, TranscriptionError> {
        Ok(Self {
            operation: transcription.start(language)?,
            recording,
            #[cfg(all(feature = "microphone", not(target_family = "wasm")))]
            _microphone: None,
            #[cfg(not(target_family = "wasm"))]
            stopper: None,
            mode: PhantomData,
        })
    }

    /// Waits for input to end, drains whatever remains, and returns the
    /// completed transcript — or [`TranscriptionError::NoSpeech`] when
    /// nothing was said.
    pub fn finish(mut self) -> Result<Transcript, TranscriptionError> {
        #[cfg(not(target_family = "wasm"))]
        if let Some(stopper) = &self.stopper {
            stopper.stop();
        }
        self.operation.finish(&mut self.recording)
    }
}

#[cfg(all(feature = "microphone", not(target_family = "wasm")))]
impl<'a, T: Transcriber, M: Mode, L: Layer> Session<'a, T, M, L> {
    /// Opens the microphone and records one utterance until stopped. `finish`
    /// ends the recording; until then every [`Streaming`] step blocks on
    /// captured audio.
    pub fn microphone(
        transcription: &'a Transcription<T, M, L>,
        language: &'a Language,
    ) -> Result<Self, TranscriptionError> {
        // Recording starts first: `start` can block, and speech during it
        // must not be lost.
        let mut microphone = Microphone::open().map_err(TranscriptionError::Capture)?;
        let (recording, stopper) = microphone
            .record(RecordingMode::UntilStopped)
            .map_err(TranscriptionError::Capture)?;
        let operation = match transcription.start(language) {
            Ok(operation) => operation,
            Err(error) => {
                stopper.stop();
                return Err(error);
            }
        };
        Ok(Self {
            operation,
            recording,
            _microphone: Some(microphone),
            stopper: Some(stopper),
            mode: PhantomData,
        })
    }
}

impl<'a, T: Transcriber, L: Layer> Session<'a, T, Streaming, L> {
    /// The next committed stretch of words, or `None` once the input has
    /// ended and the tail is delivered. Committed words are never revised.
    pub fn next_finalized(&mut self) -> Result<Option<TranscriptSegment>, TranscriptionError> {
        self.operation.next_finalized(&mut self.recording)
    }

    /// The same session, keeping only the owner's words. `vad` finds where
    /// anyone spoke, `diarizer` whose voice it was, and `segmenter` the
    /// speaker changes. Call before reading the first segment.
    #[cfg(all(feature = "silero", not(target_family = "wasm")))]
    pub fn only_own_voice<V: VoiceActivityDetector, E: Embedder, G: Segmenter>(
        mut self,
        vad: V,
        diarizer: &'a mut Diarizer<E>,
        segmenter: G,
    ) -> Result<HeardSession<'a, T, V, VoiceprintTracker<'a, E, G>, L>, TranscriptionError> {
        let from = self.recording.held_from();
        let speech = SpeechDetector::new(vad, from);
        let tracker =
            VoiceprintTracker::owner_only(diarizer, segmenter, from).map_err(|error| {
                tracing::warn!(error = %error_chain(&error), "own-voice filtering is unavailable");
                TranscriptionError::SpeakerTracking
            })?;
        let feed = TurnFeed::start(&mut self.recording, speech, Some(tracker));
        Ok(HeardSession {
            gate: Gate::new(Admission::OwnerOnly, feed.held_from()),
            session: self,
            feed,
            released: VecDeque::new(),
            decoded: false,
            kept: Vec::new(),
        })
    }
}

/// A streaming session that keeps only the owner's words. Voices are
/// judged on the caller's thread, as each segment is read.
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
pub struct HeardSession<'a, T, V, K, L = ()> {
    session: Session<'a, T, Streaming, L>,
    feed: TurnFeed<V, K>,
    gate: Gate,
    /// Segments the gate let through, not yet handed out.
    released: VecDeque<Released>,
    decoded: bool,
    /// What was handed out, for the transcript.
    kept: Vec<String>,
}

#[cfg(all(feature = "silero", not(target_family = "wasm")))]
impl<T: Transcriber, V: VoiceActivityDetector, K: SpeakerTracker, L: Layer>
    HeardSession<'_, T, V, K, L>
{
    /// The next finalized segment and the enrolled voice it was heard in,
    /// or `None` once the input has ended and the tail is delivered. Each
    /// word waits until its turn is decided, then the owner's and those too
    /// short to tell are handed out.
    pub fn next_heard(&mut self) -> Result<Option<Released>, TranscriptionError> {
        loop {
            unbroken_timeline(&self.feed)?;
            if let Some(released) = self.released.pop_front() {
                self.kept.push(released.heard.segment.as_str().to_owned());
                return Ok(Some(released));
            }
            if self.decoded {
                return Ok(None);
            }
            let operation = &mut self.session.operation;
            match next_heard(operation, &mut self.session.recording)? {
                Some(heard) => {
                    self.feed.hear_through(operation.read_to());
                    self.gate.hold(heard);
                }
                None => {
                    self.feed.hear_rest();
                    self.decoded = true;
                }
            }
            let decision = self.gate.decide(unbroken_timeline(&self.feed)?);
            self.released.extend(decision.released);
        }
    }

    /// The completed transcript: what was kept, and
    /// [`TranscriptionError::NoSpeech`] when that is nothing.
    pub fn finish(mut self) -> Result<Transcript, TranscriptionError> {
        if let Some(stopper) = &self.session.stopper {
            stopper.stop();
        }
        while self.next_heard()?.is_some() {}
        Transcript::new(self.kept.join(" "))
    }
}

/// The timeline to decide words by. Once it breaks, nobody can tell whose
/// words to drop.
#[cfg(all(feature = "silero", not(target_family = "wasm")))]
fn unbroken_timeline<V: VoiceActivityDetector, K: SpeakerTracker>(
    feed: &TurnFeed<V, K>,
) -> Result<&Timeline, TranscriptionError> {
    match feed.timeline() {
        Some(timeline) if !timeline.is_broken() => Ok(timeline),
        _ => Err(TranscriptionError::SpeakerTracking),
    }
}

#[cfg(not(target_family = "wasm"))]
impl<T, M: Mode, L> Drop for Session<'_, T, M, L> {
    fn drop(&mut self) {
        // A session dropped without finish() must not leave the microphone
        // recording to its ceiling. Stopping an already-stopped input is a
        // no-op, so this composes with finish().
        if let Some(stopper) = &self.stopper {
            stopper.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::audio::CANONICAL_SAMPLE_RATE;
    use crate::audio::testing::{Pace, paced, recorded};
    use crate::cancel::CancelToken;
    use crate::transcription::{Cadence, Hypothesis, TranscriptionAudio};

    const STRIDE_MS: usize = 100;
    const STRIDE: usize = STRIDE_MS * CANONICAL_SAMPLE_RATE as usize / 1000;

    /// Answers each decode with the next entry of its script, releasing the
    /// next paced batch as it goes.
    struct Scripted {
        answers: Mutex<VecDeque<&'static str>>,
        pace: Option<Pace>,
    }

    impl Scripted {
        fn new(answers: &[&'static str]) -> Self {
            Self {
                answers: Mutex::new(answers.iter().copied().collect()),
                pace: None,
            }
        }

        fn paced(answers: &[&'static str], pace: Pace) -> Self {
            Self {
                answers: Mutex::new(answers.iter().copied().collect()),
                pace: Some(pace),
            }
        }
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
            if let Some(pace) = &self.pace {
                pace.advance();
            }
            let answer = self
                .answers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front()
                .expect("the script covers every decode");
            Ok(Hypothesis::new(answer))
        }
    }

    fn stride_batch() -> Vec<i16> {
        vec![8_192; STRIDE]
    }

    #[test]
    fn a_streaming_session_commits_words_as_their_agreement_holds() {
        let (_directory, recording, pace) = paced(vec![stride_batch(); 4]);
        let cadence = Cadence::from_millis(STRIDE_MS, 20_000);
        let transcription = Transcription::agreement(
            cadence,
            Scripted::paced(&["one", "one two", "one two three", "one two three"], pace),
        );
        let language = Language::Automatic;
        let mut session =
            Session::over(&transcription, &language, recording).expect("the session starts");

        let mut segments = Vec::new();
        while let Some(segment) = session.next_finalized().expect("every step succeeds") {
            segments.push(segment.into_text());
        }
        let transcript = session.finish().expect("the transcript completes");

        assert_eq!(segments, ["one", "two", "three"]);
        assert_eq!(transcript.as_str(), "one two three");
    }

    #[cfg(feature = "silero")]
    mod heard {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use super::*;
        use crate::speaker::{
            Diarizer, Embedder, LevelSegmenter, Profile, Role, SpeakerError, VoiceprintTracker,
            enrolled_voice,
        };
        use crate::transcription::TimedWord;
        use crate::voice_activity::{VoiceActivityDetector, VoiceActivityError};

        const OWN: [f32; 2] = [1.0, 0.0];
        const OTHER: [f32; 2] = [0.0, 1.0];

        /// Owns everything a session that keeps other voices out borrows.
        struct Harness<T, V, E> {
            _directory: tempfile::TempDir,
            recording: Option<RecordingReader>,
            transcription: Transcription<T, Streaming>,
            language: Language,
            vad: V,
            diarizer: Diarizer<E>,
            segmenter: LevelSegmenter,
        }

        impl<T: Transcriber, V: VoiceActivityDetector, E: Embedder> Harness<T, V, E> {
            fn new(
                (directory, recording): (tempfile::TempDir, RecordingReader),
                transcription: Transcription<T, Streaming>,
                vad: V,
                voice: E,
                profiles: Vec<Profile>,
            ) -> Self {
                Self {
                    _directory: directory,
                    recording: Some(recording),
                    transcription,
                    language: Language::Automatic,
                    vad,
                    diarizer: Diarizer::new(voice, profiles),
                    segmenter: LevelSegmenter,
                }
            }

            fn only_own_voice(
                &mut self,
            ) -> Result<
                HeardSession<'_, T, &mut V, VoiceprintTracker<'_, E, &mut LevelSegmenter>>,
                TranscriptionError,
            > {
                let recording = self.recording.take().expect("one session per harness");
                Session::over(&self.transcription, &self.language, recording)?.only_own_voice(
                    &mut self.vad,
                    &mut self.diarizer,
                    &mut self.segmenter,
                )
            }
        }

        fn owner() -> Vec<Profile> {
            vec![enrolled_voice("alex", Role::Owner, OWN.to_vec())]
        }

        /// Any nonzero sample is speech.
        struct Loud;

        impl VoiceActivityDetector for Loud {
            const FRAME_SAMPLES: usize = 160;

            fn probability(&mut self, samples: &[i16]) -> Result<f32, VoiceActivityError> {
                Ok(if samples.iter().any(|&sample| sample != 0) {
                    1.0
                } else {
                    0.0
                })
            }
        }

        struct Voice([f32; 2]);

        impl Embedder for Voice {
            fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
                Ok(Some(self.0.to_vec()))
            }

            fn dimension(&self) -> usize {
                2
            }
        }

        /// Hears "one" spanning the whole window it is given.
        struct Timed;

        impl Transcriber for Timed {
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
                let TranscriptionAudio::Samples(samples) = audio else {
                    unreachable!("the session hands over samples");
                };
                let end = samples.len() as f32 / CANONICAL_SAMPLE_RATE as f32;
                Ok(Hypothesis::with_words(
                    "one",
                    vec![TimedWord { start: 0.0, end }],
                ))
            }
        }

        fn cadence() -> Cadence {
            Cadence::from_millis(STRIDE_MS, 20_000)
        }

        #[test]
        fn a_session_that_keeps_other_voices_out_hands_out_only_the_owners_words() {
            // A finished recording is heard in one decode, so the one segment
            // spans its whole 3.4 s: enough to judge.
            let judged = |voice: [f32; 2], read_first: bool| {
                let recorded = recorded(&vec![8_192; 34 * STRIDE]);
                let transcription = Transcription::agreement(cadence(), Timed);
                let mut harness =
                    Harness::new(recorded, transcription, Loud, Voice(voice), owner());
                let mut session = harness.only_own_voice().expect("the session starts");

                let mut heard = Vec::new();
                if read_first {
                    while let Some(segment) = session.next_heard().expect("every step succeeds") {
                        heard.push(segment.heard.segment.into_text());
                    }
                }
                (heard, session.finish().map(Transcript::into_text))
            };

            let (heard, transcript) = judged(OTHER, true);
            assert!(heard.is_empty());
            assert!(matches!(transcript, Err(TranscriptionError::NoSpeech)));

            let (heard, transcript) = judged(OWN, true);
            assert_eq!(heard, ["one"]);
            assert_eq!(transcript.expect("the kept words complete"), "one");

            let (heard, transcript) = judged(OWN, false);
            assert!(heard.is_empty());
            assert_eq!(
                transcript.expect("stop drains the owner's remaining words"),
                "one"
            );
        }

        #[test]
        fn owner_filtering_without_an_owner_cannot_return_every_voice() {
            for profiles in [
                Vec::new(),
                vec![enrolled_voice("guest", Role::Guest, OWN.to_vec())],
            ] {
                let recorded = recorded(&vec![8_192; 34 * STRIDE]);
                let transcription = Transcription::agreement(cadence(), Timed);
                let mut harness = Harness::new(recorded, transcription, Loud, Voice(OWN), profiles);
                assert!(matches!(
                    harness.only_own_voice(),
                    Err(TranscriptionError::SpeakerTracking)
                ));
            }
        }

        #[test]
        fn filtering_reports_tracking_failure_even_after_returning_owner_words() {
            /// Hears speech until told to fail.
            struct Failing(Arc<AtomicBool>);

            impl VoiceActivityDetector for Failing {
                const FRAME_SAMPLES: usize = 160;

                fn probability(&mut self, _: &[i16]) -> Result<f32, VoiceActivityError> {
                    if self.0.load(Ordering::Relaxed) {
                        Err(VoiceActivityError::InvalidProbability(f32::NAN))
                    } else {
                        Ok(1.0)
                    }
                }
            }

            /// Times each scripted word to its own second.
            struct TimedScript(Scripted);

            impl Transcriber for TimedScript {
                fn validate_language(
                    &self,
                    language: &Language,
                    cancel: &CancelToken,
                ) -> Result<(), TranscriptionError> {
                    self.0.validate_language(language, cancel)
                }

                fn transcribe_with(
                    &self,
                    audio: TranscriptionAudio<'_>,
                    language: &Language,
                    cancel: &CancelToken,
                ) -> Result<Hypothesis, TranscriptionError> {
                    let hypothesis = self.0.transcribe_with(audio, language, cancel)?;
                    let spans = hypothesis
                        .as_str()
                        .split_whitespace()
                        .enumerate()
                        .map(|(index, _)| TimedWord {
                            start: index as f32,
                            end: (index + 1) as f32,
                        })
                        .collect();
                    Ok(Hypothesis::with_words(hypothesis.as_str(), spans))
                }
            }

            for read_after_failure in [true, false] {
                let failed = Arc::new(AtomicBool::new(false));
                let (directory, recording, pace) = paced(vec![vec![8_192; 10 * STRIDE]; 3]);
                let transcription = Transcription::agreement(
                    Cadence::from_millis(1_000, 20_000),
                    TimedScript(Scripted::paced(&["one", "one two", "one two three"], pace)),
                );
                let detector = Failing(Arc::clone(&failed));
                let mut harness = Harness::new(
                    (directory, recording),
                    transcription,
                    detector,
                    Voice(OWN),
                    owner(),
                );
                let mut session = harness.only_own_voice().expect("the session starts");

                let first = session
                    .next_heard()
                    .unwrap()
                    .expect("the owner's first word");
                assert_eq!(first.heard.segment.as_str(), "one");
                failed.store(true, Ordering::Relaxed);
                if read_after_failure {
                    assert!(matches!(
                        session.next_heard(),
                        Err(TranscriptionError::SpeakerTracking)
                    ));
                }
                assert!(matches!(
                    session.finish(),
                    Err(TranscriptionError::SpeakerTracking)
                ));
            }
        }
    }

    #[test]
    fn a_whole_session_has_only_its_end() {
        let (_directory, recording) = recorded(&[0; 64]);
        let transcription = Transcription::complete_samples(Scripted::new(&["hello world"]));
        let language = Language::Automatic;

        let transcript = Session::over(&transcription, &language, recording)
            .expect("the session starts")
            .finish()
            .expect("the transcript completes");

        assert_eq!(transcript.as_str(), "hello world");
    }
}
