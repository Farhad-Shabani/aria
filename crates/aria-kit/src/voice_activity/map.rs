//! Where anyone was speaking in one recording, recorded frame by frame from
//! one [`Endpointer`] run, so every reader answers from the same detector.
//!
//! Positions are stream samples. Frames start where the detector starts
//! and follow each other without gaps, so a frame is named by where it
//! starts or ends.

use std::ops::Range;
use std::time::Duration;

use super::{Boundary, Endpointer, UTTERANCE_GAP, VoiceActivityDetector, VoiceActivityError};
use crate::audio::{SilenceDuration, canonical_samples, pcm16_from_f32};

/// A silence inside an utterance at least this long is a pause: a gap a
/// voice could take over across. Shorter ones fall between syllables.
const PAUSE_SAMPLES: u64 = canonical_samples(Duration::from_millis(150));
/// The newest pauses of an utterance a reader is shown. A reader looks back
/// a second or two, which holds fewer than this.
const PAUSES_KEPT: usize = 8;

/// Whether more of the recording is still to come.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Progress {
    #[default]
    Ongoing,
    /// The whole recording was heard.
    Finished,
    /// Hearing failed: nothing after `reached` can be told apart.
    Broken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Utterance {
    start: u64,
    /// The end of the frame that completed its trailing silence.
    end: Option<u64>,
}

/// A run of the trailing-silence clock inside an utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Quiet {
    start: u64,
    end: Option<u64>,
    /// Speech stopped the clock, rather than the utterance ending.
    resumed: bool,
}

/// Where anyone was speaking, as far as the detector has scored.
#[derive(Debug, Clone, Default)]
pub struct SpeechMap {
    frame_samples: usize,
    /// Where the first frame starts.
    from: u64,
    reached: u64,
    utterances: Vec<Utterance>,
    quiet: Vec<Quiet>,
    /// Frames scored as speech, joined where they touch.
    speech: Vec<Range<u64>>,
    /// Speech in what was forgotten, so counts from the start stay whole.
    forgotten_speech: u64,
    progress: Progress,
}

impl SpeechMap {
    fn new(frame_samples: usize, from: u64) -> Self {
        Self {
            frame_samples,
            from,
            reached: from,
            ..Self::default()
        }
    }

    pub(crate) fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// The end of the last frame scored.
    pub fn reached(&self) -> u64 {
        self.reached
    }

    /// The start of the frame that holds `at`.
    pub(crate) fn frame_start(&self, at: u64) -> u64 {
        let frame = self.frame_samples.max(1) as u64;
        self.from + at.saturating_sub(self.from) / frame * frame
    }

    /// The detector failed: nothing after `reached` can be told apart.
    pub fn is_broken(&self) -> bool {
        self.progress == Progress::Broken
    }

    /// Whether the detector heard the whole recording, so nothing after
    /// `reached` is still to come.
    pub fn is_finished(&self) -> bool {
        self.progress == Progress::Finished
    }

    /// Marks the map broken where it stands, as when its detector stopped
    /// without saying why.
    pub fn break_off(&mut self) {
        self.progress = Progress::Broken;
    }

    /// Whether an utterance opened on the frame starting at `at`.
    pub(crate) fn opens_at(&self, at: u64) -> bool {
        self.utterances
            .iter()
            .rev()
            .take_while(|utterance| utterance.start >= at)
            .any(|utterance| utterance.start == at)
    }

    /// Whether an utterance closed on the frame ending at `at`.
    pub(crate) fn closes_at(&self, at: u64) -> bool {
        self.utterances
            .iter()
            .rev()
            .take_while(|utterance| utterance.end.is_none_or(|end| end >= at))
            .any(|utterance| utterance.end == Some(at))
    }

    /// How much silence the open utterance ends in, once the frame ending
    /// at `at` is scored. Nothing while speech goes on, or outside an
    /// utterance.
    pub(crate) fn trailing_silence(&self, at: u64) -> u64 {
        self.quiet
            .iter()
            .rev()
            .find(|quiet| quiet.start < at)
            .filter(|quiet| match quiet.end {
                None => true,
                // Speech scored at `end` is what stops the clock, so the
                // frame ending there still ends in silence.
                Some(end) if quiet.resumed => at <= end,
                Some(end) => at < end,
            })
            .map_or(0, |quiet| at - quiet.start)
    }

    /// The pauses inside the utterance still open once the frame ending at
    /// `at` is scored, newest first.
    pub(crate) fn pauses_before(&self, at: u64) -> impl Iterator<Item = Range<u64>> + '_ {
        let open = self
            .utterances
            .iter()
            .rev()
            .find(|utterance| utterance.start < at)
            .filter(|utterance| utterance.end.is_none_or(|end| at < end))
            .map(|utterance| utterance.start);
        self.quiet
            .iter()
            .rev()
            .filter_map(|quiet| match quiet.end {
                Some(end) if quiet.resumed && end - quiet.start >= PAUSE_SAMPLES => {
                    Some(quiet.start..end)
                }
                _ => None,
            })
            .filter(move |pause| pause.end < at)
            .take_while(move |pause| open.is_some_and(|start| pause.start >= start))
            .take(PAUSES_KEPT)
    }

    /// Samples of speech scored before `at`.
    pub fn speech_before(&self, at: u64) -> u64 {
        self.forgotten_speech + self.speech(&(0..at))
    }

    pub(crate) fn forget_before(&mut self, at: u64) {
        let mut forgotten = 0;
        self.speech.retain_mut(|run| {
            if run.end <= at {
                forgotten += run.end - run.start;
                return false;
            }
            if run.start < at {
                forgotten += at - run.start;
                run.start = at;
            }
            true
        });
        self.forgotten_speech += forgotten;
        self.utterances
            .retain(|utterance| utterance.end.is_none_or(|end| end > at));
        self.quiet
            .retain(|quiet| quiet.end.is_none_or(|end| end > at));
    }

    /// Samples of speech scored inside `span`. Speech already forgotten does
    /// not count.
    pub fn speech(&self, span: &Range<u64>) -> u64 {
        self.speech
            .iter()
            .map(|run| {
                run.end
                    .min(span.end)
                    .saturating_sub(run.start.max(span.start))
            })
            .sum()
    }

    fn add_speech(&mut self, frame: Range<u64>) {
        match self.speech.last_mut() {
            Some(run) if run.end == frame.start => run.end = frame.end,
            _ => self.speech.push(frame),
        }
    }

    fn close_quiet(&mut self, end: u64, resumed: bool) {
        if let Some(quiet) = self.quiet.last_mut().filter(|quiet| quiet.end.is_none()) {
            quiet.end = Some(end);
            quiet.resumed = resumed;
        }
    }
}

/// Scores a recording's frames and records them in a [`SpeechMap`].
pub struct SpeechDetector<V> {
    endpointer: Endpointer<V>,
    frame: Vec<i16>,
    map: SpeechMap,
}

impl<V: VoiceActivityDetector> SpeechDetector<V> {
    /// A detector for one recording. The first sample heard sits at `at`.
    pub fn new(vad: V, at: u64) -> Self {
        let gap = SilenceDuration::new(UTTERANCE_GAP).expect("the utterance gap is a silence");
        Self::with_gap(vad, gap, at)
    }

    fn with_gap(vad: V, utterance_gap: SilenceDuration, at: u64) -> Self {
        Self {
            endpointer: Endpointer::new(vad, utterance_gap),
            frame: Vec::with_capacity(V::FRAME_SAMPLES),
            map: SpeechMap::new(V::FRAME_SAMPLES, at),
        }
    }

    pub fn map(&self) -> &SpeechMap {
        &self.map
    }

    /// Forgets the map before `at`: no reader asks about it again. Only the
    /// count of speech in it stays.
    pub(crate) fn forget_before(&mut self, at: u64) {
        self.map.forget_before(at);
    }

    /// The recording ended after the audio heard so far. A last partial
    /// frame is never scored.
    pub(crate) fn finish(&mut self) {
        if self.map.progress == Progress::Ongoing {
            self.map.progress = Progress::Finished;
        }
    }

    /// Scores `audio`, which continues the stream where the last call left
    /// off. A partial frame waits for the next call.
    pub fn hear(&mut self, mut audio: &[f32]) -> Result<(), VoiceActivityError> {
        while !audio.is_empty() && !self.map.is_broken() {
            let taken = audio.len().min(V::FRAME_SAMPLES - self.frame.len());
            self.frame
                .extend(audio[..taken].iter().copied().map(pcm16_from_f32));
            audio = &audio[taken..];
            if self.frame.len() == V::FRAME_SAMPLES {
                self.score_frame()?;
                self.frame.clear();
            }
        }
        Ok(())
    }

    fn score_frame(&mut self) -> Result<(), VoiceActivityError> {
        let start = self.map.reached;
        let speech_before = self.endpointer.speech_samples();
        let quiet_before = self.endpointer.silence_since();
        let boundaries = self
            .endpointer
            .push(&self.frame)
            .inspect_err(|_| self.map.break_off())?;
        let end = start + V::FRAME_SAMPLES as u64;
        self.map.reached = end;
        if self.endpointer.speech_samples() > speech_before {
            self.map.add_speech(start..end);
        }
        match boundaries.first() {
            Some(Boundary::Start(_)) => self.map.utterances.push(Utterance { start, end: None }),
            Some(Boundary::End(_)) => {
                if let Some(utterance) = self.map.utterances.last_mut() {
                    utterance.end = Some(end);
                }
                self.map.close_quiet(end, false);
            }
            None => match (quiet_before, self.endpointer.silence_since()) {
                (None, Some(since)) => self.map.quiet.push(Quiet {
                    // The endpointer counts from where the map starts.
                    start: self.map.from + since,
                    end: None,
                    resumed: false,
                }),
                (Some(_), None) => self.map.close_quiet(start, true),
                (None, None) | (Some(_), Some(_)) => {}
            },
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted {
        scores: Vec<f32>,
        calls: usize,
    }

    impl VoiceActivityDetector for Scripted {
        const FRAME_SAMPLES: usize = 160;

        fn probability(&mut self, _samples: &[i16]) -> Result<f32, VoiceActivityError> {
            let score = self.scores[self.calls];
            self.calls += 1;
            Ok(score)
        }
    }

    const FRAME: u64 = Scripted::FRAME_SAMPLES as u64;

    fn detector(scores: &[f32], gap_frames: u64, at: u64) -> SpeechDetector<Scripted> {
        let gap = SilenceDuration::new(Duration::from_millis(gap_frames * 10))
            .expect("the duration is in range");
        SpeechDetector::with_gap(
            Scripted {
                scores: scores.to_vec(),
                calls: 0,
            },
            gap,
            at,
        )
    }

    fn heard(scores: &[f32], gap_frames: u64, piece: usize) -> SpeechMap {
        let mut detector = detector(scores, gap_frames, 0);
        let audio = vec![0.0; scores.len() * Scripted::FRAME_SAMPLES];
        for chunk in audio.chunks(piece) {
            detector.hear(chunk).expect("the scripted VAD scores");
        }
        detector.map().clone()
    }

    #[test]
    fn a_pause_is_a_silence_too_short_to_end_the_utterance() {
        let mut scores = vec![0.9];
        scores.extend([0.0; 15]);
        scores.push(0.9);
        scores.extend([0.0; 5]);
        scores.push(0.9);
        scores.extend([0.0; 30]);
        let map = heard(&scores, 30, 160);
        let pause = FRAME..16 * FRAME;

        assert_eq!(map.pauses_before(22 * FRAME).collect::<Vec<_>>(), [pause]);
        assert_eq!(map.trailing_silence(22 * FRAME), 5 * FRAME);
        assert_eq!(map.trailing_silence(23 * FRAME), 0);
        assert!(map.closes_at(53 * FRAME));
        assert_eq!(map.pauses_before(53 * FRAME).count(), 0);
    }

    #[test]
    fn utterances_open_and_close_on_their_frames() {
        let mut scores = vec![0.0, 0.9, 0.9];
        scores.extend([0.0; 10]);
        scores.extend([0.9, 0.0]);

        for piece in [1, 100, 160, 1_024] {
            let map = heard(&scores, 10, piece);
            assert!(map.opens_at(FRAME), "pieces of {piece}");
            assert!(map.closes_at(13 * FRAME), "pieces of {piece}");
            assert!(map.opens_at(13 * FRAME), "pieces of {piece}");
            assert!(!map.opens_at(0) && !map.closes_at(3 * FRAME));
        }
    }

    #[test]
    fn speech_counts_only_frames_scored_as_speech() {
        let map = heard(&[0.0, 0.9, 0.4, 0.9, 0.0], 10, 160);

        assert_eq!(map.speech_before(5 * FRAME), 2 * FRAME);
        assert_eq!(map.speech(&(FRAME..2 * FRAME)), FRAME);
        assert_eq!(map.speech(&(2 * FRAME..3 * FRAME)), 0);
    }

    #[test]
    fn positions_count_from_where_the_detector_starts() {
        let mut detector = detector(&[0.9], 10, 1_000);
        detector.hear(&[0.0; 160]).unwrap();

        let map = detector.map();
        assert_eq!(map.reached(), 1_000 + FRAME);
        assert!(map.opens_at(1_000));
        assert_eq!(map.speech_before(map.reached()), FRAME);
    }

    #[test]
    fn forgetting_keeps_what_comes_after_and_the_count_before() {
        let mut scores = vec![0.9; 5];
        scores.extend([0.0; 15]);
        scores.extend([0.9; 5]);
        scores.extend([0.0; 15]);
        let mut map = heard(&scores, 10, 160);
        let before = map.speech_before(map.reached());

        map.forget_before(20 * FRAME);

        assert_eq!(map.speech_before(map.reached()), before);
        assert_eq!(map.speech(&(20 * FRAME..25 * FRAME)), 5 * FRAME);
        assert!(map.opens_at(20 * FRAME));
        assert!(!map.closes_at(15 * FRAME));
    }
}
