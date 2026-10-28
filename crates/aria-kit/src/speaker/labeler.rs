//! The per-segment labeling policy a capture loop applies — speech gate,
//! embed, label, carry — shared by listen and the diarize bench so every
//! tunable lives in one place.

use std::time::Duration;

use super::{Diarizer, Embedder, Label};
use crate::audio::{RecordingReader, canonical_samples};
use crate::primitives::error_chain;
use crate::transcription::{
    Layer, Operation, Transcriber, TranscriptSegment, TranscriptUpdate, TranscriptionError,
};
use crate::voice_activity::{SileroVad, Speech};

/// Minimum detected speech per window; shorter windows are discarded.
const MIN_SPEECH_SAMPLES: usize = canonical_samples(Duration::from_millis(200)) as usize;
/// A gap this long starts a speaker turn instead of carrying the previous label.
const TURN_GAP_SAMPLES: u64 = canonical_samples(crate::voice_activity::UTTERANCE_GAP);

pub struct SegmentLabeler<E> {
    vad: Option<SileroVad>,
    diarizer: Option<Diarizer<E>>,
    previous: Option<Label>,
    previous_speech: usize,
    /// Silence the previous window ended in, so a pause split across two
    /// windows still counts as one.
    previous_trailing: usize,
}

pub enum Verdict {
    /// Music or noise: the segment's text is not worth keeping.
    Drop,
    Keep {
        /// The live, tentative speaker. A later pass over the whole session
        /// settles the final one.
        label: Option<Label>,
        embedding: Option<Vec<f32>>,
    },
}

impl<E: Embedder> SegmentLabeler<E> {
    pub fn new(vad: Option<SileroVad>, diarizer: Option<Diarizer<E>>) -> Self {
        Self {
            vad,
            diarizer,
            previous: None,
            previous_speech: 0,
            previous_trailing: 0,
        }
    }

    /// Whether a speaker model loaded — without one every label is `None`
    /// and a final `relabel` has nothing to say.
    pub fn diarizes(&self) -> bool {
        self.diarizer.is_some()
    }

    /// Replaces the enrolled profiles the diarizer judges against. Without a
    /// speaker model there is nothing to hold them.
    pub fn set_enrolled(&mut self, enrolled: Vec<super::Profile>) {
        if let Some(diarizer) = self.diarizer.as_mut() {
            diarizer.set_enrolled(enrolled);
        }
    }

    /// `silence_before` is the samples of gap between the previous segment's
    /// audio and this one's; zero whenever the timing is unknown. With a
    /// speech gate the window's own silence counts too, since an engine's
    /// timings can stretch a word over a pause.
    pub fn observe(&mut self, window: &[f32], silence_before: u64) -> Verdict {
        let mut gap = silence_before as usize;
        let speech = self.vad.as_mut().map(|vad| {
            vad.speech(window).unwrap_or_else(|error| {
                tracing::warn!(
                    error = %error_chain(&error),
                    "voice-activity inference failed; audio passes ungated"
                );
                Speech {
                    regions: std::iter::once(0..window.len()).collect(),
                    samples: window.to_vec(),
                }
            })
        });
        if let Some(speech) = &speech {
            let regions = &speech.regions;
            let leading = regions.first().map_or(window.len(), |run| run.start);
            gap += self.previous_trailing + leading;
            self.previous_trailing = regions.last().map_or(0, |run| window.len() - run.end);
        }
        let paused = gap >= TURN_GAP_SAMPLES as usize;
        let speech = speech.map(|speech| speech.samples);
        if let Some(speech) = &speech {
            // Windows lag the words at a boundary, so a segment survives on
            // its own window or the one before it.
            let heard = speech.len().max(self.previous_speech);
            self.previous_speech = speech.len();
            if heard < MIN_SPEECH_SAMPLES {
                // Without this a broken gate and a quiet room look the same
                // from outside: text simply stops appearing.
                tracing::debug!(
                    speech_samples = heard,
                    window_samples = window.len(),
                    "the speech gate dropped a segment"
                );
                return Verdict::Drop;
            }
        }
        let voice = speech.as_deref().unwrap_or(window);
        let embedding = self
            .diarizer
            .as_mut()
            .and_then(|diarizer| embed_or_skip(diarizer, voice));
        let label = match (&embedding, self.diarizer.as_mut()) {
            (Some(embedding), Some(diarizer)) => {
                Some(diarizer.label(embedding, self.previous.as_ref()))
            }
            // No embedding to judge by. Mid-utterance the voice has not
            // changed, so it carries. After a pause it may have, and a wrong
            // carry here glues the segment to the previous speaker's
            // paragraph for good — so leave it open for the final pass.
            _ if paused => None,
            _ => self.previous.clone(),
        };
        self.previous.clone_from(&label);
        Verdict::Keep { label, embedding }
    }

    /// The final labels for the session's paragraphs; all `None` without a
    /// speaker model.
    pub fn relabel(&self, paragraphs: &[Option<Vec<f32>>]) -> Vec<Option<Label>> {
        match &self.diarizer {
            Some(diarizer) => diarizer.relabel(paragraphs),
            None => vec![None; paragraphs.len()],
        }
    }
}

/// A model failure leaves the window unlabeled, as a quiet window is: one
/// broken embedding must not end the session.
pub(super) fn embed_or_skip<E: Embedder>(
    diarizer: &mut Diarizer<E>,
    window: &[f32],
) -> Option<Vec<f32>> {
    diarizer.embed(window).unwrap_or_else(|error| {
        tracing::warn!(error = %error_chain(&error), "speaker embedding failed");
        None
    })
}

/// Walks the finalized segments of one recording: cuts each segment's audio
/// out of the window, labels it, and frees what no reader needs any more.
pub struct SegmentReader<E> {
    labeler: SegmentLabeler<E>,
    heard_through: Option<u64>,
    warned_untimed: bool,
}

impl<E: Embedder> SegmentReader<E> {
    pub fn new(labeler: SegmentLabeler<E>) -> Self {
        Self {
            labeler,
            heard_through: None,
            warned_untimed: false,
        }
    }

    pub fn labeler(&self) -> &SegmentLabeler<E> {
        &self.labeler
    }

    /// A recording starts at zero; pause gaps cannot cross recordings.
    pub fn rewind(&mut self) {
        self.heard_through = None;
    }

    pub fn labeler_mut(&mut self) -> &mut SegmentLabeler<E> {
        &mut self.labeler
    }

    /// The next finalized segment and whose voice it was, or `None` once the
    /// recording ends. Decoding, labeling, and freeing
    /// the audio behind the decoder have to happen in that order, so the
    /// order lives here rather than in every caller.
    ///
    /// Freeing is safe without asking anyone: a second reader of the same
    /// recording holds its own fork, with its own window and its own floor.
    pub fn next<T: Transcriber, L: Layer>(
        &mut self,
        operation: &mut Operation<'_, T, L>,
        recording: &mut RecordingReader,
    ) -> Result<Option<Heard>, TranscriptionError> {
        let segment = loop {
            match operation.next(recording, false)? {
                Some(TranscriptUpdate::Finalized(segment)) => break segment,
                Some(TranscriptUpdate::Provisional { .. }) => {}
                None => return Ok(None),
            }
        };
        let silence = self.silence_before(&segment);
        let span = (segment.word_spans().is_some() || segment.source_window().is_some())
            .then(|| segment.span())
            .flatten();
        let window = span
            .clone()
            .or_else(|| segment.source_window())
            .unwrap_or(0..operation.read_to());
        let verdict = if window.start < recording.held_from() || window.end > recording.held_to() {
            Verdict::Keep {
                label: None,
                embedding: None,
            }
        } else {
            self.labeler
                .observe(recording.span(window.clone()), silence)
        };
        // The labeler is the last reader of a segment's audio, so what the
        // decoder no longer needs is now free. Without this a session's
        // memory grows with its length.
        recording.release_before(operation.retained_from());
        Ok(Some(Heard {
            segment,
            verdict,
            span,
            window,
        }))
    }

    /// Gap between timed segments; returns zero when word timings are unavailable.
    fn silence_before(&mut self, segment: &TranscriptSegment) -> u64 {
        let timed = segment.word_spans().is_some() || segment.source_window().is_some();
        let Some(span) = timed.then(|| segment.span()).flatten() else {
            if !self.warned_untimed {
                self.warned_untimed = true;
                tracing::info!(
                    "the engine reports no word timings; speaker turns are not detected by pause"
                );
            }
            return 0;
        };
        let gap = self
            .heard_through
            .map_or(0, |end| span.start.saturating_sub(end));
        self.heard_through = Some(span.end);
        gap
    }
}

/// One finalized segment, as the labeler judged it.
pub struct Heard {
    pub segment: TranscriptSegment,
    pub verdict: Verdict,
    /// Where the segment's words sit in the recording's stream, when the
    /// engine timed them. `None` otherwise — the decoder's own position runs
    /// ahead of the words it has emitted, so it cannot stand in for theirs.
    pub span: Option<std::ops::Range<u64>>,
    /// Possible source audio, used for admission when words are untimed.
    /// Its start is where the transcript timestamps the segment.
    pub window: std::ops::Range<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::CANONICAL_SAMPLE_RATE;
    use crate::speaker::SpeakerError;

    struct Broken;

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
        let mut reader = SegmentReader::new(SegmentLabeler::<Broken>::new(None, None));
        let mut heard = Vec::new();
        while let Some(segment) = reader.next(&mut operation, &mut recording).unwrap() {
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

    impl Embedder for Broken {
        fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
            Err(SpeakerError::Inference {
                backend: "broken",
                source: crate::backend::BackendError::engine(std::io::Error::other("no model")),
            })
        }

        fn dimension(&self) -> usize {
            2
        }
    }

    #[test]
    fn a_failing_speaker_model_keeps_the_segment_unlabeled() {
        let mut labeler = SegmentLabeler::new(None, Some(Diarizer::new(Broken, Vec::new())));

        let verdict = labeler.observe(&[0.0; 16_000], TURN_GAP_SAMPLES);

        assert!(matches!(
            verdict,
            Verdict::Keep {
                label: None,
                embedding: None
            }
        ));
    }
}
