//! Reads one recording's finalized segments and places each in the
//! recording, freeing the audio no reader needs any more.

use crate::audio::RecordingReader;
use crate::transcription::{
    Layer, Operation, Transcriber, TranscriptSegment, TranscriptUpdate, TranscriptionError,
};

/// The next finalized segment, or `None` once the recording ends. Decoding,
/// placing, and freeing the audio behind the decoder have to happen in that
/// order, so the order lives here rather than in every caller.
///
/// Freeing is safe without asking anyone: a second reader of the same
/// recording holds its own fork and frees its own audio.
pub fn next_heard<T: Transcriber, L: Layer>(
    operation: &mut Operation<'_, T, L>,
    recording: &mut RecordingReader,
) -> Result<Option<Heard>, TranscriptionError> {
    loop {
        match next_heard_update(operation, recording, false)? {
            Some(HeardUpdate::Finalized(heard)) => return Ok(Some(heard)),
            Some(HeardUpdate::Provisional { .. }) => {}
            None => return Ok(None),
        }
    }
}

/// [`next_heard`], with the decoder's provisional text between finalized
/// segments when `previews` asks for it.
pub fn next_heard_update<T: Transcriber, L: Layer>(
    operation: &mut Operation<'_, T, L>,
    recording: &mut RecordingReader,
    previews: bool,
) -> Result<Option<HeardUpdate>, TranscriptionError> {
    let segment = match operation.next(recording, previews)? {
        Some(TranscriptUpdate::Finalized(segment)) => segment,
        Some(TranscriptUpdate::Provisional {
            text,
            source_window,
        }) => {
            return Ok(Some(HeardUpdate::Provisional {
                text,
                source_window,
            }));
        }
        None => return Ok(None),
    };
    let span = (segment.word_spans().is_some() || segment.source_window().is_some())
        .then(|| segment.span())
        .flatten();
    let window = span
        .clone()
        .or_else(|| segment.source_window())
        .unwrap_or(0..operation.read_to());
    recording.release_before(operation.retained_from());
    Ok(Some(HeardUpdate::Finalized(Heard {
        segment,
        span,
        window,
    })))
}

pub enum HeardUpdate {
    Finalized(Heard),
    Provisional {
        text: String,
        source_window: Option<std::ops::Range<u64>>,
    },
}

/// One finalized segment, placed in its recording.
pub struct Heard {
    pub segment: TranscriptSegment,
    /// Where the segment's words sit in the recording's stream, when the
    /// engine timed them. `None` otherwise — the decoder's own position runs
    /// ahead of the words it has emitted, so it cannot stand in for theirs.
    pub span: Option<std::ops::Range<u64>>,
    /// The audio the segment came from: its words' span when timed, else
    /// the decoder's source window.
    pub window: std::ops::Range<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::CANONICAL_SAMPLE_RATE;

    #[test]
    fn preview_reads_preserve_finalized_segments_and_completion() {
        use crate::audio::testing::{Pace, paced};
        use crate::cancel::CancelToken;
        use crate::transcription::{
            Cadence, Hypothesis, Language, TimedWord, Transcription, TranscriptionAudio,
        };
        use std::sync::Mutex;

        struct Script {
            pace: Pace,
            calls: Mutex<usize>,
            timed: bool,
        }
        impl Transcriber for Script {
            fn validate_language(
                &self,
                _: &Language,
                _: &CancelToken,
            ) -> Result<(), TranscriptionError> {
                Ok(())
            }
            fn transcribe_with(
                &self,
                audio: TranscriptionAudio<'_>,
                _: &Language,
                _: &CancelToken,
            ) -> Result<Hypothesis, TranscriptionError> {
                let mut calls = self.calls.lock().expect("no poison");
                let text = ["one two", "one two three", "three four", "three four five"][*calls];
                *calls += 1;
                self.pace.advance();
                if self.timed {
                    let duration = audio.require_samples("script")?.len() as f32
                        / CANONICAL_SAMPLE_RATE as f32;
                    let words = text.split_whitespace().count();
                    Ok(Hypothesis::with_words(
                        text,
                        (0..words)
                            .map(|index| TimedWord {
                                start: duration * index as f32 / words as f32,
                                end: duration * (index + 1) as f32 / words as f32,
                            })
                            .collect(),
                    ))
                } else {
                    Ok(Hypothesis::new(text))
                }
            }
        }
        let run = |previews, timed| {
            let (_directory, mut recording, pace) = paced(vec![vec![8192_i16; 1600]; 4]);
            let transcription = Transcription::agreement(
                Cadence::from_millis(100, 200),
                Script {
                    pace,
                    calls: Mutex::new(0),
                    timed,
                },
            );
            let mut operation = transcription.start(&Language::Automatic).expect("starts");
            let mut rows = Vec::new();
            let mut preview_count = 0;
            loop {
                let update = if previews {
                    next_heard_update(&mut operation, &mut recording, true)
                } else {
                    next_heard(&mut operation, &mut recording)
                        .map(|heard| heard.map(HeardUpdate::Finalized))
                }
                .expect("reads");
                match update {
                    Some(HeardUpdate::Finalized(heard)) => {
                        rows.push((heard.segment.into_text(), heard.window));
                    }
                    Some(HeardUpdate::Provisional { .. }) => preview_count += 1,
                    None => break,
                }
            }
            let final_text = operation
                .finish(&mut recording)
                .expect("transcript")
                .into_text();
            (rows, final_text, preview_count)
        };
        for timed in [false, true] {
            let (baseline, baseline_text, _) = run(false, timed);
            let (previewed, preview_text, count) = run(true, timed);
            assert!(count > 0);
            assert_eq!(previewed, baseline);
            assert_eq!(preview_text, baseline_text);
        }
    }

    #[test]
    fn delayed_untimed_words_keep_the_entire_decode_window() {
        use crate::audio::testing::{Pace, paced};
        use crate::cancel::CancelToken;
        use crate::transcription::{
            Cadence, Hypothesis, Language, Transcription, TranscriptionAudio,
        };
        use std::sync::Mutex;

        struct Script {
            pace: Pace,
            calls: Mutex<usize>,
        }
        impl Transcriber for Script {
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
                let mut calls = self.calls.lock().unwrap();
                let answers = [
                    "one two",
                    "one two",
                    "one two three four",
                    "one two three four",
                    "one two three four five six",
                    "one two three four five six",
                    "one two three four five six guest phrase",
                    "one two three four five six guest phrase",
                ];
                let text = answers[*calls];
                *calls += 1;
                self.pace.advance();
                Ok(Hypothesis::new(text))
            }
        }
        let second = CANONICAL_SAMPLE_RATE as usize;
        let (_directory, mut recording, pace) = paced(vec![vec![8192_i16; second]; 8]);
        let transcription = Transcription::agreement(
            Cadence::from_millis(1_000, 20_000),
            Script {
                pace,
                calls: Mutex::new(0),
            },
        );
        let mut operation = transcription.start(&Language::Automatic).unwrap();
        let mut heard = Vec::new();
        while let Some(segment) = next_heard(&mut operation, &mut recording).unwrap() {
            heard.push(segment);
        }
        assert_eq!(
            heard
                .iter()
                .map(|heard| heard.segment.as_str())
                .collect::<Vec<_>>(),
            ["one two", "three four", "five six", "guest phrase"]
        );
        let guest = heard.last().unwrap();
        assert_eq!(guest.segment.span(), None);
        assert_eq!(guest.segment.word_spans(), None);
        assert_eq!(guest.segment.source_window(), Some(0..8 * second as u64));
        assert_eq!(guest.window, 0..8 * second as u64);
    }
}
