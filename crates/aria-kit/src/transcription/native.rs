//! Streaming through an engine whose encoder streams on its own: audio in
//! by the chunk, final words out. Nothing is revised, so there is no second
//! decode to wait for.

use crate::audio::{RecordingReader, canonical_duration};
use crate::cancel::CancelToken;

use super::{
    ChunkStep, ChunkStream, Emitted, Transcript, TranscriptSegment, TranscriptUpdate,
    TranscriptionError, check_cancelled,
};

/// Silence fed once the audio has ended, in chunks. The model holds a word
/// back until it has heard its right context, and one chunk covers that:
/// on the corpus a second one changes nothing.
const FLUSH_CHUNKS: usize = 1;

/// Drives a [`ChunkStream`] over a recording, one chunk per step.
pub struct NativeState {
    stream: Box<dyn ChunkStream>,
    /// Stream position handed to the decoder so far.
    fed: u64,
    /// A chunk's last word, kept until the next chunk shows whether it
    /// opens with the rest of it.
    held: Option<Held>,
    transcript: Vec<String>,
    finished: bool,
}

struct Held {
    text: String,
    /// Where the word's audio starts.
    from: u64,
}

impl NativeState {
    pub(super) fn new(stream: Box<dyn ChunkStream>) -> Self {
        Self {
            stream,
            fed: 0,
            held: None,
            transcript: Vec::new(),
            finished: false,
        }
    }

    /// The audio of the held word and of the chunk the last segment came
    /// from both stay: their readers still want them.
    pub(super) fn retained_from(&self) -> u64 {
        let last_chunk = self.fed.saturating_sub(self.chunk());
        self.held
            .as_ref()
            .map_or(last_chunk, |held| held.from.min(last_chunk))
    }

    fn chunk(&self) -> u64 {
        self.stream.chunk_samples() as u64
    }

    pub(super) fn next_finalized_with(
        &mut self,
        recording: &mut RecordingReader,
        cancel: &CancelToken,
    ) -> Result<Option<ChunkStep>, TranscriptionError> {
        if self.finished {
            return Ok(None);
        }
        let chunk = self.chunk();
        loop {
            check_cancelled(cancel)?;
            let whole_chunk = recording
                .fill_through(self.fed + chunk)
                .map_err(TranscriptionError::Capture)?;
            check_cancelled(cancel)?;
            if !whole_chunk {
                return self.finish(recording, cancel);
            }
            let at = self.fed;
            let emitted = self.stream.feed(recording.span(at..at + chunk))?;
            self.fed = at + chunk;
            tracing::debug!(
                fed_ms = canonical_duration(self.fed).as_millis(),
                words = emitted.words.as_str().split_whitespace().count(),
                "native decode step"
            );
            if let Some(segment) = self.absorb(&emitted, at, chunk) {
                self.transcript.push(segment.as_str().to_owned());
                return Ok(Some(ChunkStep {
                    update: Some(TranscriptUpdate::Finalized(segment)),
                    transcript: None,
                }));
            }
        }
    }

    /// Takes in what a chunk let go of and returns the words now complete,
    /// pinned to the audio they came from. The chunk's last word stays
    /// held: the next chunk may open with the rest of it.
    fn absorb(&mut self, emitted: &Emitted, at: u64, chunk: u64) -> Option<TranscriptSegment> {
        let heard = &emitted.words;
        let mut words: Vec<String> = heard
            .as_str()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if words.is_empty() {
            return None;
        }
        let timings = heard.words();
        let starts_at = |index: usize| match timings.get(index) {
            Some(timing) => timing.span_from(at).start,
            None => at,
        };
        let ends_at = |index: usize| match timings.get(index) {
            Some(timing) => timing.span_from(at).end,
            None => at + chunk,
        };

        let mut complete = Vec::new();
        let mut from = starts_at(0);
        let mut first_joined = false;
        if let Some(held) = self.held.take() {
            from = held.from;
            if emitted.joins_previous {
                words[0].insert_str(0, &held.text);
                first_joined = true;
            } else {
                complete.push(held.text);
            }
        }
        let last = words.pop().expect("a chunk with words has a last one");
        let last_index = words.len();
        let last_from = match (last_index, first_joined) {
            (0, true) => from,
            _ => starts_at(last_index),
        };
        complete.extend(words);
        self.held = Some(Held {
            text: last,
            from: last_from,
        });
        // The held word delivered on its own ended before this chunk.
        let to = match last_index {
            0 => at,
            _ => ends_at(last_index - 1),
        };
        TranscriptSegment::spanned(&complete, Some(from..to.max(from)))
    }

    /// Feeds what is left of the audio, padded to a chunk, then silence,
    /// and delivers everything still unsaid as one last segment.
    fn finish(
        &mut self,
        recording: &RecordingReader,
        cancel: &CancelToken,
    ) -> Result<Option<ChunkStep>, TranscriptionError> {
        self.finished = true;
        let chunk = self.chunk();
        let audio_end = recording.held_to();
        let tail_start = self.fed;
        let mut parts: Vec<String> = Vec::new();
        if audio_end > tail_start {
            let mut tail = recording.span(tail_start..audio_end).to_vec();
            // A short chunk is dropped by the model, not decoded.
            tail.resize(chunk as usize, 0.0);
            let emitted = self.stream.feed(&tail)?;
            parts.extend(
                self.absorb(&emitted, tail_start, chunk)
                    .map(TranscriptSegment::into_text),
            );
        }
        if audio_end > 0 {
            let silence = vec![0.0; chunk as usize];
            for _ in 0..FLUSH_CHUNKS {
                check_cancelled(cancel)?;
                let emitted = self.stream.feed(&silence)?;
                parts.extend(
                    self.absorb(&emitted, audio_end, chunk)
                        .map(TranscriptSegment::into_text),
                );
            }
        }
        parts.extend(self.held.take().map(|held| held.text));
        // What comes out here was spoken no earlier than the last full
        // chunk, whose audio stays retained; the span starts there.
        let span = tail_start.saturating_sub(chunk)..audio_end;
        let segment = TranscriptSegment::spanned(&parts, Some(span));
        if let Some(segment) = &segment {
            self.transcript.push(segment.as_str().to_owned());
        }
        let text = std::mem::take(&mut self.transcript).join(" ");
        let transcript = (!text.is_empty())
            .then(|| Transcript::new(text))
            .transpose()?;
        Ok(Some(ChunkStep {
            update: segment.map(TranscriptUpdate::Finalized),
            transcript,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::audio::testing::{Pace, paced, recorded};
    use crate::transcription::{Hypothesis, TimedWord};

    /// 100 ms, the batch a test recording is fed in.
    const CHUNK: usize = 1_600;

    /// Answers each chunk with the next entry of its script, recording how
    /// long each fed chunk was and releasing the next paced batch. A script
    /// entry opening with `+` continues the previous chunk's last word.
    struct Scripted {
        answers: VecDeque<Emitted>,
        fed: Arc<Mutex<Vec<usize>>>,
        pace: Option<Pace>,
    }

    fn emitted(script: &str) -> Emitted {
        match script.strip_prefix('+') {
            Some(rest) => Emitted {
                words: Hypothesis::new(rest),
                joins_previous: true,
            },
            None => Emitted {
                words: Hypothesis::new(script),
                joins_previous: false,
            },
        }
    }

    impl Scripted {
        fn new(answers: &[&str]) -> (Self, Arc<Mutex<Vec<usize>>>) {
            let fed = Arc::new(Mutex::new(Vec::new()));
            let stream = Self {
                answers: answers.iter().map(|script| emitted(script)).collect(),
                fed: Arc::clone(&fed),
                pace: None,
            };
            (stream, fed)
        }

        fn timed(answers: Vec<Hypothesis>) -> Self {
            Self {
                answers: answers
                    .into_iter()
                    .map(|words| Emitted {
                        words,
                        joins_previous: false,
                    })
                    .collect(),
                fed: Arc::new(Mutex::new(Vec::new())),
                pace: None,
            }
        }
    }

    impl ChunkStream for Scripted {
        fn chunk_samples(&self) -> usize {
            CHUNK
        }

        fn feed(&mut self, chunk: &[f32]) -> Result<Emitted, TranscriptionError> {
            self.fed.lock().expect("no poison").push(chunk.len());
            if let Some(pace) = &self.pace {
                pace.advance();
            }
            Ok(self.answers.pop_front().unwrap_or_else(|| emitted("")))
        }
    }

    fn state(stream: Scripted) -> NativeState {
        NativeState::new(Box::new(stream))
    }

    fn segments(state: &mut NativeState, recording: &mut RecordingReader) -> Vec<String> {
        let cancel = CancelToken::never();
        let mut texts = Vec::new();
        while let Some(step) = state
            .next_finalized_with(recording, &cancel)
            .expect("the step succeeds")
        {
            if let Some(TranscriptUpdate::Finalized(segment)) = step.update {
                texts.push(segment.as_str().to_owned());
            }
        }
        texts
    }

    #[test]
    fn a_chunks_last_word_waits_for_the_next_chunk() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 3 * CHUNK]);
        let (stream, _) = Scripted::new(&["one two", "three", "four five"]);
        let mut state = state(stream);

        assert_eq!(
            segments(&mut state, &mut recording),
            ["one", "two", "three four", "five"]
        );
    }

    #[test]
    fn a_word_split_across_chunks_is_joined() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 3 * CHUNK]);
        let (stream, _) = Scripted::new(&["thrown into pris", "+on for this", "+ness"]);
        let mut state = state(stream);

        assert_eq!(
            segments(&mut state, &mut recording),
            ["thrown into", "prison for", "thisness"]
        );
    }

    #[test]
    fn a_silent_chunk_waits_for_the_next() {
        let (_directory, mut recording, pace) = paced(vec![
            vec![0_i16; CHUNK],
            vec![0_i16; CHUNK],
            vec![0_i16; CHUNK],
        ]);
        let (mut stream, _) = Scripted::new(&["", "late one", ""]);
        stream.pace = Some(pace);
        let mut state = state(stream);

        let first = state
            .next_finalized_with(&mut recording, &CancelToken::never())
            .expect("the step succeeds")
            .expect("a segment arrives");

        let Some(TranscriptUpdate::Finalized(segment)) = first.update else {
            panic!("a segment");
        };
        assert_eq!(segment.as_str(), "late");
        assert_eq!(state.fed, 2 * CHUNK as u64);
    }

    #[test]
    fn the_last_partial_chunk_is_padded_and_flushed() {
        let (_directory, mut recording) = recorded(&vec![0_i16; CHUNK + CHUNK / 2]);
        let (stream, fed) = Scripted::new(&["one", "two", "three"]);
        let mut state = state(stream);

        let texts = segments(&mut state, &mut recording);

        assert_eq!(texts, ["one two three"]);
        assert_eq!(*fed.lock().expect("no poison"), [CHUNK; 3]);
    }

    #[test]
    fn frames_become_absolute_stream_spans() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 3 * CHUNK]);
        let timed = Hypothesis::with_words(
            "hello world",
            vec![
                TimedWord {
                    start: 0.01,
                    end: 0.05,
                },
                TimedWord {
                    start: 0.06,
                    end: 0.09,
                },
            ],
        );
        let mut state = state(Scripted::timed(vec![
            Hypothesis::new(""),
            Hypothesis::new(""),
            timed,
        ]));

        let step = state
            .next_finalized_with(&mut recording, &CancelToken::never())
            .expect("the step succeeds")
            .expect("a segment arrives");

        let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
            panic!("a segment");
        };
        assert_eq!(segment.as_str(), "hello");
        let at = 2 * CHUNK as u64;
        assert_eq!(segment.span().expect("timed"), at + 160..at + 800);
    }

    #[test]
    fn an_untimed_chunk_spans_itself() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 2 * CHUNK]);
        let (stream, _) = Scripted::new(&["", "hello world"]);
        let mut state = state(stream);

        let step = state
            .next_finalized_with(&mut recording, &CancelToken::never())
            .expect("the step succeeds")
            .expect("a segment arrives");

        let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
            panic!("a segment");
        };
        let span = segment.span().expect("spanned");
        assert_eq!(span, CHUNK as u64..2 * CHUNK as u64);
    }

    #[test]
    fn a_held_word_delivered_alone_ends_where_the_next_chunk_starts() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 3 * CHUNK]);
        let (stream, _) = Scripted::new(&["hello", "world", ""]);
        let mut state = state(stream);

        let step = state
            .next_finalized_with(&mut recording, &CancelToken::never())
            .expect("the step succeeds")
            .expect("a segment arrives");

        let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
            panic!("a segment");
        };
        assert_eq!(segment.as_str(), "hello");
        assert_eq!(segment.span().expect("spanned"), 0..CHUNK as u64);
    }

    #[test]
    fn the_flush_segment_never_claims_audio_past_the_recording() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 2 * CHUNK]);
        let (stream, _) = Scripted::new(&["", "", "held back"]);
        let mut state = state(stream);

        let step = state
            .next_finalized_with(&mut recording, &CancelToken::never())
            .expect("the step succeeds")
            .expect("the end delivers");

        let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
            panic!("a segment");
        };
        assert_eq!(segment.as_str(), "held back");
        assert_eq!(
            segment.span().expect("spanned"),
            CHUNK as u64..2 * CHUNK as u64
        );
        assert!(step.transcript.is_some());
    }

    #[test]
    fn retained_from_keeps_the_held_word_and_the_last_chunk() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 3 * CHUNK]);
        let (stream, _) = Scripted::new(&["one", "", "two"]);
        let mut state = state(stream);
        let cancel = CancelToken::never();

        assert_eq!(state.retained_from(), 0);
        state
            .next_finalized_with(&mut recording, &cancel)
            .expect("the step succeeds");
        // "one" is delivered by chunk three, which holds "two" from position
        // 2·CHUNK; the chunk before it stays as well.
        assert_eq!(state.fed, 3 * CHUNK as u64);
        assert_eq!(state.retained_from(), 2 * CHUNK as u64);
    }

    #[test]
    fn segments_concatenate_into_the_final_transcript() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 2 * CHUNK]);
        let (stream, _) = Scripted::new(&["Hello there,", "friend.", "Bye."]);
        let mut state = state(stream);
        let cancel = CancelToken::never();

        let mut transcript = None;
        while let Some(step) = state
            .next_finalized_with(&mut recording, &cancel)
            .expect("the step succeeds")
        {
            transcript = transcript.or(step.transcript);
        }

        assert_eq!(
            transcript.expect("a transcript").as_str(),
            "Hello there, friend. Bye."
        );
    }

    #[test]
    fn a_silent_stream_finishes_without_a_transcript() {
        let (_directory, mut recording) = recorded(&vec![0_i16; CHUNK]);
        let (stream, _) = Scripted::new(&[]);
        let mut state = state(stream);

        let step = state
            .next_finalized_with(&mut recording, &CancelToken::never())
            .expect("the step succeeds")
            .expect("the end is a step");

        assert!(step.update.is_none());
        assert!(step.transcript.is_none());
        assert!(
            state
                .next_finalized_with(&mut recording, &CancelToken::never())
                .expect("the step succeeds")
                .is_none()
        );
    }

    #[test]
    fn an_empty_recording_is_the_capture_error_and_feeds_nothing() {
        let (_directory, mut recording) = recorded(&[]);
        let (stream, fed) = Scripted::new(&["never"]);
        let mut state = state(stream);

        let outcome = state.next_finalized_with(&mut recording, &CancelToken::never());

        assert!(matches!(outcome, Err(TranscriptionError::Capture(_))));
        assert!(fed.lock().expect("no poison").is_empty());
    }

    #[test]
    fn a_cancelled_operation_stops_before_the_next_feed() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 2 * CHUNK]);
        let (stream, fed) = Scripted::new(&["one", "two"]);
        let mut state = state(stream);
        let cancel = CancelToken::new();
        cancel.cancel();

        let outcome = state.next_finalized_with(&mut recording, &cancel);

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
        assert!(fed.lock().expect("no poison").is_empty());
    }
}
