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
    held: Option<Word>,
    transcript: Vec<String>,
    finished: bool,
}

struct Word {
    text: String,
    window: std::ops::Range<u64>,
    timed: bool,
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
            .map_or(last_chunk, |held| held.window.start.min(last_chunk))
    }

    fn chunk(&self) -> u64 {
        self.stream.chunk_samples() as u64
    }

    /// Feeds chunks until one completes a word, and returns those words as
    /// a segment. With `previews`, a chunk that completes none returns its
    /// unfinished last word as provisional text instead.
    pub(super) fn next_finalized_with(
        &mut self,
        recording: &mut RecordingReader,
        cancel: &CancelToken,
        previews: bool,
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
            let words = self.absorb(&emitted, at, chunk)?;
            if let Some(segment) = segment(&words, self.fed) {
                let segment = segment.bounded(self.fed)?;
                self.transcript.push(segment.as_str().to_owned());
                return Ok(Some(ChunkStep {
                    update: Some(TranscriptUpdate::Finalized(segment)),
                    transcript: None,
                }));
            }
            if previews {
                let text = self
                    .held
                    .as_ref()
                    .map_or_else(String::new, |held| held.text.clone());
                let source_window = self.held.as_ref().and_then(|held| {
                    segment(std::slice::from_ref(held), self.fed)?.source_window()
                });
                return Ok(Some(ChunkStep {
                    update: Some(TranscriptUpdate::Provisional {
                        text,
                        source_window,
                    }),
                    transcript: None,
                }));
            }
        }
    }

    /// Takes in what a chunk let go of and returns the words now complete,
    /// pinned to the audio they came from. The chunk's last word stays
    /// held: the next chunk may open with the rest of it.
    fn absorb(
        &mut self,
        emitted: &Emitted,
        at: u64,
        chunk: u64,
    ) -> Result<Vec<Word>, TranscriptionError> {
        let heard = &emitted.words;
        let mut words: Vec<Word> = heard
            .as_str()
            .split_whitespace()
            .enumerate()
            .map(|(index, text)| {
                let timing = heard.words().get(index);
                let window = match timing {
                    Some(timing) => super::bounded_span(timing.span_from(at)?, at + chunk)?,
                    None => at..at + chunk,
                };
                Ok(Word {
                    text: text.to_owned(),
                    window,
                    timed: timing.is_some(),
                })
            })
            .collect::<Result<_, TranscriptionError>>()?;
        if words.is_empty() {
            return Ok(Vec::new());
        }

        let mut complete = Vec::new();
        if let Some(held) = self.held.take() {
            if emitted.joins_previous {
                let first = &mut words[0];
                if first.text.chars().all(super::sentence_punctuation)
                    && held
                        .text
                        .chars()
                        .any(|character| !super::sentence_punctuation(character))
                {
                    first.window.end = held.window.end;
                }
                first.text.insert_str(0, &held.text);
                first.window.start = held.window.start;
                first.timed &= held.timed;
            } else {
                complete.push(held);
            }
        }
        self.held = words.pop();
        complete.extend(words);
        Ok(complete)
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
        let mut parts = Vec::new();
        if audio_end > tail_start {
            let mut tail = recording.span(tail_start..audio_end).to_vec();
            // A short chunk is dropped by the model, not decoded.
            tail.resize(chunk as usize, 0.0);
            let emitted = self.stream.feed(&tail)?;
            parts.extend(self.absorb(&emitted, tail_start, chunk)?);
        }
        if audio_end > 0 {
            let silence = vec![0.0; chunk as usize];
            for _ in 0..FLUSH_CHUNKS {
                check_cancelled(cancel)?;
                let emitted = self.stream.feed(&silence)?;
                parts.extend(self.absorb(&emitted, audio_end, chunk)?);
            }
        }
        parts.extend(self.held.take());
        let retained = tail_start.saturating_sub(chunk)..audio_end;
        for word in &mut parts {
            if word.window.start >= audio_end {
                // A word emitted in synthetic padding has no real timestamp.
                word.window = retained.clone();
                word.timed = false;
            } else {
                word.window.end = word.window.end.min(audio_end);
            }
        }
        let segment = segment(&parts, audio_end);
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

fn segment(words: &[Word], audio_end: u64) -> Option<TranscriptSegment> {
    if words.iter().all(|word| word.timed) {
        let timed: Vec<_> = words
            .iter()
            .map(|word| (word.text.clone(), word.window.clone()))
            .collect();
        let mut segment = TranscriptSegment::timed(&timed)?;
        segment.source_window = segment.span();
        return Some(segment);
    }
    let text: Vec<_> = words.iter().map(|word| word.text.clone()).collect();
    let mut segment = TranscriptSegment::spanned(&text, None)?;
    // A native stream may emit untimed words from any earlier chunk.
    segment.source_window = Some(0..audio_end);
    Some(segment)
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
            .next_finalized_with(recording, &cancel, false)
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
            .next_finalized_with(&mut recording, &CancelToken::never(), false)
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
    fn malformed_word_timings_are_rejected_before_native_output() {
        let mut recording = RecordingReader::from_samples(&[0.5; CHUNK]);
        let mut state = state(Scripted::timed(vec![Hypothesis::with_words(
            "hello",
            vec![TimedWord {
                start: f32::NAN,
                end: 0.05,
            }],
        )]));

        assert!(matches!(
            state.next_finalized_with(&mut recording, &CancelToken::never(), false),
            Err(TranscriptionError::InvalidSegment)
        ));
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
            .next_finalized_with(&mut recording, &CancelToken::never(), false)
            .expect("the step succeeds")
            .expect("a segment arrives");

        let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
            panic!("a segment");
        };
        assert_eq!(segment.as_str(), "hello");
        let at = 2 * CHUNK as u64;
        assert_eq!(segment.span().expect("timed"), at + 160..at + 800);
        assert_eq!(
            segment.word_spans(),
            Some(std::slice::from_ref(&(at + 160..at + 800)))
        );

        let tail = state
            .next_finalized_with(&mut recording, &CancelToken::never(), false)
            .unwrap()
            .unwrap();
        let Some(TranscriptUpdate::Finalized(segment)) = tail.update else {
            panic!("the held word finishes");
        };
        assert_eq!(segment.as_str(), "world");
        assert_eq!(
            segment.word_spans(),
            Some(std::slice::from_ref(&(at + 960..at + 1440)))
        );
    }

    #[test]
    fn previews_keep_timed_sources_and_bound_untimed_sources_conservatively() {
        let at = 2 * CHUNK as u64;
        for (hypothesis, expected) in [
            (
                Hypothesis::with_words(
                    "hello",
                    vec![TimedWord {
                        start: 0.01,
                        end: 0.05,
                    }],
                ),
                at + 160..at + 800,
            ),
            (Hypothesis::new("hello"), 0..3 * CHUNK as u64),
        ] {
            let (_directory, mut recording) = recorded(&vec![0_i16; 3 * CHUNK]);
            let mut state = state(Scripted::timed(vec![
                Hypothesis::new(""),
                Hypothesis::new(""),
                hypothesis,
            ]));
            for index in 0..3 {
                let step = state
                    .next_finalized_with(&mut recording, &CancelToken::never(), true)
                    .unwrap()
                    .unwrap();
                let Some(TranscriptUpdate::Provisional {
                    text,
                    source_window,
                }) = step.update
                else {
                    panic!("an unfinished word remains provisional");
                };
                if index == 2 {
                    assert_eq!(text, "hello");
                    assert_eq!(source_window, Some(expected.clone()));
                } else {
                    assert!(text.is_empty());
                }
            }
        }
    }

    #[test]
    fn an_untimed_chunk_can_have_words_from_any_earlier_audio() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 2 * CHUNK]);
        let (stream, _) = Scripted::new(&["", "hello world"]);
        let mut state = state(stream);

        let step = state
            .next_finalized_with(&mut recording, &CancelToken::never(), false)
            .expect("the step succeeds")
            .expect("a segment arrives");

        let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
            panic!("a segment");
        };
        assert_eq!(segment.span(), None);
        assert_eq!(segment.source_window(), Some(0..2 * CHUNK as u64));
    }

    #[test]
    fn joined_sentence_punctuation_keeps_the_spoken_end() {
        for (prefix, suffix, end) in [("owner", ".", 1440), ("20", "%", 2240), (".", ".", 2240)] {
            let mut state = state(Scripted::timed(vec![]));
            let first = Emitted {
                words: Hypothesis::with_words(
                    prefix,
                    vec![TimedWord {
                        start: 0.01,
                        end: 0.09,
                    }],
                ),
                joins_previous: false,
            };
            assert!(state.absorb(&first, 0, CHUNK as u64).unwrap().is_empty());
            let second = Emitted {
                words: Hypothesis::with_words(
                    format!("{suffix} next"),
                    vec![
                        TimedWord {
                            start: 0.0,
                            end: 0.04,
                        },
                        TimedWord {
                            start: 0.05,
                            end: 0.09,
                        },
                    ],
                ),
                joins_previous: true,
            };
            let joined = segment(
                &state.absorb(&second, CHUNK as u64, CHUNK as u64).unwrap(),
                2 * CHUNK as u64,
            )
            .unwrap();
            assert_eq!(joined.as_str(), format!("{prefix}{suffix}"));
            assert_eq!(joined.word_spans(), Some(std::slice::from_ref(&(160..end))));
        }
    }

    #[test]
    fn a_word_completed_by_the_flush_keeps_its_real_audio() {
        let (_directory, mut recording) = recorded(&vec![0_i16; CHUNK]);
        let mut stream = Scripted::timed(vec![
            Hypothesis::with_words(
                "some",
                vec![TimedWord {
                    start: 0.01,
                    end: 0.09,
                }],
            ),
            Hypothesis::with_words(
                "thing",
                vec![TimedWord {
                    start: 0.0,
                    end: 0.04,
                }],
            ),
        ]);
        stream.answers[1].joins_previous = true;
        let mut state = state(stream);
        let tail = state
            .next_finalized_with(&mut recording, &CancelToken::never(), false)
            .unwrap()
            .unwrap();
        let Some(TranscriptUpdate::Finalized(segment)) = tail.update else {
            panic!("the joined word finishes");
        };
        assert_eq!(segment.as_str(), "something");
        assert_eq!(
            segment.word_spans(),
            Some(std::slice::from_ref(&(160..CHUNK as u64)))
        );
    }

    #[test]
    fn the_flush_segment_never_claims_audio_past_the_recording() {
        let (_directory, mut recording) = recorded(&vec![0_i16; 2 * CHUNK]);
        let (stream, _) = Scripted::new(&["", "", "held back"]);
        let mut state = state(stream);

        let step = state
            .next_finalized_with(&mut recording, &CancelToken::never(), false)
            .expect("the step succeeds")
            .expect("the end delivers");

        let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
            panic!("a segment");
        };
        assert_eq!(segment.as_str(), "held back");
        assert_eq!(segment.source_window(), Some(0..2 * CHUNK as u64));
        assert_eq!(segment.span(), None);
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
            .next_finalized_with(&mut recording, &cancel, false)
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
            .next_finalized_with(&mut recording, &cancel, false)
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
            .next_finalized_with(&mut recording, &CancelToken::never(), false)
            .expect("the step succeeds")
            .expect("the end is a step");

        assert!(step.update.is_none());
        assert!(step.transcript.is_none());
        assert!(
            state
                .next_finalized_with(&mut recording, &CancelToken::never(), false)
                .expect("the step succeeds")
                .is_none()
        );
    }

    #[test]
    fn an_empty_recording_is_the_capture_error_and_feeds_nothing() {
        let (_directory, mut recording) = recorded(&[]);
        let (stream, fed) = Scripted::new(&["never"]);
        let mut state = state(stream);

        let outcome = state.next_finalized_with(&mut recording, &CancelToken::never(), false);

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

        let outcome = state.next_finalized_with(&mut recording, &cancel, false);

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
        assert!(fed.lock().expect("no poison").is_empty());
    }
}
