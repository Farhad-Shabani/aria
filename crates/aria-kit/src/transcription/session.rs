//! One transcription run: a recording the session owns, the transcription
//! over it, and the lifecycle between them. A [`Streaming`] session yields
//! words as they are committed; a `Whole` session has only its end.

use std::marker::PhantomData;

use super::{
    Language, Layer, Mode, Operation, Streaming, Transcriber, Transcript, TranscriptSegment,
    Transcription, TranscriptionError,
};
use crate::audio::RecordingReader;

#[cfg(not(target_family = "wasm"))]
use crate::audio::Stopper;
#[cfg(all(feature = "microphone", not(target_family = "wasm")))]
use crate::audio::{Microphone, RecordingMode};

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
