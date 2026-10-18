//! The per-segment labeling policy a capture loop applies — speech gate,
//! embed, label, carry — shared by dictation, listen, and the diarize bench
//! so every tunable lives in one place.

use std::ops::Range;
use std::time::Duration;

use super::{Diarizer, Embedder};
use crate::audio::{RecordingReader, canonical_duration, canonical_samples};
use crate::transcription::{
    Layer, Operation, Transcriber, TranscriptSegment, TranscriptUpdate, TranscriptionError,
};
use crate::voice_activity::{SileroVad, Speech};

/// Minimum detected speech per window; shorter windows are discarded.
const MIN_SPEECH_SAMPLES: usize = canonical_samples(Duration::from_millis(200)) as usize;
/// A gap this long starts a speaker turn instead of carrying the previous label.
const TURN_GAP_SAMPLES: u64 = canonical_samples(Duration::from_millis(700));
/// How much of a turn the own-voice check listens to.
const TURN_VOICE_SAMPLES: usize = canonical_samples(Duration::from_secs(8)) as usize;
/// Minimum turn audio for owner matching. Earlier speech is kept without a verdict,
/// so another speaker's opening words can pass through.
const TURN_JUDGED_SAMPLES: usize = canonical_samples(Duration::from_secs(3)) as usize;
/// Owner-match threshold below the naming gate to allow short turns.
/// Calibrated on one voice and one synthetic alternative.
pub const OWN_VOICE_THRESHOLD: f32 = 0.4;

pub struct SegmentLabeler<E> {
    vad: Option<SileroVad>,
    diarizer: Option<Diarizer<E>>,
    previous: Option<String>,
    previous_speech: usize,
    /// Silence the previous window ended in, so a pause split across two
    /// windows still counts as one.
    previous_trailing: usize,
    /// Whether segments in a voice that is not the owner's are kept out.
    only_own_voice: bool,
    /// The voice of the turn so far, and its embedding.
    turn: Vec<f32>,
    turn_voice: Option<Vec<f32>>,
}

pub enum Verdict {
    /// Music or noise: the segment's text is not worth keeping.
    Drop,
    /// A voice the labeler was told to keep out: not the owner's.
    Stranger,
    Keep {
        /// The live, tentative speaker. A later pass over the whole session
        /// settles the final one.
        label: Option<String>,
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
            only_own_voice: false,
            turn: Vec::new(),
            turn_voice: None,
        }
    }

    /// Whether a speaker model loaded — without one every label is `None`
    /// and a final `relabel` has nothing to say.
    pub fn diarizes(&self) -> bool {
        self.diarizer.is_some()
    }

    /// Keeps out segments in a voice that is not the owner's, judged over the
    /// turn so far. Without a speaker model nothing can be judged and
    /// everything stays.
    pub fn set_only_own_voice(&mut self, only_own_voice: bool) {
        // A speaker model with nobody owning a profile judges nothing, so the
        // setting quietly keeps every voice. Say so: silence here looks like
        // the filter working.
        if only_own_voice && self.diarizer.as_ref().is_some_and(|d| d.owners() == 0) {
            tracing::warn!("no owner is enrolled; own-voice filtering keeps every voice");
        }
        self.only_own_voice = only_own_voice;
    }

    pub fn only_own_voice(&self) -> bool {
        self.only_own_voice
    }

    /// Whether a stranger's voice is actually kept out: the setting is on
    /// and there is a model with an owner to judge by. The setting alone
    /// promises nothing.
    pub fn judges_own_voice(&self) -> bool {
        self.only_own_voice && self.diarizer.as_ref().is_some_and(|d| d.owners() > 0)
    }

    /// Replaces the enrolled profiles the diarizer judges against. Without a
    /// speaker model there is nothing to hold them.
    pub fn set_enrolled(&mut self, enrolled: Vec<super::Profile>) {
        if let Some(diarizer) = self.diarizer.as_mut() {
            diarizer.set_enrolled(enrolled);
        }
    }

    /// Whether the turn heard so far is in the owner's voice. `None` until
    /// enough of a voice could be judged, and always without a speaker
    /// model.
    fn own_voice(&self) -> Option<bool> {
        self.judge(self.turn.len(), self.turn_voice.as_deref()?)
    }

    /// The stretches of a finished recording in the owner's voice, joined in
    /// order: what a decode that hears everything at once should be handed.
    /// Turns are cut and judged as the live path cuts and judges them. `None`
    /// when everything stays: not keeping strangers out, or no speaker model
    /// to judge by.
    pub fn own_voice_audio(&mut self, samples: &[f32]) -> Option<Vec<f32>> {
        if !self.only_own_voice {
            return None;
        }
        self.diarizer.as_ref()?;
        let regions = match self.vad.as_mut() {
            Some(vad) => vad.speech_regions(samples).unwrap_or_else(|error| {
                tracing::warn!(%error, "voice-activity inference failed; audio passes ungated");
                std::iter::once(0..samples.len()).collect()
            }),
            None => std::iter::once(0..samples.len()).collect(),
        };
        let mut kept = Vec::new();
        for turn in turns(&regions) {
            let voice: Vec<f32> = turn
                .iter()
                .flat_map(|region| samples[region.clone()].iter().copied())
                .collect();
            let heard = &voice[voice.len().saturating_sub(TURN_VOICE_SAMPLES)..];
            let embedding = self
                .diarizer
                .as_mut()
                .and_then(|diarizer| embed_or_skip(diarizer, heard));
            let stranger = embedding
                .as_deref()
                .and_then(|embedding| self.judge(voice.len(), embedding))
                == Some(false);
            if !stranger {
                kept.extend_from_slice(&voice);
            }
        }
        Some(kept)
    }

    /// Whether a turn is in the owner's voice, from its length and the
    /// embedding of what it holds. `None` under the judging minimum.
    fn judge(&self, turn_samples: usize, voice: &[f32]) -> Option<bool> {
        let diarizer = self.diarizer.as_ref()?;
        if turn_samples < TURN_JUDGED_SAMPLES {
            return None;
        }
        let similarity = diarizer.owner_similarity(voice)?;
        tracing::debug!(
            similarity,
            turn_ms = canonical_duration(turn_samples as u64).as_millis(),
            "own-voice check"
        );
        Some(similarity >= OWN_VOICE_THRESHOLD)
    }

    /// `silence_before` is the samples of gap between the previous segment's
    /// audio and this one's; zero whenever the timing is unknown. With a
    /// speech gate the window's own silence counts too, since an engine's
    /// timings can stretch a word over a pause.
    pub fn observe(&mut self, window: &[f32], silence_before: u64) -> Verdict {
        let mut gap = silence_before as usize;
        let speech = self.vad.as_mut().map(|vad| {
            vad.speech(window).unwrap_or_else(|error| {
                tracing::warn!(%error, "voice-activity inference failed; audio passes ungated");
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
        self.hear_turn(voice, paused);
        let embedding = self
            .diarizer
            .as_mut()
            .and_then(|diarizer| embed_or_skip(diarizer, voice));
        let label = match (&embedding, self.diarizer.as_mut()) {
            (Some(embedding), Some(diarizer)) => {
                Some(diarizer.label(embedding, self.previous.as_deref()))
            }
            // No embedding to judge by. Mid-utterance the voice has not
            // changed, so it carries. After a pause it may have, and a wrong
            // carry here glues the segment to the previous speaker's
            // paragraph for good — so leave it open for the final pass.
            _ if paused => None,
            _ => self.previous.clone(),
        };
        self.previous.clone_from(&label);
        if self.only_own_voice && self.own_voice() == Some(false) {
            return Verdict::Stranger;
        }
        Verdict::Keep { label, embedding }
    }

    /// Adds a segment's voice to the turn, opening a new turn after a pause,
    /// and embeds the turn as a whole.
    fn hear_turn(&mut self, voice: &[f32], paused: bool) {
        let Some(diarizer) = self.diarizer.as_mut() else {
            return;
        };
        if paused {
            self.turn.clear();
        }
        self.turn.extend_from_slice(voice);
        let excess = self.turn.len().saturating_sub(TURN_VOICE_SAMPLES);
        self.turn.drain(..excess);
        self.turn_voice = embed_or_skip(diarizer, &self.turn);
    }

    /// The final labels for the session's paragraphs; all `None` without a
    /// speaker model.
    pub fn relabel(&self, paragraphs: &[Option<Vec<f32>>]) -> Vec<Option<String>> {
        match &self.diarizer {
            Some(diarizer) => diarizer.relabel(paragraphs),
            None => vec![None; paragraphs.len()],
        }
    }
}

/// A model failure leaves the window unlabeled, as a quiet window is: one
/// broken embedding must not end the session.
fn embed_or_skip<E: Embedder>(diarizer: &mut Diarizer<E>, window: &[f32]) -> Option<Vec<f32>> {
    diarizer.embed(window).unwrap_or_else(|error| {
        tracing::warn!(%error, "speaker embedding failed");
        None
    })
}

/// Walks the finalized segments of one recording: cuts each segment's audio
/// out of the window, labels it, and frees what no reader needs any more.
///
/// The buffer is marked before each decode, because a segment's audio
/// ends where the buffer stood before the call that emitted it — agreement
/// lags one decode behind the newest stride.
pub struct SegmentReader<E> {
    labeler: SegmentLabeler<E>,
    mark: u64,
    before: u64,
    awaiting_finalized: bool,
    heard_through: Option<u64>,
    warned_untimed: bool,
}

impl<E: Embedder> SegmentReader<E> {
    pub fn new(labeler: SegmentLabeler<E>) -> Self {
        Self {
            labeler,
            mark: 0,
            before: 0,
            awaiting_finalized: false,
            heard_through: None,
            warned_untimed: false,
        }
    }

    pub fn labeler(&self) -> &SegmentLabeler<E> {
        &self.labeler
    }

    /// Forgets where the last recording stood. A new recording starts at
    /// zero, and a window measured from the old position would clamp to
    /// nothing. The labeler's memory of voices stays.
    pub fn rewind(&mut self) {
        self.mark = 0;
        self.before = 0;
        self.awaiting_finalized = false;
        self.heard_through = None;
    }

    pub fn labeler_mut(&mut self) -> &mut SegmentLabeler<E> {
        &mut self.labeler
    }

    /// The next finalized segment and whose voice it was, or `None` once the
    /// recording ends. Marking the buffer, decoding, labeling, and freeing
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
        match self.read(operation, recording, false)? {
            Some(HeardUpdate::Finalized(heard)) => Ok(Some(heard)),
            Some(HeardUpdate::Provisional(_)) => unreachable!("previews are disabled"),
            None => Ok(None),
        }
    }

    pub fn next_update<T: Transcriber, L: Layer>(
        &mut self,
        operation: &mut Operation<'_, T, L>,
        recording: &mut RecordingReader,
    ) -> Result<Option<HeardUpdate>, TranscriptionError> {
        self.read(operation, recording, true)
    }

    fn read<T: Transcriber, L: Layer>(
        &mut self,
        operation: &mut Operation<'_, T, L>,
        recording: &mut RecordingReader,
        previews: bool,
    ) -> Result<Option<HeardUpdate>, TranscriptionError> {
        if !self.awaiting_finalized {
            self.before = recording.held_to();
            self.awaiting_finalized = true;
        }
        let segment = match operation.next(recording, previews)? {
            Some(TranscriptUpdate::Finalized(segment)) => segment,
            Some(TranscriptUpdate::Provisional(text)) => {
                return Ok(Some(HeardUpdate::Provisional(text)));
            }
            None => {
                self.awaiting_finalized = false;
                return Ok(None);
            }
        };
        self.awaiting_finalized = false;
        let silence = self.silence_before(&segment);
        let span = segment.span();
        // The decode window stands in for audio nobody timed. It is the right
        // audio to judge a voice by, and the wrong thing to call a position:
        // the decoder runs a buffer ahead of the words it has emitted.
        let window = span.clone().unwrap_or(self.mark..self.before);
        let at = window.start;
        self.mark = self.before;
        let verdict = self.labeler.observe(recording.span(window), silence);
        // The labeler is the last reader of a segment's audio, so what the
        // decoder no longer needs is now free. Without this a session's
        // memory grows with its length.
        recording.release_before(operation.retained_from());
        Ok(Some(HeardUpdate::Finalized(Heard {
            segment,
            verdict,
            span,
            at,
        })))
    }

    /// Gap between timed segments; returns zero when word timings are unavailable.
    fn silence_before(&mut self, segment: &TranscriptSegment) -> u64 {
        let Some(span) = segment.span() else {
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

/// Groups speech regions into turns separated by at least `TURN_GAP_SAMPLES`.
fn turns(regions: &[Range<usize>]) -> Vec<Vec<Range<usize>>> {
    let mut turns: Vec<Vec<Range<usize>>> = Vec::new();
    for region in regions {
        let gap = turns
            .last()
            .and_then(|turn| turn.last())
            .map_or(0, |last| region.start.saturating_sub(last.end));
        if turns.is_empty() || gap >= TURN_GAP_SAMPLES as usize {
            turns.push(Vec::new());
        }
        turns
            .last_mut()
            .expect("a turn was just opened")
            .push(region.clone());
    }
    turns
}

pub enum HeardUpdate {
    Finalized(Heard),
    Provisional(String),
}

/// One finalized segment, as the labeler judged it.
pub struct Heard {
    pub segment: TranscriptSegment,
    pub verdict: Verdict,
    /// Where the segment's words sit in the recording's stream, when the
    /// engine timed them. `None` otherwise — the decoder's own position runs
    /// ahead of the words it has emitted, so it cannot stand in for theirs.
    pub span: Option<std::ops::Range<u64>>,
    /// Where the transcript timestamps the segment: the start of
    /// [`Self::span`], or of the decode window when the engine timed nothing.
    pub at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::CANONICAL_SAMPLE_RATE;
    use crate::speaker::SpeakerError;

    #[test]
    fn preview_reads_preserve_finalized_labeling_audio_and_completion() {
        use crate::audio::testing::{Pace, paced};
        use crate::cancel::CancelToken;
        use crate::transcription::{
            Cadence, Hypothesis, Language, TimedWord, Transcription, TranscriptionAudio,
        };
        use std::sync::{Arc, Mutex};

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
        struct Observed(Arc<Mutex<Vec<usize>>>);
        impl Embedder for Observed {
            fn embed(&mut self, window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
                self.0.lock().expect("no poison").push(window.len());
                Ok(Some(vec![1.0, 0.0]))
            }
            fn dimension(&self) -> usize {
                2
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
            let observations = Arc::new(Mutex::new(Vec::new()));
            let mut reader = SegmentReader::new(SegmentLabeler::new(
                None,
                Some(Diarizer::new(
                    Observed(Arc::clone(&observations)),
                    Vec::new(),
                )),
            ));
            let mut rows = Vec::new();
            let mut preview_count = 0;
            loop {
                let update = if previews {
                    reader.next_update(&mut operation, &mut recording)
                } else {
                    reader
                        .next(&mut operation, &mut recording)
                        .map(|heard| heard.map(HeardUpdate::Finalized))
                }
                .expect("reads");
                match update {
                    Some(HeardUpdate::Finalized(heard)) => {
                        assert!(matches!(heard.verdict, Verdict::Keep { .. }));
                        rows.push((
                            heard.segment.into_text(),
                            heard.at,
                            reader.mark,
                            reader.before,
                        ));
                    }
                    Some(HeardUpdate::Provisional(_)) => preview_count += 1,
                    None => break,
                }
            }
            let final_text = operation
                .finish(&mut recording)
                .expect("transcript")
                .into_text();
            let windows = observations.lock().expect("no poison").clone();
            (rows, final_text, preview_count, windows)
        };
        for timed in [false, true] {
            let (baseline, baseline_text, _, baseline_windows) = run(false, timed);
            let (previewed, preview_text, count, preview_windows) = run(true, timed);
            assert!(count > 0);
            assert_eq!(previewed, baseline);
            assert_eq!(preview_text, baseline_text);
            assert_eq!(preview_windows, baseline_windows);
        }
    }

    const OWN: [f32; 2] = [1.0, 0.0];
    const OTHER: [f32; 2] = [0.0, 1.0];

    /// Hears one fixed voice in every window.
    struct Voice([f32; 2]);

    impl Embedder for Voice {
        fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
            Ok(Some(self.0.to_vec()))
        }

        fn dimension(&self) -> usize {
            2
        }
    }

    /// A model that fails on every window.
    struct Broken;

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

    fn labeler(voice: [f32; 2]) -> SegmentLabeler<Voice> {
        let enrolled = vec![super::super::enrolled_voice(
            "alex",
            super::super::Role::Owner,
            OWN.to_vec(),
        )];
        let mut labeler = SegmentLabeler::new(None, Some(Diarizer::new(Voice(voice), enrolled)));
        labeler.set_only_own_voice(true);
        labeler
    }

    #[test]
    fn a_stranger_is_kept_out_only_when_asked() {
        let mut stranger = labeler(OTHER);
        stranger.observe(&[0.0; 48_000], 0);
        assert!(matches!(
            stranger.observe(&[0.0; 1_600], 0),
            Verdict::Stranger
        ));

        stranger.set_only_own_voice(false);
        assert!(matches!(
            stranger.observe(&[0.0; 1_600], 0),
            Verdict::Keep { .. }
        ));
    }

    #[test]
    fn the_turn_is_judged_once_enough_of_it_was_heard() {
        let mut own = labeler(OWN);
        own.observe(&[0.0; 32_000], 0);
        assert_eq!(own.own_voice(), None, "two seconds are too little to judge");
        own.observe(&[0.0; 16_000], 0);
        assert_eq!(own.own_voice(), Some(true));

        let mut other = labeler(OTHER);
        other.observe(&[0.0; 48_000], 0);
        assert_eq!(other.own_voice(), Some(false));
    }

    #[test]
    fn a_pause_opens_a_new_turn() {
        let mut labeler = labeler(OTHER);
        labeler.observe(&[0.0; 48_000], 0);
        assert_eq!(labeler.own_voice(), Some(false));

        labeler.observe(&[0.0; 16_000], TURN_GAP_SAMPLES);
        assert_eq!(labeler.own_voice(), None, "the new turn is a second old");
    }

    #[test]
    fn a_whole_recording_keeps_the_enrolled_voice_and_drops_a_stranger() {
        let audio = vec![0.0; 64_000];

        let kept = labeler(OWN).own_voice_audio(&audio);
        assert_eq!(kept.map(|kept| kept.len()), Some(64_000));

        let kept = labeler(OTHER).own_voice_audio(&audio);
        assert_eq!(kept.map(|kept| kept.len()), Some(0));

        let mut everyone = labeler(OTHER);
        everyone.set_only_own_voice(false);
        assert!(everyone.own_voice_audio(&audio).is_none());
    }

    #[test]
    fn a_recording_too_short_to_judge_is_kept_on_trust() {
        let kept = labeler(OTHER).own_voice_audio(&[0.0; 16_000]);
        assert_eq!(kept.map(|kept| kept.len()), Some(16_000));
    }

    #[test]
    fn regions_a_turn_gap_apart_are_separate_turns() {
        let gap = TURN_GAP_SAMPLES as usize;
        let grouped = turns(&[0..100, 200..300, 300 + gap..400 + gap]);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0], [0..100, 200..300]);
        assert_eq!(grouped[1].len(), 1);
        assert_eq!(grouped[1][0], 300 + gap..400 + gap);
    }

    #[test]
    fn without_a_speaker_model_no_turn_is_judged() {
        let mut labeler = SegmentLabeler::<Voice>::new(None, None);
        labeler.observe(&[0.0; 48_000], 0);

        assert_eq!(labeler.own_voice(), None);
    }
}
