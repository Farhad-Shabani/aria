//! Committing a transcript prefix only when two successive decodes of a
//! growing buffer agree on it (LocalAgreement-2).

use std::time::Duration;

use crate::audio::{RecordingReader, canonical_duration, canonical_samples};
use crate::cancel::CancelToken;
use crate::primitives::error_chain;

use super::sentences::ends_sentence;
use super::{
    ChunkStep, Hypothesis, SILENCE_FLOOR, Transcript, TranscriptSegment, TranscriptUpdate,
    TranscriptionError, check_cancelled,
};

/// How much new audio each decode waits for, and how far the buffer may grow
/// before it is cut whether or not the speaker paused.
#[derive(Clone, Copy)]
pub struct Cadence {
    stride_samples: usize,
    max_buffer_samples: usize,
}

impl Cadence {
    pub const fn from_millis(stride_millis: usize, max_buffer_millis: usize) -> Self {
        assert!(stride_millis > 0, "a stride must advance the buffer");
        assert!(
            stride_millis < max_buffer_millis,
            "a buffer must hold more than one stride"
        );
        Self {
            stride_samples: canonical_samples(Duration::from_millis(stride_millis as u64)) as usize,
            max_buffer_samples: canonical_samples(Duration::from_millis(max_buffer_millis as u64))
                as usize,
        }
    }

    /// How many samples of new audio each decode waits for.
    pub const fn stride_samples(&self) -> usize {
        self.stride_samples
    }

    pub const fn max_buffer_samples(&self) -> usize {
        self.max_buffer_samples
    }
}

/// One trim-search frame: 512 samples, about 32 ms of speech.
const TRIM_FRAME_SAMPLES: usize = 512;
/// A pause must run this many frames before a cut trusts it: about 416 ms,
/// longer than a stop closure.
const TRIM_SILENCE_FRAMES: usize = 13;
/// A frame counts as quiet at or under this fraction of the buffer's loudest
/// frame. In a noisy room nothing falls under it and every trim becomes a
/// cap-forced cut.
const TRIM_QUIET_FRACTION: f32 = 0.05;
/// Heard words a decode may lose and still count as a revision rather than
/// a dropped window.
const TOLERATED_LOSS: usize = 2;
/// Words a decode may insert or drop ahead of the delivered words and
/// still be placed by them.
const PLACEMENT_SHIFT: usize = 2;
/// Frames a word's timing reaches either side of itself. Timings are coarse,
/// and a word's edges blur into its neighbours.
const HEARD_MARGIN_FRAMES: usize = 3;
/// Speech frames in a row that no word covers before the buffer counts as
/// skipped speech: about half a second, longer than a cough or a breath.
const UNHEARD_SPEECH_FRAMES: usize = 16;
/// A buffer's quietest tenth of frames is its room noise. The single
/// quietest frame could be a dropout.
const NOISE_FRAME_DIVISOR: usize = 10;
/// The margin over room noise a speech frame needs: 6 dB.
const SPEECH_OVER_NOISE: f32 = 4.0;
/// Frames in a row over a pause that break it from under the floor: about
/// 96 ms, longer than a key click.
const PAUSE_BREAK_FRAMES: usize = 3;
/// No frame this quiet is speech, however quiet the pause it follows:
/// -60 dBFS, under any distant voice and over a muted stream's dither.
const QUIETEST_SPEECH: f32 = 1e-6;
/// How far before the last delivered word's end a cap cut lands, so the
/// next buffer hears that word whole rather than a fragment of it: 240 ms,
/// past a timing's error and most of a short word.
const CUT_OVERLAP_SAMPLES: u64 = canonical_samples(Duration::from_millis(240));
/// Delivered words the next buffer keeps in mind, so the ones its opening
/// hears again are not said twice.
const CARRIED_WORDS: usize = 4;
/// How many words a new buffer's decode may open with before a delivered
/// word: a fragment of the cut word, or a junk word.
const REPEAT_REACH: usize = 2;
/// Eight words in under a second are too many to be speech: the decode
/// invented them.
const CRAMMED_WORDS: usize = 8;

const RESTORE_ANCHOR_WORDS: usize = 4;
const RESTORE_MIN_MISSING_WORDS: usize = 3;
const RESTORE_TIMING_TOLERANCE_SAMPLES: u64 = canonical_samples(Duration::from_millis(240));
const RESTORE_MARGIN_SAMPLES: u64 = canonical_samples(Duration::from_millis(96));
/// Audio before a cut, decoded again with the buffer. Alone, a buffer
/// always opens a new sentence.
const LOOKBACK_SAMPLES: u64 = canonical_samples(Duration::from_secs(3));

/// A cut the decodes still reach back past.
#[derive(Default)]
struct Lookback {
    /// How the latest decode wrote the last word before the cut.
    sentence_ended_at_cut: Option<bool>,
}

/// Drives a growing buffer of audio through [`Agreement`] and delivers what
/// it confirms, one segment per step.
pub struct AgreementState {
    cadence: Cadence,
    /// Stream position the buffer opens at. Moves only on a trim.
    buffer_start: u64,
    /// Stream position the last decode reached.
    decoded_end: u64,
    /// The loudest frame of the pause the buffer opened in. While nothing
    /// since has broken that pause, its decode is ignored.
    pause_level: Option<f32>,
    /// What this buffer's decodes have agreed on so far. A trim keeps only
    /// the last delivered words, so the next buffer's decodes can tell them
    /// apart when they hear them again. A pause cut also keeps the words
    /// heard past the pause, which lie inside the next buffer.
    agreement: Agreement,
    transcript: Vec<String>,
    /// A segment waiting for the next call, so a step carries one segment
    /// even when a trim or an ending produces two.
    pending_segment: Option<TranscriptSegment>,
    pending_preview: Option<(String, std::ops::Range<u64>)>,
    finished: bool,
    lookback: Option<Lookback>,
    /// The engine times no words, so a lookback can't be split off. Every
    /// buffer from here on decodes alone.
    untimed: bool,
}

impl AgreementState {
    pub(super) fn new(cadence: Cadence) -> Self {
        Self {
            cadence,
            buffer_start: 0,
            decoded_end: 0,
            pause_level: None,
            agreement: Agreement::default(),
            transcript: Vec::new(),
            pending_segment: None,
            pending_preview: None,
            finished: false,
            lookback: None,
            untimed: false,
        }
    }

    /// The earliest position a reader may still need. Never decreases: a
    /// cut lands at or after the opening, so the next lookback starts no
    /// earlier. A cap-forced trim moves the opening past a segment nobody
    /// has taken yet, whose reader still wants its audio. So does a word
    /// that waits for a second decode: a lookback decode may time it
    /// before the buffer, and an untimed decode then ends the lookback.
    pub(super) fn retained_from(&self) -> u64 {
        let pending = self
            .pending_segment
            .as_ref()
            .and_then(TranscriptSegment::source_window)
            .map(|span| span.start);
        let waiting = self
            .agreement
            .waiting()
            .iter()
            .filter_map(|word| word.span.as_ref())
            .map(|span| span.start);
        waiting.chain(pending).fold(self.lookback_from(), u64::min)
    }

    /// Where a lookback starts. The buffer's opening when the engine times
    /// no words.
    fn lookback_from(&self) -> u64 {
        if self.untimed {
            self.buffer_start
        } else {
            self.buffer_start.saturating_sub(LOOKBACK_SAMPLES)
        }
    }

    #[cfg(test)]
    pub(super) fn next_finalized_with(
        &mut self,
        recording: &mut RecordingReader,
        cancel: &CancelToken,
        decode: impl FnMut(&[f32]) -> Result<Hypothesis, TranscriptionError>,
    ) -> Result<Option<ChunkStep>, TranscriptionError> {
        self.next_with(recording, cancel, false, decode)
    }

    pub(super) fn next_with(
        &mut self,
        recording: &mut RecordingReader,
        cancel: &CancelToken,
        previews: bool,
        mut decode: impl FnMut(&[f32]) -> Result<Hypothesis, TranscriptionError>,
    ) -> Result<Option<ChunkStep>, TranscriptionError> {
        if previews {
            check_cancelled(cancel)?;
        }
        if let Some(segment) = self.pending_segment.take() {
            return Ok(Some(ChunkStep {
                update: Some(TranscriptUpdate::Finalized(segment)),
                transcript: None,
            }));
        }
        if let Some((text, source_window)) = self.pending_preview.take()
            && previews
        {
            return Ok(Some(ChunkStep {
                update: Some(TranscriptUpdate::Provisional {
                    text,
                    source_window: Some(source_window),
                }),
                transcript: None,
            }));
        }
        if self.finished {
            return Ok(None);
        }
        loop {
            let step = match self.await_stride(recording, cancel)? {
                Some(available) => self.step(recording, available, cancel, &mut decode)?,
                None => self.finish_after(None)?,
            };
            // Every stride's provisional text goes out, changed or not. The caller
            // decides what is worth showing.
            if previews {
                let text = self.agreement.preview();
                let source_window = self.buffer_start..self.decoded_end;
                let finalized = step.as_ref().is_some_and(|step| {
                    matches!(step.update, Some(TranscriptUpdate::Finalized(_)))
                });
                if finalized {
                    self.pending_preview = Some((text, source_window));
                } else {
                    let mut step = step.unwrap_or(ChunkStep {
                        update: None,
                        transcript: None,
                    });
                    step.update = Some(TranscriptUpdate::Provisional {
                        text,
                        source_window: Some(source_window),
                    });
                    return Ok(Some(step));
                }
            }
            if step.is_some() {
                return Ok(step);
            }
        }
    }

    /// Waits for one more stride of audio and returns where capture now
    /// ends. `None` when nothing new arrived and capture is over: what
    /// remains is the tail.
    fn await_stride(
        &self,
        recording: &mut RecordingReader,
        cancel: &CancelToken,
    ) -> Result<Option<u64>, TranscriptionError> {
        check_cancelled(cancel)?;
        recording
            .fill_through(self.decoded_end + self.cadence.stride_samples as u64)
            .map_err(TranscriptionError::Capture)?;
        check_cancelled(cancel)?;
        // The cap bounds a decode that fell behind capture; without it a
        // finished recording decodes as one window.
        let buffer_end = self.buffer_start + self.cadence.max_buffer_samples as u64;
        let available = recording.held_to().min(buffer_end);
        if available == self.decoded_end && recording.reached_end() {
            return Ok(None);
        }
        Ok(Some(available))
    }

    /// One decode of the buffer up to `available`. `None` when it delivered
    /// nothing and capture goes on, so the caller waits for the next stride.
    fn step(
        &mut self,
        recording: &RecordingReader,
        available: u64,
        cancel: &CancelToken,
        decode: &mut impl FnMut(&[f32]) -> Result<Hypothesis, TranscriptionError>,
    ) -> Result<Option<ChunkStep>, TranscriptionError> {
        // Audio that landed while the previous decode was running.
        // `fill_through` waits for one stride, so anything past that
        // piled up behind the decoder — and a decode covering four
        // strides of audio emits four strides of words at once, which
        // is what a reader sees as a sentence arriving whole.
        let behind =
            available.saturating_sub(self.decoded_end + self.cadence.stride_samples as u64);
        self.decoded_end = available;
        // The decode opens on the lookback until the buffer has delivered
        // its first words.
        let decode_from = if self.lookback.is_some() {
            self.lookback_from()
        } else {
            self.buffer_start
        };
        // The width this step handed the model, read before the trim moves
        // the buffer on.
        let decoded = available - decode_from;

        let buffer = recording.span(self.buffer_start..available);
        let (hypothesis, decode_ms) = timed(|| decode(recording.span(decode_from..available)));
        let hypothesis = hypothesis?;
        check_cancelled(cancel)?;
        if hypothesis.words().is_empty() && !hypothesis.as_str().trim().is_empty() {
            self.untimed = true;
        }
        let loudness = Loudness::of(buffer);
        let heard = self.hear(&hypothesis, &loudness, decode_from);
        let heard_count = heard.len();
        let skipped = self.skipped_speech(&loudness, &heard);
        let (confirmed, standing) = self.agreement.agree(heard, skipped);
        let confirmed = self.commit(confirmed);
        // Relative to this decode's window, so it is read before the trim
        // moves the window on.
        let skipped = skipped.map(|at| at - self.buffer_start);
        let step = if recording.reached_end() && available == recording.held_to() {
            self.finish_after(confirmed)?
        } else {
            let tail = self.trim_when_due(&loudness, available, standing);
            self.one_at_a_time(confirmed, tail)
                .map(|segment| ChunkStep {
                    update: Some(TranscriptUpdate::Finalized(segment)),
                    transcript: None,
                })
        };
        // A step hands out one segment and parks any second for the next call.
        let released = step.as_ref().and_then(|step| match &step.update {
            Some(TranscriptUpdate::Finalized(segment)) => Some(segment),
            _ => None,
        });
        log_step(
            decoded,
            decode_ms,
            behind,
            heard_count,
            skipped,
            [released, self.pending_segment.as_ref()],
            standing == Standing::Settled,
        );
        Ok(step)
    }

    /// The decode's words at their stream positions, untimed when timings
    /// do not align. A lookback decode keeps only the buffer's own words,
    /// and reads the cut from the one before them. Empty when none can go
    /// out.
    fn hear(&mut self, hypothesis: &Hypothesis, loudness: &Loudness, from: u64) -> Vec<Word> {
        if self.pause_level.is_some_and(|level| loudness.within(level)) {
            tracing::debug!("buffer holds only the pause; decode ignored");
            return Vec::new();
        }
        let mut words = self.words_of(hypothesis, from);
        if let Some(lookback) = &mut self.lookback
            && from < self.buffer_start
        {
            if words.iter().any(|word| word.span.is_none()) {
                tracing::debug!("lookback decode has an untimed word; buffers decode alone");
                self.untimed = true;
                self.lookback = None;
                return Vec::new();
            }
            let by_word = self.agreement.after_last_delivered(&words);
            // With no delivered word to go by, the decode reworded it or
            // it was untimed, and only the timings are left.
            let buffer_words = by_word.unwrap_or_else(|| {
                words
                    .iter()
                    .position(|word| {
                        word.span
                            .as_ref()
                            .is_some_and(|span| span.start >= self.buffer_start)
                    })
                    .unwrap_or(words.len())
            });
            tracing::debug!(
                by_word = by_word.is_some(),
                before = buffer_words,
                "lookback split"
            );
            let mut heard_before = words.drain(..buffer_words);
            lookback.sentence_ended_at_cut = heard_before
                .next_back()
                .map(|word| ends_sentence(&word.text));
        }
        if crammed(&words) {
            tracing::debug!(words = words.len(), "decode invented words");
            return Vec::new();
        }
        words
    }

    fn words_of(&self, hypothesis: &Hypothesis, from: u64) -> Vec<Word> {
        let texts: Vec<&str> = hypothesis.as_str().split_whitespace().collect();
        let mut spans = self
            .spans_of(hypothesis, from, texts.len())
            .map(Vec::into_iter);
        texts
            .iter()
            .map(|text| Word {
                text: (*text).to_owned(),
                span: spans.as_mut().and_then(Iterator::next),
            })
            .collect()
    }

    /// Where each word sits in the decoded audio, which opened at `from`.
    /// A decode with any impossible timing counts as untimed: its text
    /// still agrees.
    fn spans_of(
        &self,
        hypothesis: &Hypothesis,
        from: u64,
        words: usize,
    ) -> Option<Vec<std::ops::Range<u64>>> {
        if hypothesis.words().len() != words {
            return None;
        }
        let spans: Result<Vec<_>, TranscriptionError> = hypothesis
            .words()
            .iter()
            .map(|word| super::bounded_span(word.span_from(from)?, self.decoded_end))
            .collect();
        spans
            .inspect_err(|error| {
                tracing::debug!(error = %error_chain(error), "decode timings are impossible; heard untimed");
            })
            .ok()
    }

    /// A segment carries its own reading of the boundary before it, or the
    /// lookback's reading of the cut.
    fn commit(&mut self, segment: Option<TranscriptSegment>) -> Option<TranscriptSegment> {
        let mut segment = segment.map(|segment| {
            let at_cut = self
                .lookback
                .take()
                .and_then(|lookback| lookback.sentence_ended_at_cut);
            let ended_before = segment.sentence_ended_before().or(at_cut);
            segment.with_sentence_ended_before(ended_before)
        });
        if let Some(segment) = &mut segment {
            // A word kept from a lookback decode may open before the buffer.
            let from = segment
                .span()
                .map_or(self.buffer_start, |span| span.start.min(self.buffer_start));
            segment.source_window = Some(from..self.decoded_end);
            self.transcript.push(segment.as_str().to_owned());
        }
        segment
    }

    /// Cuts the buffer at a pause, or at the cap. Returns the tail a cap
    /// cut delivers unconfirmed.
    fn trim_when_due(
        &mut self,
        loudness: &Loudness,
        available: u64,
        standing: Standing,
    ) -> Option<TranscriptSegment> {
        let pause = match standing {
            Standing::Ignored => None,
            Standing::Current => self.pause_past_delivered(loudness),
            // A further stride added no words: a pause, or a word the model
            // holds back until it hears the end of it. When no delivered
            // word places the pause, only one the buffer ends in tells the
            // two apart. No word is left waiting to carry past it.
            Standing::Settled => self
                .pause_past_delivered(loudness)
                .or_else(|| loudness.trailing_pause()),
        };
        if let Some(pause) = pause {
            let agreement = self.agreement.next_buffer_past_pause();
            return self.trim(
                self.buffer_start + pause.at as u64,
                agreement,
                None,
                Some(pause.level),
            );
        }
        if available - self.buffer_start >= self.cadence.max_buffer_samples as u64 {
            return self.cap_cut(loudness, available);
        }
        None
    }

    /// The pause that follows the last delivered word's sound. It ends
    /// before any word still waiting for a second decode. Every word before
    /// it is out and none past it is, so a cut there repeats and drops
    /// nothing, wherever the stride ends fall. Timings only bound the
    /// pause: a word's end often runs into the silence after it, and the
    /// next word's start into the silence before it. `None` unless the
    /// last delivered word is timed inside the buffer: the loudness holds
    /// no frame before the opening.
    fn pause_past_delivered(&self, loudness: &Loudness) -> Option<Pause> {
        let last = self
            .agreement
            .delivered
            .last()?
            .span
            .as_ref()
            .filter(|span| span.start >= self.buffer_start)?;
        let until = match self.agreement.waiting().first() {
            Some(word) => word.span.as_ref()?.start,
            None => self.decoded_end,
        };
        if until < last.end {
            return None;
        }
        let frame = |at: u64| (at - self.buffer_start) as usize / TRIM_FRAME_SAMPLES;
        loudness.pause_after(frame(last.start)..frame(last.end), frame(until))
    }

    /// The buffer must go whether or not the speaker paused. The unconfirmed
    /// tail goes out, and the cut lands a little before the last delivered
    /// word or the skipped speech, whichever comes first. A later cut would
    /// lose short words the model missed in between. Without timings the cut
    /// lands at the end: an earlier one would re-decode words just typed.
    /// Skipped speech too near the opening to cut before is ignored, or the
    /// words heard after it would be lost.
    fn cap_cut(&mut self, loudness: &Loudness, available: u64) -> Option<TranscriptSegment> {
        let skipped = self
            .skipped_speech(loudness, &self.agreement.candidate)
            .filter(|&at| at.saturating_sub(CUT_OVERLAP_SAMPLES) > self.buffer_start);
        let tail = self.agreement.take_tail(skipped);
        let tail = self.commit(tail);
        let cut = [self.agreement.delivered_through, skipped]
            .into_iter()
            .flatten()
            .map(|through| through.saturating_sub(CUT_OVERLAP_SAMPLES))
            .filter(|&through| through > self.buffer_start)
            .min()
            .map_or(available, |through| through.min(available));
        // A pause nobody has broken since the last pause cut is still the
        // same pause, cap or no cap. Its level follows the room, so noise
        // that creeps up over many caps stays the pause.
        let pause_level = self
            .pause_level
            .filter(|&level| loudness.within(level))
            .map(|level| level.max(loudness.loudest));
        let agreement = self.agreement.next_buffer();
        self.trim(cut, agreement, tail, pause_level)
    }

    /// Where speech the model skipped begins, past what was delivered: the
    /// first stretch of speech no word of `heard` covers. Nothing within a
    /// stride of the opening counts, or a buffer the model keeps skipping
    /// the start of would never move on. A buffer that opened in a pause
    /// opens on no sound, so all its speech counts: the words past a
    /// skipped first word would go out ahead of it.
    fn skipped_speech(&self, loudness: &Loudness, heard: &[Word]) -> Option<u64> {
        let opening = match self.pause_level {
            Some(_) => self.buffer_start,
            None => self.buffer_start + self.cadence.stride_samples as u64,
        };
        let from = self
            .agreement
            .delivered_through
            .unwrap_or(self.buffer_start)
            .max(opening);
        loudness.unheard_speech(self.buffer_start, heard, from)
    }

    fn trim(
        &mut self,
        cut: u64,
        agreement: Agreement,
        tail: Option<TranscriptSegment>,
        pause_level: Option<f32>,
    ) -> Option<TranscriptSegment> {
        self.buffer_start = cut;
        self.pause_level = pause_level;
        self.lookback = Some(Lookback::default());
        self.agreement = agreement;
        tail
    }

    /// A step carries one segment. When it produced two, the second waits
    /// for the next call.
    fn one_at_a_time(
        &mut self,
        first: Option<TranscriptSegment>,
        second: Option<TranscriptSegment>,
    ) -> Option<TranscriptSegment> {
        match (first, second) {
            (Some(first), Some(second)) => {
                self.pending_segment = Some(second);
                Some(first)
            }
            (first, second) => first.or(second),
        }
    }

    /// Delivers whatever was never confirmed by a second decode, plus the
    /// final transcript. Reading the decode in force means a last decode
    /// that returned empty still delivers the tail from the last good one.
    fn finish_after(
        &mut self,
        committed: Option<TranscriptSegment>,
    ) -> Result<Option<ChunkStep>, TranscriptionError> {
        let tail = self.agreement.take_tail(None);
        let tail = self.commit(tail);
        // Nobody spoke, which is an ordinary end to a streaming transcription, not a
        // failure. `Operation::finish` still answers `NoSpeech` for callers
        // that asked for a transcript and got none.
        let words = std::mem::take(&mut self.transcript).join(" ");
        let transcript = (!words.is_empty())
            .then(|| Transcript::new(words))
            .transpose()?;
        self.finished = true;
        Ok(Some(ChunkStep {
            update: self
                .one_at_a_time(committed, tail)
                .map(TranscriptUpdate::Finalized),
            transcript,
        }))
    }
}

/// LocalAgreement-2 over one buffer: a word leaves once two successive
/// decodes agree on it.
///
/// A decode's words are told from the delivered ones by their text, in
/// order: the longest opening run that repeats the end of what was
/// delivered is a repeat, whether the decode opened later than the last one
/// or heard a word again across a cut. Timings place cuts and distinguish
/// a later opening from a revision covering the same audio.
#[derive(Default)]
struct Agreement {
    /// The latest hypothesis, including retained speech.
    candidate: Vec<Word>,
    /// Prefix excluded from delivery, including words covering delivered audio.
    emitted: usize,
    /// Set once a decode that no delivered text could place is placed by
    /// its word timings alone, and kept for the rest of the buffer. From
    /// then on the words before `emitted` are not known to repeat delivered
    /// text, so a later decode's new words begin right after the last
    /// delivered word, or else after the delivered audio.
    timing_recovery: bool,
    /// The words delivered so far — the buffer's own, and the last few from
    /// before its cut — which a decode may hear again.
    delivered: Vec<Word>,
    /// Where the last delivered word's audio ends. `None` until a timed
    /// word is delivered, and again once an untimed one is: its end is
    /// unknown, and a cut before it would deliver it twice.
    delivered_through: Option<u64>,
}

impl Agreement {
    /// The agreement for the buffer after this one, which keeps only the
    /// last delivered words to recognise them again.
    fn next_buffer(&mut self) -> Self {
        let carried = self.delivered.len().saturating_sub(CARRIED_WORDS);
        Self {
            delivered: self.delivered.split_off(carried),
            delivered_through: self.delivered_through,
            ..Self::default()
        }
    }

    /// The agreement for the buffer a pause cut opens. Beside the last
    /// delivered words, it keeps the words heard past the pause, so the next
    /// decode can confirm them rather than hear them afresh.
    fn next_buffer_past_pause(&mut self) -> Self {
        let waiting = self.waiting().to_vec();
        Self {
            candidate: waiting,
            ..self.next_buffer()
        }
    }

    /// How many opening words of a lookback decode were delivered already:
    /// the words through the latest, longest run of the last delivered
    /// words that ends where the last one sounded. Text alone finds an
    /// earlier copy of a phrase when the decode rewords the delivered one,
    /// and audio alone a later copy of the word timed back into the pause.
    /// The audio need only overlap, and no timing says which words follow:
    /// an engine moves a word's start by up to a second, most of all the
    /// first word after a pause.
    ///
    /// `None` when the decode holds no such run, or the last delivered word
    /// is untimed.
    fn after_last_delivered(&self, words: &[Word]) -> Option<usize> {
        let carried = &self.delivered[self.delivered.len().saturating_sub(CARRIED_WORDS)..];
        let sounded = carried.last()?.span.as_ref()?;
        (1..=carried.len().min(words.len()))
            .rev()
            .find_map(|count| {
                let run = &carried[carried.len() - count..];
                (count..=words.len()).rev().find(|&end| {
                    lcp(run, &words[end - count..]) == count
                        && words[end - 1]
                            .span
                            .as_ref()
                            .is_some_and(|span| same_audio(span, sounded))
                })
            })
    }

    /// The latest decode's words still waiting for a second decode.
    fn waiting(&self) -> &[Word] {
        &self.candidate[self.emitted.min(self.candidate.len())..]
    }

    /// Takes one decode. Returns the words it newly agrees on with the last
    /// one, and how the decode stands.
    ///
    /// With `skipped`, where speech the decode skipped begins, only the
    /// words before it go out: the ones past it would leave a hole behind.
    fn agree(
        &mut self,
        mut current: Vec<Word>,
        skipped: Option<u64>,
    ) -> (Option<TranscriptSegment>, Standing) {
        // An empty decode — a dropped window, or one `hear` threw out —
        // leaves the last good decode in force, so the emptiness does not
        // stall the steps after it.
        if current.is_empty() {
            return (None, Standing::Ignored);
        }
        let retained_prefix_len = self.missing_prefix_len(&current);
        if let Some(count) = retained_prefix_len {
            let mut retained = self.candidate[..count].to_vec();
            retained.append(&mut current);
            current = retained;
        }
        if self.dropped_window(&current) {
            return (None, Standing::Ignored);
        }
        let placement = self.placed(&current);
        let recovered = placement.is_none();
        let (start, emitted) = match placement {
            Some(placement) => placement,
            None => {
                let Some(start) = self.after_delivered_audio(&current) else {
                    return (None, Standing::Ignored);
                };
                self.timing_recovery = true;
                (start, start)
            }
        };
        // Retained words have only one model observation; they remain provisional.
        let agreed = start
            + if retained_prefix_len.is_some() {
                0
            } else {
                lcp(self.waiting(), &current[start..])
            };
        let deliverable = match skipped {
            Some(at) => heard_before(&current[..agreed], at),
            None => agreed,
        };
        let confirmed = if deliverable > start {
            self.deliver(&current[start..deliverable], ended_before(&current, start))
        } else {
            None
        };
        self.emitted = deliverable.max(emitted);
        // A decode placed by timing alone has not agreed with anything yet.
        let settled = !recovered
            && agreed == current.len()
            && current.len() == self.candidate.len()
            && skipped.is_none();
        self.candidate = current;
        let standing = if settled {
            Standing::Settled
        } else {
            Standing::Current
        };
        (confirmed, standing)
    }

    fn missing_prefix_len(&self, current: &[Word]) -> Option<usize> {
        let anchor = current.get(..RESTORE_ANCHOR_WORDS)?;
        let opening = anchor[0].span.as_ref()?;
        for offset in self.emitted + RESTORE_MIN_MISSING_WORDS..self.candidate.len() {
            let Some(previous) = self.candidate.get(offset..offset + RESTORE_ANCHOR_WORDS) else {
                break;
            };
            if lcp(previous, anchor) != RESTORE_ANCHOR_WORDS {
                continue;
            }
            let same_audio = previous.iter().zip(anchor).all(|(old, new)| {
                old.span
                    .as_ref()
                    .zip(new.span.as_ref())
                    .is_some_and(|(old, new)| {
                        old.start.abs_diff(new.start) <= RESTORE_TIMING_TOLERANCE_SAMPLES
                    })
            });
            let uncovered = self.candidate[self.emitted..offset].iter().all(|word| {
                word.span
                    .as_ref()
                    .is_some_and(|span| span.end <= opening.start + RESTORE_MARGIN_SAMPLES)
            });
            if same_audio && uncovered {
                return Some(offset);
            }
        }
        None
    }

    /// Where the decode's new words begin, and how many words count as out
    /// once it stands as the last decode. A decode that opens where the
    /// last one did is placed by [`Self::aligned`]. One that opens
    /// elsewhere is counted afresh by the delivered words it repeats.
    ///
    /// `None` when no delivered text or anchored candidate locates the
    /// decode. Its word timings may still locate undelivered audio.
    fn placed(&self, current: &[Word]) -> Option<(usize, usize)> {
        // Once words are out, a lone repeated word is too little to place a
        // decode by: a heavily revised decode shows one by chance.
        let least = if self.emitted > 0 { 2 } else { 1 };
        let placement = match self.aligned(current) {
            Some(placed) if self.emitted > 0 => Some(placed),
            _ => self
                .repeated(current, least)
                .map(|repeated| (repeated, repeated)),
        };
        if let Some((start, emitted)) = placement
            && (start > 0 || (self.emitted == 0 && !self.timing_recovery))
        {
            if self.timing_recovery && !self.anchored_at(current, start) {
                let through = self.after_delivered_audio(current)?;
                return Some((start.max(through), emitted.max(through)));
            }
            return Some((start, emitted));
        }
        tracing::debug!(
            heard = self.candidate.len(),
            current = current.len(),
            "decode lost its place"
        );
        None
    }

    fn anchored_at(&self, words: &[Word], end: usize) -> bool {
        let Some((word, delivered)) = end
            .checked_sub(1)
            .and_then(|index| words.get(index))
            .zip(self.delivered.last())
        else {
            return false;
        };
        if spoken(&word.text) != spoken(&delivered.text) {
            return false;
        }
        match (&word.span, &delivered.span) {
            (Some(word), Some(delivered)) => same_audio(word, delivered),
            _ => {
                end >= 2
                    && self.delivered.len() >= 2
                    && lcp(
                        &self.delivered[self.delivered.len() - 2..],
                        &words[end - 2..end],
                    ) == 2
            }
        }
    }

    /// How many opening words lie in delivered audio, going by timing alone.
    /// A word that starts within `HEARD_MARGIN_FRAMES` of the delivered end
    /// is new speech whose timing drifted, as in [`Self::opens_new_audio`].
    fn after_delivered_audio(&self, words: &[Word]) -> Option<usize> {
        let through = self.delivered_through?;
        let margin = (HEARD_MARGIN_FRAMES * TRIM_FRAME_SAMPLES) as u64;
        let mut start = 0;
        for (index, word) in words.iter().enumerate() {
            let span = word.span.as_ref()?;
            if span.start + margin < through {
                start = index + 1;
            }
        }
        Some(start)
    }

    /// How many opening words of `words` were delivered already, going by
    /// the last delivered words: a run of them found in the opening, as
    /// long a run as possible, as late in the delivered words as possible.
    /// Everything before the run went out earlier, and so did whatever
    /// delivered words follow it, however the decode now hears them. Runs
    /// shorter than `least` do not count. Zero when none turn up: the
    /// decode opened past them. `None` when the decode ends before the
    /// delivered words do.
    fn repeated(&self, words: &[Word], least: usize) -> Option<usize> {
        let carried = &self.delivered[self.delivered.len().saturating_sub(CARRIED_WORDS)..];
        for count in (least..=carried.len().min(words.len())).rev() {
            for at in (0..=carried.len() - count).rev() {
                let rest = carried.len() - at - count;
                if let Some(end) = self.opening_run(&carried[at..at + count], rest, words) {
                    return (end + rest <= words.len()).then_some(end + rest);
                }
            }
        }
        Some(0)
    }

    /// Where `run` appears at the opening of `words`, as the index past
    /// it. Speech repeats itself, so a run is trusted only where a cut
    /// leaves what is heard again: at the very opening, after at most a
    /// fragment or a junk word, and starting before the delivered end —
    /// where new speech cannot be — when it is timed. A timed single word
    /// must also be the last delivered word, or end before the delivered
    /// end.
    ///
    /// Once this buffer has delivered words, a full carried run may sit
    /// anywhere in its decode. Any run then counts only when new audio
    /// follows it and the `rest` of the carried words: an earlier copy of
    /// the phrase is followed by delivered audio. A fresh buffer repeats
    /// only a fragment at its opening.
    fn opening_run(&self, run: &[Word], rest: usize, words: &[Word]) -> Option<usize> {
        let count = run.len();
        let reach = if count >= CARRIED_WORDS && self.emitted > 0 {
            words.len()
        } else {
            count + REPEAT_REACH
        };
        let before_delivered_end = |word: &Word, edge: fn(&std::ops::Range<u64>) -> u64| match (
            &word.span,
            self.delivered_through,
        ) {
            (Some(span), Some(through)) => edge(span) < through,
            (None, _) => true,
            _ => false,
        };
        let last_delivered = |word: &Word| {
            self.delivered
                .last()
                .is_some_and(|last| spoken(&last.text) == spoken(&word.text))
        };
        (count..=reach.min(words.len())).find(|&end| {
            let first = &words[end - count];
            let last = &words[end - 1];
            lcp(run, &words[end - count..]) == count
                && before_delivered_end(first, |span| span.start)
                && (count > 1
                    || last_delivered(last)
                    || before_delivered_end(last, |span| span.end))
                && (self.emitted == 0 || self.opens_new_audio(words, end + rest))
        })
    }

    /// Where a decode that opens like the last one begins its new words,
    /// and how many words count as out.
    ///
    /// The last delivered words mark the place, since an inserted or
    /// dropped word moves them. When they are gone, the decode revised
    /// them. It still opens at the same place if it differs from the last
    /// decode in at most `TOLERATED_LOSS` delivered words. The count then
    /// stays put: a shorter decode holds nothing new, and lowering the count
    /// would offer words a second time. `None` otherwise.
    fn aligned(&self, words: &[Word]) -> Option<(usize, usize)> {
        if let Some(end) = self.delivered_end(words) {
            return Some((end, end));
        }
        let compared = self.emitted.min(words.len()).min(self.candidate.len());
        let revised = self.candidate[..compared]
            .iter()
            .zip(&words[..compared])
            .filter(|(heard, word)| spoken(&heard.text) != spoken(&word.text))
            .count();
        (revised <= TOLERATED_LOSS).then_some((self.emitted.min(words.len()), self.emitted))
    }

    /// Where the last delivered words end in `words`. Tries the last two,
    /// then the last one alone, searching outward from the count by up to
    /// `PLACEMENT_SHIFT` words. New audio must follow the match: a revised
    /// delivered word looks like a dropped one, and only timing tells them
    /// apart.
    fn delivered_end(&self, words: &[Word]) -> Option<usize> {
        let nearest_ends: Vec<usize> =
            std::iter::once(Some(self.emitted))
                .chain((1..=PLACEMENT_SHIFT).flat_map(|shift| {
                    [Some(self.emitted + shift), self.emitted.checked_sub(shift)]
                }))
                .flatten()
                .collect();
        for count in [2, 1] {
            let Some(from) = self.delivered.len().checked_sub(count) else {
                continue;
            };
            let run = &self.delivered[from..];
            let found = nearest_ends.iter().copied().find(|&end| {
                end >= count
                    && end <= words.len()
                    && lcp(run, &words[end - count..]) == count
                    && self.opens_new_audio(words, end)
            });
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// Whether the word at `end` starts past the delivered audio, within
    /// `HEARD_MARGIN_FRAMES`. True when there is no such word or no timing
    /// to compare.
    fn opens_new_audio(&self, words: &[Word], end: usize) -> bool {
        let margin = (HEARD_MARGIN_FRAMES * TRIM_FRAME_SAMPLES) as u64;
        match (
            words.get(end).and_then(|word| word.span.as_ref()),
            self.delivered_through,
        ) {
            (Some(span), Some(through)) => span.start + margin >= through,
            _ => true,
        }
    }

    /// A decode that hears several words fewer than the last one did
    /// dropped part of its window. It stays out of the record: settling on
    /// it would cut audio it never heard.
    fn dropped_window(&self, current: &[Word]) -> bool {
        let lost = self.candidate.len().saturating_sub(current.len());
        if lost > TOLERATED_LOSS {
            tracing::debug!(
                heard = self.candidate.len(),
                current = current.len(),
                "decode dropped part of its window"
            );
        }
        lost > TOLERATED_LOSS
    }

    /// Hands `words` over, and remembers them. `ended_before` is how the
    /// decode heard the word before them.
    fn deliver(&mut self, words: &[Word], ended_before: Option<bool>) -> Option<TranscriptSegment> {
        self.delivered_through = ends_at(words, self.delivered_through);
        self.delivered.extend_from_slice(words);
        segment_of(words).map(|segment| segment.with_sentence_ended_before(ended_before))
    }

    fn tail_range(&self, skipped: Option<u64>) -> std::ops::Range<usize> {
        let undelivered = self
            .repeated(&self.candidate, 1)
            .unwrap_or(self.candidate.len())
            .max(self.emitted.min(self.candidate.len()));
        let through = match skipped {
            Some(at) => heard_before(&self.candidate, at).max(undelivered),
            None => self.candidate.len(),
        };
        undelivered..through
    }

    fn preview(&self) -> String {
        self.candidate[self.tail_range(None)]
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Delivers the words no second decode confirmed — up to `skipped`,
    /// where speech the decode missed begins, when there is such a place.
    fn take_tail(&mut self, skipped: Option<u64>) -> Option<TranscriptSegment> {
        let range = self.tail_range(skipped);
        self.emitted = range.end;
        let ended_before = ended_before(&self.candidate, range.start);
        let tail: Vec<Word> = self.candidate.drain(range).collect();
        self.deliver(&tail, ended_before)
    }
}

/// How one decode stands in the agreement.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Standing {
    /// It left the last decode in force.
    Ignored,
    /// It stands as the last decode.
    Current,
    /// It stands as the last decode, and says the same words as the one
    /// before.
    Settled,
}

/// One decoded word and where its audio sits in the stream.
#[derive(Clone)]
struct Word {
    text: String,
    span: Option<std::ops::Range<u64>>,
}

/// One segment for a run of words. Its span is `None` when either end
/// lacks a timing, and it keeps each word's timing when every word has one.
fn segment_of(words: &[Word]) -> Option<TranscriptSegment> {
    let timed: Option<Vec<(String, std::ops::Range<u64>)>> = words
        .iter()
        .map(|word| Some((word.text.clone(), word.span.clone()?)))
        .collect();
    if let Some(timed) = timed {
        return TranscriptSegment::timed(&timed);
    }
    let texts: Vec<String> = words.iter().map(|word| word.text.clone()).collect();
    let span = match (words.first(), words.last()) {
        (
            Some(Word {
                span: Some(first), ..
            }),
            Some(Word {
                span: Some(last), ..
            }),
        ) => Some(first.start..last.end.max(first.start)),
        _ => None,
    };
    TranscriptSegment::spanned(&texts, span)
}

/// How many leading words two decodes agree on, zero when they agree on
/// nothing.
fn lcp(previous: &[Word], current: &[Word]) -> usize {
    previous
        .iter()
        .zip(current)
        .take_while(|(left, right)| spoken(&left.text) == spoken(&right.text))
        .count()
}

/// Whether the word before `at` ended a sentence. `None` when `at` opens
/// `words`.
fn ended_before(words: &[Word], at: usize) -> Option<bool> {
    at.checked_sub(1)
        .map(|before| ends_sentence(&words[before].text))
}

/// Whether two timings of a word overlap. Two empty timings must match.
fn same_audio(one: &std::ops::Range<u64>, other: &std::ops::Range<u64>) -> bool {
    one == other || (one.start < other.end && other.start < one.end)
}

/// Ignores case and punctuation when comparing successive decodes.
fn spoken(word: &str) -> String {
    word.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// No time on wasm, where `Instant::now` panics.
fn timed<T>(work: impl FnOnce() -> T) -> (T, Option<u128>) {
    #[cfg(not(target_family = "wasm"))]
    {
        let started = std::time::Instant::now();
        let value = work();
        (value, Some(started.elapsed().as_millis()))
    }
    #[cfg(target_family = "wasm")]
    (work(), None)
}

fn log_step(
    decoded: u64,
    decode_ms: Option<u128>,
    behind: u64,
    heard: usize,
    skipped: Option<u64>,
    released: [Option<&TranscriptSegment>; 2],
    settled: bool,
) {
    let millis = |samples: u64| canonical_duration(samples).as_millis();
    tracing::debug!(
        buffer_ms = millis(decoded),
        decode_ms,
        behind_ms = millis(behind),
        heard,
        skipped_ms = skipped.map(millis),
        words = released
            .into_iter()
            .flatten()
            .map(|segment| segment.as_str().split_whitespace().count())
            .sum::<usize>(),
        settled,
        "decode step"
    );
}

/// Whether a timed word sequence exceeds the hallucination density cutoff.
fn crammed(words: &[Word]) -> bool {
    words.windows(CRAMMED_WORDS).any(|run| {
        let (Some(first), Some(last)) = (&run[0].span, &run[run.len() - 1].span) else {
            return false;
        };
        last.end.saturating_sub(first.start) < canonical_samples(Duration::from_secs(1))
    })
}

/// How many leading words end before `at`. Untimed words count as heard.
fn heard_before(words: &[Word], at: u64) -> usize {
    words
        .iter()
        .take_while(|word| word.span.as_ref().is_none_or(|span| span.end <= at))
        .count()
}

/// Where delivered words leave the delivered end: the last word's, when it
/// is timed and later than `before`; unknown once an untimed word goes out.
fn ends_at(words: &[Word], before: Option<u64>) -> Option<u64> {
    let Some(last) = words.last() else {
        return before;
    };
    last.span
        .as_ref()
        .map(|span| span.end.max(before.unwrap_or(0)))
}

fn room_noise(squares: &[f32]) -> f32 {
    if squares.is_empty() {
        return 0.0;
    }
    let mut powers = squares.to_vec();
    let rank = powers.len() / NOISE_FRAME_DIVISOR;
    let (_, noise, _) = powers.select_nth_unstable_by(rank, f32::total_cmp);
    *noise
}

/// A pause to cut the buffer at.
struct Pause {
    /// Where to cut, as a sample offset into the buffer.
    at: usize,
    /// The loudest frame's mean square past the cut, where the next buffer
    /// opens. The last word's fade before it doesn't count.
    level: f32,
}

/// A buffer's loudness, frame by frame, and the levels that split it into
/// quiet and speech.
struct Loudness {
    squares: Vec<f32>,
    loudest: f32,
    quiet: f32,
    /// A soft sentence after one loud word is quiet beside that word, yet
    /// still speech.
    speech: f32,
}

impl Loudness {
    fn of(buffer: &[f32]) -> Self {
        let squares: Vec<f32> = buffer
            .chunks(TRIM_FRAME_SAMPLES)
            .filter(|frame| frame.len() == TRIM_FRAME_SAMPLES)
            .map(|frame| {
                frame.iter().map(|sample| sample * sample).sum::<f32>() / TRIM_FRAME_SAMPLES as f32
            })
            .collect();
        let loudest = squares.iter().copied().fold(0.0_f32, f32::max);
        let quiet = loudest * TRIM_QUIET_FRACTION;
        // Capped at `quiet`, so every loud frame is also speech.
        let speech = (room_noise(&squares) * SPEECH_OVER_NOISE)
            .max(SILENCE_FLOOR)
            .min(quiet);
        Self {
            squares,
            loudest,
            quiet,
            speech,
        }
    }

    fn is_loud(&self, frame: usize) -> bool {
        self.squares[frame] > self.quiet
    }

    fn is_speech(&self, frame: usize) -> bool {
        self.squares[frame] > self.speech
    }

    /// Whether the buffer is still the pause whose loudest frame was
    /// `level`. One frame over the floor breaks it. So does a run of frames
    /// well over the pause, as a distant voice makes without reaching the
    /// floor.
    fn within(&self, level: f32) -> bool {
        if self.loudest > SILENCE_FLOOR {
            return false;
        }
        let ceiling = (level * SPEECH_OVER_NOISE).clamp(QUIETEST_SPEECH, SILENCE_FLOOR);
        !self
            .squares
            .windows(PAUSE_BREAK_FRAMES)
            .any(|run| run.iter().all(|&square| square > ceiling))
    }

    /// Where the buffer's tail opens onto a real pause, as a sample offset
    /// into the buffer. Only a pause the buffer ends in qualifies: a quiet
    /// run followed by more speech is a dip inside a sentence, and cutting
    /// there would keep audio whose words were already delivered, which the
    /// next buffer would deliver again.
    fn trailing_pause(&self) -> Option<Pause> {
        let quiet = (0..self.squares.len())
            .rev()
            .take_while(|&frame| !self.is_loud(frame))
            .count();
        if quiet < TRIM_SILENCE_FRAMES {
            return None;
        }
        let start = self.squares.len() - quiet;
        Some(self.pause_in(start..self.squares.len()))
    }

    /// The pause that follows a word timed over the frames `word`. It
    /// opens right after speech, inside the timing or within
    /// `HEARD_MARGIN_FRAMES` past it, stays quiet through the timing's end,
    /// and holds no speech for long enough to trust before frame `until`.
    /// Quiet that speech breaks inside the timing comes before the word's
    /// sound, whose start was timed early. Quiet beside the loudest frame
    /// is not enough here: a soft word after a loud one reads as quiet, and
    /// a cut there splits it.
    fn pause_after(&self, word: std::ops::Range<usize>, until: usize) -> Option<Pause> {
        let until = until.min(self.squares.len());
        let quiet_from = |start: usize| {
            (start..until)
                .take_while(|&frame| !self.is_speech(frame))
                .count()
        };
        let opens = word.start + 1..word.end + HEARD_MARGIN_FRAMES + 1;
        let start = opens.take_while(|&frame| frame < until).find(|&frame| {
            let quiet = quiet_from(frame);
            self.is_speech(frame - 1) && quiet >= TRIM_SILENCE_FRAMES && frame + quiet >= word.end
        })?;
        Some(self.pause_in(start..start + quiet_from(start)))
    }

    /// The pause a quiet run of frames makes: cut at its midpoint.
    fn pause_in(&self, quiet: std::ops::Range<usize>) -> Pause {
        let at = quiet.start + quiet.len() / 2;
        Pause {
            at: at * TRIM_FRAME_SAMPLES,
            level: self.squares[at..quiet.end]
                .iter()
                .copied()
                .fold(0.0_f32, f32::max),
        }
    }

    /// Where speech no heard word accounts for begins, at or after `from`:
    /// the first of enough speech frames in a row outside every word's
    /// timing. Untimed words vouch for the whole buffer.
    ///
    /// Before the last heard word, only loud frames count. The model skipped
    /// past softer sound there to reach that word, so it is background. Past
    /// the last heard word, soft sound may be words the model has not caught
    /// yet.
    fn unheard_speech(&self, buffer_start: u64, words: &[Word], from: u64) -> Option<u64> {
        let frame =
            |position: u64| position.saturating_sub(buffer_start) as usize / TRIM_FRAME_SAMPLES;
        let mut heard = vec![false; self.squares.len()];
        for word in words {
            let span = word.span.as_ref()?;
            let first = frame(span.start).saturating_sub(HEARD_MARGIN_FRAMES);
            let last = (frame(span.end) + HEARD_MARGIN_FRAMES).min(heard.len());
            heard[first.min(last)..last].fill(true);
        }
        let past_last_word = heard
            .iter()
            .rposition(|&heard| heard)
            .map_or(0, |last| last + 1);
        let mut run = 0;
        for (index, &heard) in heard.iter().enumerate().skip(frame(from)) {
            let speech = if index < past_last_word {
                self.is_loud(index)
            } else {
                self.is_speech(index)
            };
            run = if speech && !heard { run + 1 } else { 0 };
            if run > UNHEARD_SPEECH_FRAMES {
                let first = index + 1 - run;
                return Some(buffer_start + (first * TRIM_FRAME_SAMPLES) as u64);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::audio::CANONICAL_SAMPLE_RATE;
    use crate::audio::testing::{Pace, paced, recorded};
    use crate::transcription::TimedWord;

    const STRIDE_MS: usize = 100;
    const STRIDE: usize = STRIDE_MS * CANONICAL_SAMPLE_RATE as usize / 1000;

    fn cadence(max_buffer_ms: usize) -> Cadence {
        Cadence::from_millis(STRIDE_MS, max_buffer_ms)
    }

    fn loud_batch() -> Vec<i16> {
        vec![8_192; STRIDE]
    }

    fn quiet_batch() -> Vec<i16> {
        vec![0; STRIDE]
    }

    /// Faint room noise just under SILENCE_FLOOR: a pause that is not the
    /// trivial all-zero one.
    fn faint_batch() -> Vec<i16> {
        vec![225; STRIDE]
    }

    /// One run's observable behavior: the segments in delivery order, the
    /// final transcript, the window every decode was handed, and how many
    /// steps it took.
    struct Run {
        segments: Vec<String>,
        /// Each segment's reading of the boundary before it.
        ended_before: Vec<Option<bool>>,
        final_transcript: Option<String>,
        windows: Vec<usize>,
        steps: usize,
    }

    impl Run {
        fn transcript(&self) -> &str {
            self.final_transcript
                .as_deref()
                .expect("the run finishes with a transcript")
        }
    }

    /// Drives `state` over `recording` to completion. With a pace, each
    /// decode releases the next batch, so a step's window is exactly one
    /// stride further than the last. `inspect` sees the state after every
    /// step, for invariants that hold between steps.
    fn run(
        mut state: AgreementState,
        recording: &mut RecordingReader,
        pace: Option<&Pace>,
        mut decode: impl FnMut(&[f32]) -> Hypothesis,
        mut inspect: impl FnMut(&AgreementState),
    ) -> Run {
        let cancel = CancelToken::new();
        let mut run = Run {
            segments: Vec::new(),
            ended_before: Vec::new(),
            final_transcript: None,
            windows: Vec::new(),
            steps: 0,
        };
        loop {
            let mut heard = |window: &[f32]| {
                run.windows.push(window.len());
                if let Some(pace) = pace {
                    pace.advance();
                }
                Ok(decode(window))
            };
            let step = state
                .next_finalized_with(recording, &cancel, &mut heard)
                .expect("every step succeeds");
            inspect(&state);
            let Some(step) = step else { break };
            run.steps += 1;
            if let Some(TranscriptUpdate::Finalized(segment)) = step.update {
                if let Some((span, window)) = segment.span().zip(segment.source_window()) {
                    assert!(
                        window.start <= span.start && span.end <= window.end,
                        "segment {span:?} lies outside its source window {window:?}"
                    );
                }
                run.ended_before.push(segment.sentence_ended_before());
                run.segments.push(segment.into_text());
            }
            // A trailing pending-tail step carries no transcript; like
            // Operation, keep the last one delivered rather than erase it.
            if let Some(transcript) = step.transcript {
                run.final_transcript = Some(transcript.into_text());
            }
        }
        run
    }

    #[test]
    fn previews_preserve_the_decode_window_after_a_buffer_cut() {
        let (mut recording, pace) = paced(vec![loud_batch(); 4]);
        let mut state = AgreementState::new(cadence(200));
        let mut decode = scripted(&["guest", "guest", "owner", "owner"]);
        let mut previews = Vec::new();
        while let Some(step) = state
            .next_with(&mut recording, &CancelToken::never(), true, |samples| {
                pace.advance();
                Ok(decode(samples))
            })
            .unwrap()
        {
            if let Some(TranscriptUpdate::Provisional {
                text,
                source_window,
            }) = step.update
                && !text.is_empty()
            {
                previews.push((text, source_window));
            }
        }
        assert_eq!(
            previews,
            [
                ("guest".into(), Some(0..STRIDE as u64)),
                ("owner".into(), Some(2 * STRIDE as u64..3 * STRIDE as u64)),
            ]
        );
    }

    #[test]
    fn previews_replace_the_accepted_tail_and_drain_after_final_segments() {
        let answers = ["turn lift", "turn left now", "", "turn left now"];
        let (mut recording, pace) = paced(vec![loud_batch(); answers.len()]);
        let mut state = AgreementState::new(cadence(20_000));
        let mut decode = scripted(&answers);
        let mut events = Vec::new();
        while let Some(step) = state
            .next_with(&mut recording, &CancelToken::never(), true, |samples| {
                pace.advance();
                Ok(decode(samples))
            })
            .expect("script succeeds")
        {
            match step.update {
                Some(TranscriptUpdate::Finalized(segment)) => {
                    events.push(format!("final:{}", segment.as_str()));
                }
                Some(TranscriptUpdate::Provisional { text: preview, .. }) => {
                    assert!(state.pending_segment.is_none());
                    events.push(format!("preview:{preview}"));
                }
                None => {}
            }
        }
        // The third decode changes nothing, and its preview still goes out.
        assert_eq!(
            events,
            [
                "preview:turn lift",
                "final:turn",
                "preview:left now",
                "preview:left now",
                "final:left now",
                "preview:",
            ]
        );
    }

    #[test]
    fn consuming_previews_preserves_decodes_segments_and_completion_through_trims() {
        for (cap, batches, answers) in [
            (
                20_000,
                vec![loud_batch(); 3],
                vec!["one two", "one two three", "one two three four"],
            ),
            (
                200,
                vec![loud_batch(); 4],
                vec!["one two", "one two three", "three four", "three four five"],
            ),
            (
                20_000,
                vec![quiet_batch(); 7],
                vec![
                    "one two", "one two", "one two", "one two", "one two", "one two", "one two",
                ],
            ),
            (
                20_000,
                vec![loud_batch(); 4],
                vec!["one two three", "one two", "", "one two four"],
            ),
        ] {
            let baseline = drive(cap, batches.clone(), &answers);
            let (mut recording, pace) = paced(batches);
            let mut state = AgreementState::new(cadence(cap));
            let mut decode = scripted(&answers);
            let mut segments = Vec::new();
            let mut windows = Vec::new();
            let mut completed = None;
            while let Some(step) = state
                .next_with(&mut recording, &CancelToken::never(), true, |samples| {
                    windows.push(samples.len());
                    pace.advance();
                    Ok(decode(samples))
                })
                .expect("script succeeds")
            {
                if matches!(step.update, Some(TranscriptUpdate::Provisional { .. })) {
                    assert!(state.pending_segment.is_none());
                }
                if let Some(TranscriptUpdate::Finalized(segment)) = step.update {
                    segments.push(segment.into_text());
                }
                if let Some(transcript) = step.transcript {
                    completed = Some(transcript.into_text());
                }
            }
            assert_eq!(segments, baseline.segments);
            assert_eq!(windows, baseline.windows);
            assert_eq!(completed, baseline.final_transcript);
        }
    }

    #[test]
    fn a_repeated_word_tail_is_republished_after_commitment() {
        let (mut recording, pace) = paced(vec![loud_batch(); 3]);
        let mut state = AgreementState::new(cadence(20_000));
        let mut decode = scripted(&["go", "go go", "go go home"]);
        let mut updates = Vec::new();
        for _ in 0..3 {
            let step = state
                .next_with(&mut recording, &CancelToken::never(), true, |samples| {
                    pace.advance();
                    Ok(decode(samples))
                })
                .expect("reads")
                .expect("update");
            updates.push(match step.update {
                Some(TranscriptUpdate::Finalized(segment)) => (Some(segment.into_text()), None),
                Some(TranscriptUpdate::Provisional { text: preview, .. }) => (None, Some(preview)),
                None => (None, None),
            });
        }
        assert_eq!(
            updates,
            [
                (None, Some("go".to_owned())),
                (Some("go".to_owned()), None),
                (None, Some("go".to_owned())),
            ]
        );
    }

    #[test]
    fn cancellation_does_not_deliver_a_queued_preview() {
        let (mut recording, pace) = paced(vec![loud_batch(); 3]);
        let mut state = AgreementState::new(cadence(20_000));
        let cancel = CancelToken::new();
        let mut decode = scripted(&["one two", "one two three", "one two three"]);
        for _ in 0..2 {
            state
                .next_with(&mut recording, &cancel, true, |samples| {
                    pace.advance();
                    Ok(decode(samples))
                })
                .expect("script succeeds");
        }
        assert!(state.pending_preview.is_some());
        cancel.cancel();
        assert!(matches!(
            state.next_with(&mut recording, &cancel, true, |_| {
                panic!("cancelled operation cannot decode")
            }),
            Err(TranscriptionError::Cancelled)
        ));
    }

    /// A decoder that answers from a script, one answer per decode.
    fn scripted(answers: &[&str]) -> impl FnMut(&[f32]) -> Hypothesis {
        let mut script: VecDeque<String> =
            answers.iter().map(|answer| answer.to_string()).collect();
        move |_| Hypothesis::new(script.pop_front().expect("the script covers every decode"))
    }

    /// A paced run: each decode releases the next batch.
    fn drive(max_buffer_ms: usize, batches: Vec<Vec<i16>>, answers: &[&str]) -> Run {
        let (mut recording, pace) = paced(batches);
        run(
            AgreementState::new(cadence(max_buffer_ms)),
            &mut recording,
            Some(&pace),
            scripted(answers),
            |_| {},
        )
    }

    /// A run over a recording closed before the first read: every sample is
    /// available at once, which is what a decode slower than its producer
    /// sees.
    fn drive_whole(samples: usize, max_buffer_ms: usize, answers: &[&str]) -> Run {
        let mut recording = recorded(&vec![0; samples]);
        run(
            AgreementState::new(cadence(max_buffer_ms)),
            &mut recording,
            None,
            scripted(answers),
            |_| {},
        )
    }

    mod agreement {
        use super::*;

        fn heard(list: &[&str]) -> Vec<Word> {
            list.iter()
                .map(|word| Word {
                    text: word.to_string(),
                    span: None,
                })
                .collect()
        }

        /// Agrees `words` twice, so the second decode confirms them.
        fn confirmed(agreement: &mut Agreement, words: Vec<Word>) -> Option<TranscriptSegment> {
            agreement.agree(words.clone(), None);
            agreement.agree(words, None).0
        }

        fn text_of(segment: Option<TranscriptSegment>) -> Option<String> {
            segment.map(TranscriptSegment::into_text)
        }

        fn opening_loss() -> [Vec<Word>; 2] {
            let decodes: [Vec<(String, std::ops::Range<u64>)>; 2] =
                serde_json::from_str(include_str!("opening-loss.json")).unwrap();
            decodes.map(|words| {
                words
                    .into_iter()
                    .map(|(text, span)| Word {
                        text,
                        span: Some(span),
                    })
                    .collect()
            })
        }

        #[test]
        fn an_omitted_opening_and_new_final_words_both_survive() {
            let [previous, current] = opening_loss();
            let suffix = text_of(segment_of(&current)).unwrap();
            let expected = format!("And to spread abroad its widening tale. {suffix}");
            for repetitions in [1, 2] {
                let mut agreement = Agreement::default();
                agreement.agree(previous.clone(), None);
                for _ in 0..repetitions {
                    let (segment, standing) = agreement.agree(current.clone(), None);
                    assert!(
                        segment.is_none(),
                        "retained words are not a second observation"
                    );
                    assert_ne!(standing, Standing::Settled);
                }
                assert_eq!(
                    text_of(agreement.take_tail(None)).as_deref(),
                    Some(expected.as_str())
                );
            }
        }

        #[test]
        fn a_later_complete_decode_can_correct_the_retained_opening() {
            let [previous, current] = opening_loss();
            let mut recovered = previous[..7].to_vec();
            recovered[6].text = "tail.".to_owned();
            recovered.extend(current.clone());
            let expected = text_of(segment_of(&recovered)).unwrap();
            let mut agreement = Agreement::default();
            agreement.agree(previous, None);
            agreement.agree(current, None);
            let (confirmed, _) = agreement.agree(recovered, None);
            let actual = format!(
                "{} {}",
                text_of(confirmed).unwrap(),
                text_of(agreement.take_tail(None)).unwrap()
            );
            assert_eq!(actual, expected);
        }

        #[test]
        fn a_revision_covering_the_earlier_audio_replaces_the_opening() {
            let [previous, mut current] = opening_loss();
            let shift =
                current[0].span.as_ref().unwrap().start - previous[0].span.as_ref().unwrap().start;
            for word in &mut current {
                let span = word.span.as_mut().unwrap();
                span.start -= shift;
                span.end -= shift;
            }
            let expected = text_of(segment_of(&current));
            let mut agreement = Agreement::default();
            agreement.agree(previous, None);
            agreement.agree(current.clone(), None);
            let (confirmed, standing) = agreement.agree(current, None);
            assert_eq!(text_of(confirmed), expected);
            assert_eq!(standing, Standing::Settled);
        }

        #[test]
        fn an_untimed_revision_does_not_restore_an_opening() {
            for previous_untimed in [false, true] {
                let [mut previous, mut current] = opening_loss();
                for word in if previous_untimed {
                    &mut previous
                } else {
                    &mut current
                } {
                    word.span = None;
                }
                let expected = text_of(segment_of(&current));
                let mut agreement = Agreement::default();
                agreement.agree(previous, None);
                agreement.agree(current.clone(), None);
                let (confirmed, _) = agreement.agree(current, None);
                assert_eq!(text_of(confirmed), expected);
            }
        }

        #[test]
        fn a_delivered_opening_is_not_repeated() {
            let [previous, current] = opening_loss();
            let suffix = text_of(segment_of(&current)).unwrap();
            let expected = format!("And to spread abroad its widening tale. {suffix}");
            let mut agreement = Agreement::default();
            agreement.agree(previous.clone(), None);
            let (first, _) = agreement.agree(previous, None);
            agreement.agree(current.clone(), None);
            let (second, _) = agreement.agree(current, None);
            assert_eq!(
                format!("{} {}", text_of(first).unwrap(), text_of(second).unwrap()),
                expected
            );
        }

        /// Back-to-back words, each longer than a timing's blur.
        fn timed(list: &[&str]) -> Vec<Word> {
            let word_samples = 10 * TRIM_FRAME_SAMPLES as u64;
            list.iter()
                .zip(0u64..)
                .map(|(word, index)| Word {
                    text: word.to_string(),
                    span: Some(index * word_samples..(index + 1) * word_samples),
                })
                .collect()
        }

        #[test]
        fn a_revised_word_in_a_delivered_slot_is_not_taken_for_a_dropped_one() {
            let mut agreement = Agreement::default();
            agreement.agree(timed(&["jumps", "over", "the", "the"]), None);
            let (first, _) = agreement.agree(timed(&["jumps", "over", "the", "the"]), None);
            agreement.agree(timed(&["jumps", "over", "the", "lazy", "dog"]), None);
            let (second, _) =
                agreement.agree(timed(&["jumps", "over", "the", "lazy", "dog"]), None);

            assert_eq!(text_of(first).as_deref(), Some("jumps over the the"));
            assert_eq!(text_of(second).as_deref(), Some("dog"));
        }

        #[test]
        fn a_timed_revision_at_eof_drains_only_audio_past_the_delivered_end() {
            let mut agreement = Agreement::default();
            let opening = timed(&["one", "two", "three", "four"]);
            confirmed(&mut agreement, opening);
            let mut revised = timed(&["different", "words", "replace", "the", "opening", "later"]);
            let through = agreement.delivered_through.unwrap();
            let early = (HEARD_MARGIN_FRAMES * TRIM_FRAME_SAMPLES) as u64 + 100;
            revised[4].span = Some(through - early..through + 100);
            agreement.agree(revised, None);
            assert_eq!(text_of(agreement.take_tail(None)).as_deref(), Some("later"));
        }

        #[test]
        fn a_recovered_word_that_drifted_early_is_still_new_speech() {
            let mut agreement = Agreement::default();
            let opening = timed(&["one", "two", "three", "four"]);
            confirmed(&mut agreement, opening);
            let mut revised = timed(&["different", "words", "replace", "the", "opening", "later"]);
            let through = agreement.delivered_through.unwrap();
            revised[4].span = Some(through - 100..through + 100);
            agreement.agree(revised, None);
            assert_eq!(
                text_of(agreement.take_tail(None)).as_deref(),
                Some("opening later")
            );
        }

        #[test]
        fn a_timed_recovery_does_not_reopen_audio_when_the_prefix_returns() {
            let mut agreement = Agreement::default();
            let opening = timed(&["one", "two", "three", "four"]);
            confirmed(&mut agreement, opening);
            let mut revised = timed(&[
                "a",
                "completely",
                "different",
                "opening",
                "new",
                "speech",
                "is",
                "here",
            ]);
            let early = (HEARD_MARGIN_FRAMES * TRIM_FRAME_SAMPLES) as u64 + 100;
            revised[3].span.as_mut().unwrap().start = agreement.delivered_through.unwrap() - early;
            assert!(agreement.agree(revised[4..].to_vec(), None).0.is_none());
            let (confirmed, _) = agreement.agree(revised.clone(), None);
            assert_eq!(text_of(confirmed).as_deref(), Some("new speech is here"));
            assert!(agreement.agree(revised, None).0.is_none());
            assert!(agreement.take_tail(None).is_none());
        }

        #[test]
        fn unplaceable_untimed_speech_does_not_repeat_delivered_audio() {
            let mut agreement = Agreement::default();
            let opening = heard(&["one", "two", "three", "four"]);
            confirmed(&mut agreement, opening);
            let revised = heard(&["a", "completely", "different", "opening", "new", "speech"]);
            for _ in 0..2 {
                assert!(agreement.agree(revised.clone(), None).0.is_none());
            }
            assert!(agreement.take_tail(None).is_none());
        }

        #[test]
        fn a_stable_delivered_prefix_survives_timing_drift() {
            // One frame of drift sits inside the timing margin; four lie beyond it.
            for drift in [TRIM_FRAME_SAMPLES as u64, 4 * TRIM_FRAME_SAMPLES as u64] {
                for finish_at_eof in [false, true] {
                    let mut agreement = Agreement::default();
                    let opening = timed(&["one", "two", "three", "four"]);
                    confirmed(&mut agreement, opening);
                    let mut continued = timed(&["one", "two", "three", "four", "five"]);
                    let boundary = continued[4].span.as_ref().unwrap().start - drift;
                    continued[3].span.as_mut().unwrap().end = boundary;
                    continued[4].span.as_mut().unwrap().start = boundary;
                    agreement.agree(continued.clone(), None);
                    let confirmed = if finish_at_eof {
                        agreement.take_tail(None)
                    } else {
                        agreement.agree(continued, None).0
                    };
                    assert_eq!(text_of(confirmed).as_deref(), Some("five"));
                    assert!(agreement.take_tail(None).is_none());
                }
            }
        }

        #[test]
        fn delivering_a_short_recovered_tail_does_not_reopen_the_discarded_prefix() {
            let mut agreement = Agreement::default();
            let opening = timed(&["one", "two", "three", "four"]);
            confirmed(&mut agreement, opening);
            let revised = timed(&[
                "new",
                "completely",
                "different",
                "opening",
                "new",
                "speech",
                "here",
            ]);
            agreement.agree(revised[4..].to_vec(), None);
            let (confirmed, _) = agreement.agree(revised[4..].to_vec(), None);
            assert_eq!(text_of(confirmed).as_deref(), Some("new speech here"));
            for _ in 0..2 {
                assert!(agreement.agree(revised.clone(), None).0.is_none());
            }
            assert!(agreement.take_tail(None).is_none());
        }

        #[test]
        fn a_short_timed_recovery_does_not_lower_the_delivered_audio_boundary() {
            let mut agreement = Agreement::default();
            let opening = timed(&["one", "two", "three", "four", "five", "six"]);
            confirmed(&mut agreement, opening);
            let revised = timed(&[
                "a",
                "different",
                "revised",
                "opening",
                "five",
                "six",
                "seven",
            ]);
            for length in [4, 5, 6, 7] {
                assert!(
                    agreement
                        .agree(revised[..length].to_vec(), None)
                        .0
                        .is_none()
                );
            }
            let (confirmed, _) = agreement.agree(revised, None);
            assert_eq!(text_of(confirmed).as_deref(), Some("seven"));
            assert!(agreement.take_tail(None).is_none());
        }

        #[test]
        fn a_delivered_anchor_after_recovery_allows_timing_drift() {
            let mut agreement = Agreement::default();
            let opening = timed(&["one", "two", "three", "four"]);
            confirmed(&mut agreement, opening);
            let revised = timed(&["a", "completely", "different", "opening", "new", "speech"]);
            confirmed(&mut agreement, revised);
            let mut continued = timed(&[
                "a",
                "completely",
                "different",
                "opening",
                "new",
                "speech",
                "continues",
            ]);
            let drifted_end = agreement.delivered_through.unwrap() - 2_048;
            continued[5].span.as_mut().unwrap().end = drifted_end;
            continued[6].span.as_mut().unwrap().start = drifted_end;
            agreement.agree(continued.clone(), None);
            let (confirmed, _) = agreement.agree(continued, None);
            assert_eq!(text_of(confirmed).as_deref(), Some("continues"));
            assert!(agreement.take_tail(None).is_none());
        }

        #[test]
        fn a_recovered_delivered_anchor_survives_loss_and_return_of_timings() {
            let mut agreement = Agreement::default();
            let opening = timed(&["one", "two", "three", "four"]);
            confirmed(&mut agreement, opening);
            let revised = timed(&["a", "completely", "different", "opening", "new", "speech"]);
            let (first, standing) = agreement.agree(revised.clone(), None);
            assert!(first.is_none());
            assert_ne!(standing, Standing::Settled);
            assert_eq!(agreement.preview(), "new speech");
            let (confirmed, _) = agreement.agree(revised, None);
            assert_eq!(text_of(confirmed).as_deref(), Some("new speech"));

            let untimed = heard(&[
                "a",
                "completely",
                "different",
                "opening",
                "new",
                "speech",
                "continues",
            ]);
            agreement.agree(untimed.clone(), None);
            let (confirmed, _) = agreement.agree(untimed, None);
            assert_eq!(text_of(confirmed).as_deref(), Some("continues"));

            let timed = timed(&[
                "a",
                "completely",
                "different",
                "opening",
                "new",
                "speech",
                "continues",
                "later",
            ]);
            agreement.agree(timed.clone(), None);
            let (confirmed, _) = agreement.agree(timed, None);
            assert_eq!(text_of(confirmed).as_deref(), Some("later"));
            assert!(agreement.take_tail(None).is_none());
        }

        #[test]
        fn an_untimed_candidate_cannot_anchor_a_discarded_prefix() {
            let mut agreement = Agreement::default();
            let opening = timed(&["one", "two", "three", "four"]);
            confirmed(&mut agreement, opening);
            agreement.agree(
                timed(&["a", "completely", "different", "opening", "new", "speech"]),
                None,
            );
            let untimed = heard(&[
                "a",
                "completely",
                "different",
                "opening",
                "new",
                "speech",
                "continues",
            ]);
            for _ in 0..2 {
                assert!(agreement.agree(untimed.clone(), None).0.is_none());
            }
            assert_eq!(
                text_of(agreement.take_tail(None)).as_deref(),
                Some("new speech")
            );
        }

        #[test]
        fn a_phrase_said_again_in_a_fresh_buffer_is_new_speech() {
            let mut agreement = Agreement::default();
            agreement.agree(timed(&["a", "b", "c", "d"]), None);
            agreement.agree(timed(&["a", "b", "c", "d"]), None);
            let mut agreement = agreement.next_buffer();

            // The fresh buffer starts after "d" and hears the phrase again.
            let later = timed(&["a", "b", "c", "d", "e", "f", "g", "a", "b", "c", "d", "h"]);
            let later = later[4..].to_vec();
            agreement.agree(later.clone(), None);
            let (confirmed, _) = agreement.agree(later, None);

            assert_eq!(text_of(confirmed).as_deref(), Some("e f g a b c d h"));
        }

        #[test]
        fn agreement_delivers_a_confirmed_prefix_once() {
            let mut agreement = Agreement::default();
            let (first, standing) = agreement.agree(heard(&["one", "two"]), None);
            assert_eq!(text_of(first), None, "one decode confirms nothing");
            assert_ne!(standing, Standing::Settled);

            let (second, standing) = agreement.agree(heard(&["one", "two", "three"]), None);
            assert_eq!(text_of(second).as_deref(), Some("one two"));
            assert_ne!(
                standing,
                Standing::Settled,
                "a decode that grew has not settled"
            );

            let (third, standing) = agreement.agree(heard(&["one", "two", "three"]), None);
            assert_eq!(text_of(third).as_deref(), Some("three"));
            assert_eq!(standing, Standing::Settled, "two identical decodes settle");
            assert_eq!(text_of(agreement.take_tail(None)), None);
        }

        #[test]
        fn an_empty_decode_keeps_the_last_one_in_force() {
            let mut agreement = Agreement::default();
            agreement.agree(heard(&["one", "two"]), None);
            let (confirmed, standing) = agreement.agree(heard(&[]), None);
            assert_eq!(text_of(confirmed), None);
            assert_ne!(standing, Standing::Settled);
            assert_eq!(
                text_of(agreement.take_tail(None)).as_deref(),
                Some("one two")
            );
        }

        #[test]
        fn lcp_stops_at_the_first_disagreement_ignoring_case_and_punctuation() {
            assert_eq!(lcp(&[], &heard(&["any"])), 0);
            assert_eq!(lcp(&heard(&["Hello,"]), &heard(&["hello"])), 1);
            assert_eq!(
                lcp(
                    &heard(&["one", "two", "three"]),
                    &heard(&["one", "too", "three"])
                ),
                1
            );
            assert_eq!(
                lcp(&heard(&["one", "two"]), &heard(&["one", "two", "three"])),
                2
            );
        }
    }

    mod state {
        use super::*;

        #[test]
        fn an_untimed_phrase_repeated_in_the_tail_is_kept() {
            let run = drive(
                20_000,
                vec![loud_batch(); 3],
                &["a b c d", "a b c d", "a b c d a b c d"],
            );

            assert_eq!(run.transcript(), "a b c d a b c d");
        }

        #[test]
        fn agreement_commits_only_the_shared_prefix() {
            let run = drive(
                5_000,
                vec![loud_batch(); 3],
                &[
                    "alpha beta gamma",
                    "alpha beta delta epsilon",
                    "alpha beta delta epsilon zeta",
                ],
            );

            // The third word of the first decode was revised away, so only the
            // two words both decodes heard are committed; the rest waits for a
            // second opinion or for the end.
            assert_eq!(run.segments, ["alpha beta", "delta epsilon", "zeta"]);
            assert_eq!(run.transcript(), "alpha beta delta epsilon zeta");
            assert_eq!(run.windows, [STRIDE, 2 * STRIDE, 3 * STRIDE]);
        }

        #[test]
        fn decodes_sharing_nothing_publish_nothing_until_the_end() {
            let run = drive(
                5_000,
                vec![loud_batch(); 3],
                &["one two", "three four", "five six"],
            );

            assert_eq!(run.segments, ["five six"]);
            assert_eq!(run.transcript(), "five six");
        }

        #[test]
        fn a_revision_inside_the_emitted_prefix_emits_nothing_and_does_not_panic() {
            let run = drive(
                5_000,
                vec![loud_batch(); 3],
                &["a b c", "a b c d", "a x y z"],
            );

            // After the revision the buffer's indices sit behind what was
            // delivered, so the tail commit loses one word.
            assert_eq!(run.segments, ["a b c", "z"]);
            assert_eq!(run.transcript(), "a b c z");
        }

        #[test]
        fn a_word_inserted_before_the_delivered_words_does_not_repeat_them() {
            let run = drive(
                5_000,
                vec![loud_batch(); 4],
                &[
                    "the quick brown fox jumps",
                    "the quick brown fox jumps",
                    "the quick brown red fox jumps over",
                    "the quick brown red fox jumps over",
                ],
            );

            assert_eq!(run.segments, ["the quick brown fox jumps", "over"]);
            assert_eq!(run.transcript(), "the quick brown fox jumps over");
        }

        #[test]
        fn a_word_dropped_before_the_delivered_words_does_not_hide_the_next() {
            let run = drive(
                5_000,
                vec![loud_batch(); 4],
                &[
                    "the quick brown fox jumps",
                    "the quick brown fox jumps",
                    "the quick brown jumps over",
                    "the quick brown jumps over",
                ],
            );

            assert_eq!(run.segments, ["the quick brown fox jumps", "over"]);
            assert_eq!(run.transcript(), "the quick brown fox jumps over");
        }

        #[test]
        fn a_decode_that_drops_most_of_its_window_is_ignored() {
            let run = drive(
                400,
                vec![loud_batch(), loud_batch(), loud_batch(), loud_batch()],
                &[
                    "alpha beta",
                    "alpha beta gamma",
                    "alpha beta gamma delta",
                    "zeta",
                ],
            );
            assert_eq!(run.segments, ["alpha beta", "gamma", "delta"]);
        }

        #[test]
        fn an_empty_decode_does_not_poison_the_next_step() {
            let run = drive(
                5_000,
                vec![loud_batch(); 3],
                &["alpha beta", "", "alpha beta gamma"],
            );

            assert_eq!(run.segments, ["alpha beta", "gamma"]);
            assert_eq!(run.transcript(), "alpha beta gamma");
        }

        #[test]
        fn the_end_commits_a_tail_that_never_got_a_second_decode() {
            let run = drive(5_000, vec![loud_batch()], &["never confirmed"]);

            assert_eq!(run.segments, ["never confirmed"]);
            assert_eq!(run.transcript(), "never confirmed");
            assert_eq!(run.windows, [STRIDE]);
        }

        #[test]
        fn a_silent_recording_finishes_without_a_transcript_instead_of_failing() {
            let mut recording = recorded(&vec![0; STRIDE * 4]);
            let run = run(
                AgreementState::new(cadence(20_000)),
                &mut recording,
                None,
                |_| Hypothesis::new(""),
                |_| {},
            );

            assert_eq!(run.steps, 1, "the finishing step is delivered once");
            assert!(run.segments.is_empty(), "silence yields no segment");
            assert_eq!(run.final_transcript, None, "silence yields no transcript");
        }

        #[test]
        fn a_settled_trim_opens_the_next_buffer_at_the_pause() {
            let mut batches = vec![loud_batch()];
            batches.extend(vec![quiet_batch(); 5]);
            batches.push(loud_batch());
            batches.push(quiet_batch());
            let run = drive(
                5_000,
                batches,
                &[
                    "one",
                    "one two",
                    "one two three",
                    "one two three four",
                    "one two three four five",
                    "one two three four five",
                    "six seven",
                    "six seven",
                ],
            );

            // The pause behind "five" spans fourteen full frames, so the settled
            // trim cuts at its midpoint: (4 + 14 / 2) * 512. The next decode then
            // reads from there instead of the buffer's start, and no word repeats.
            let cut = (4 + 7) * 512;
            assert_eq!(
                run.windows,
                [
                    STRIDE,
                    2 * STRIDE,
                    3 * STRIDE,
                    4 * STRIDE,
                    5 * STRIDE,
                    6 * STRIDE,
                    7 * STRIDE - cut,
                    8 * STRIDE - cut,
                ]
            );
            assert_eq!(
                run.segments,
                ["one", "two", "three", "four", "five", "six seven"]
            );
            assert_eq!(run.transcript(), "one two three four five six seven");
        }

        #[test]
        fn a_decode_that_comes_back_short_does_not_reopen_delivered_words() {
            // The third decode drops "c", which the second had already
            // delivered. Following it down would leave the fifth decode
            // offering "c" again.
            let mut batches = vec![quiet_batch(); 5];
            batches.push(loud_batch());
            let run = drive(
                20_000,
                batches,
                &["a b c", "a b c x", "a b", "a b c d", "a b c d", "e f"],
            );

            // The fifth decode settles and cuts the buffer; "e f" is the next
            // one, which is why it arrives whole.
            assert_eq!(run.segments, ["a b c", "d", "e f"]);
            assert!(
                !run.transcript().contains("c c"),
                "transcript repeated a word: {:?}",
                run.transcript()
            );
        }

        #[test]
        fn a_cap_forced_cut_commits_every_word_exactly_once() {
            let run = drive_whole(4 * STRIDE, 300, &["p q r s t", ""]);

            assert_eq!(run.segments, ["p q r s t"]);
            assert_eq!(run.transcript(), "p q r s t");
            assert_eq!(run.windows, [3 * STRIDE, STRIDE]);
        }

        #[test]
        fn a_pending_segment_keeps_its_audio_from_being_released() {
            let mut state = AgreementState::new(cadence(5_000));
            // What a cap-forced trim leaves behind: the buffer has moved past a
            // segment nobody has been handed yet.
            state.buffer_start = 10 * STRIDE as u64;
            let mut pending = TranscriptSegment::spanned(
                &["alpha".to_owned()],
                Some(STRIDE as u64..2 * STRIDE as u64),
            )
            .unwrap();
            pending.source_window = Some(STRIDE as u64..2 * STRIDE as u64);
            state.pending_segment = Some(pending);
            state.untimed = true;
            assert_eq!(state.retained_from(), STRIDE as u64);

            state.pending_segment = None;
            assert_eq!(state.retained_from(), 10 * STRIDE as u64);
        }

        #[test]
        fn an_untimed_cap_tail_keeps_the_decode_window_after_trim() {
            let mut state = AgreementState::new(cadence(5_000));
            state.untimed = true;
            state.buffer_start = STRIDE as u64;
            state.decoded_end = 10 * STRIDE as u64;
            let confirmed = state.commit(TranscriptSegment::spanned(&["owner".into()], None));
            let tail = state.commit(TranscriptSegment::spanned(&["guest".into()], None));
            let agreement = state.agreement.next_buffer();
            let tail = state.trim(state.decoded_end, agreement, tail, None);
            let first = state.one_at_a_time(confirmed, tail).unwrap();
            assert_eq!(first.span(), None);
            assert_eq!(
                first.source_window(),
                Some(STRIDE as u64..10 * STRIDE as u64)
            );
            assert_eq!(state.retained_from(), STRIDE as u64);
            let pending = state.pending_segment.as_ref().unwrap();
            assert_eq!(pending.source_window(), first.source_window());
            assert_eq!(pending.span(), None);
        }

        #[test]
        fn a_decode_behind_its_producer_queues_no_backlog() {
            let whole = 2 * CANONICAL_SAMPLE_RATE as usize;
            let run = drive_whole(whole, 20_000, &["whole buffer"]);

            // The first decode jumps straight to everything captured, so no
            // second pass over stale audio can queue.
            assert_eq!(run.windows, [whole]);
            assert_eq!(run.segments, ["whole buffer"]);
            assert_eq!(run.transcript(), "whole buffer");
        }

        #[test]
        fn a_finished_recording_longer_than_the_cap_decodes_in_capped_windows() {
            let max_buffer_ms = 5_000;
            let max_window =
                canonical_samples(Duration::from_millis(max_buffer_ms as u64)) as usize;
            let whole = 3 * max_window;
            let run = drive_whole(whole, max_buffer_ms, &["one", "two", "three"]);

            assert_eq!(run.windows, [max_window, max_window, max_window]);
            assert_eq!(run.segments, ["one", "two", "three"]);
            assert_eq!(run.transcript(), "one two three");
        }

        #[test]
        fn segments_carry_absolute_sample_spans_from_word_timings() {
            let mut recording = recorded(&vec![0_i16; 16_000]);
            let mut state = AgreementState::new(cadence(5_000));
            let cancel = CancelToken::new();
            let decode = |_window: &[f32]| {
                Ok(Hypothesis::with_words(
                    "alpha beta",
                    vec![
                        TimedWord {
                            start: 0.0,
                            end: 0.4,
                        },
                        TimedWord {
                            start: 0.5,
                            end: 0.9,
                        },
                    ],
                ))
            };

            let step = state
                .next_finalized_with(&mut recording, &cancel, decode)
                .expect("the step succeeds")
                .expect("a closed recording finishes in one step");
            let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
                panic!("the tail is delivered");
            };

            assert_eq!(segment.as_str(), "alpha beta");
            let span = segment.span.expect("timed words produce a span");
            assert_eq!(span.start, 0);
            // 0.9 s at 16 kHz.
            assert_eq!(span.end, 14_400);
        }

        #[test]
        fn a_decode_with_impossible_timings_is_heard_untimed() {
            let mut recording = RecordingReader::from_samples(&[0.5; 16_000]);
            let mut state = AgreementState::new(cadence(5_000));

            let step = state
                .next_finalized_with(&mut recording, &CancelToken::never(), |_| {
                    Ok(Hypothesis::with_words(
                        "hello",
                        vec![TimedWord {
                            start: f32::NAN,
                            end: 0.5,
                        }],
                    ))
                })
                .expect("an impossible timing does not fail the decode")
                .expect("the tail is delivered");
            let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
                panic!("the tail is delivered");
            };
            assert_eq!(segment.as_str(), "hello");
            assert!(segment.word_spans().is_none());
        }

        #[test]
        fn final_word_timings_stay_inside_the_decoded_audio() {
            let timing = TimedWord {
                start: 0.8,
                end: 1.1,
            };
            let mut recording = RecordingReader::from_samples(&vec![0.5; 16_000]);
            let mut state = AgreementState::new(cadence(5_000));
            let step = state
                .next_finalized_with(&mut recording, &CancelToken::never(), |_| {
                    Ok(Hypothesis::with_words("tail", vec![timing]))
                })
                .unwrap()
                .unwrap();
            let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
                panic!("the tail is delivered");
            };
            let span = segment.word_spans().unwrap()[0].clone();
            assert!(span.start < 16_000 && span.end <= 16_000, "{span:?}");
        }

        #[test]
        fn a_hypothesis_without_timings_yields_spanless_segments() {
            let mut recording = recorded(&vec![0_i16; 2 * STRIDE]);
            let mut state = AgreementState::new(cadence(5_000));
            let cancel = CancelToken::new();
            let decode = |_window: &[f32]| Ok(Hypothesis::new("no timings here"));

            let step = state
                .next_finalized_with(&mut recording, &cancel, decode)
                .expect("the step succeeds")
                .expect("a closed recording finishes in one step");
            let Some(TranscriptUpdate::Finalized(segment)) = step.update else {
                panic!("the tail is delivered");
            };

            assert_eq!(segment.as_str(), "no timings here");
            assert!(segment.span().is_none(), "untimed words carry no span");
        }

        #[test]
        fn delivered_segments_concatenate_into_the_final_transcript_across_both_trims() {
            // Six growing decodes settle onto the pause behind "five" and trim
            // without a forced commit. Twelve more grow across fresh buffers
            // until the cap cuts mid-sentence, committing the unconfirmed tail
            // "nineteen". Either way, delivery is the record.
            let mut answers: Vec<String> = vec![
                "one".into(),
                "one two".into(),
                "one two three".into(),
                "one two three four".into(),
                "one two three four five".into(),
                "one two three four five".into(),
            ];
            let mut running = String::new();
            for word in [
                "six",
                "seven",
                "eight",
                "nine",
                "ten",
                "eleven",
                "twelve",
                "thirteen",
                "fourteen",
                "fifteen",
                "sixteen",
                "seventeen",
            ] {
                running.push_str(if running.is_empty() { "" } else { " " });
                running.push_str(word);
                answers.push(running.clone());
            }
            answers.push(format!("{running} eighteen nineteen"));
            // The audio after the cap's cut decodes as its own window.
            answers.push(String::new());
            let answers: Vec<&str> = answers.iter().map(String::as_str).collect();

            let mut batches = vec![loud_batch()];
            batches.extend(vec![quiet_batch(); 5]);
            batches.push(loud_batch());
            batches.extend(vec![loud_batch(); 12]);
            let run = drive(1_500, batches, &answers);

            let delivered = run.segments.join(" ");
            assert_eq!(delivered, run.transcript());
            assert_eq!(
                delivered,
                "one two three four five six seven eight nine ten eleven twelve thirteen \
                 fourteen fifteen sixteen seventeen eighteen nineteen"
            );
        }
    }

    mod trimming {
        use super::*;

        fn loud_frames(count: usize) -> Vec<f32> {
            frames_at(0.5, count)
        }

        fn frames_at(level: f32, count: usize) -> Vec<f32> {
            vec![level; count * TRIM_FRAME_SAMPLES]
        }

        /// A word's text, level, and start and end in seconds.
        type Spoken = (&'static str, i16, f32, f32);

        fn sample_at(seconds: f32) -> usize {
            (seconds * CANONICAL_SAMPLE_RATE as f32) as usize
        }

        fn seconds_at(sample: usize) -> f32 {
            sample as f32 / CANONICAL_SAMPLE_RATE as f32
        }

        /// `seconds` of silence with each word at its level.
        fn spoken_audio(spoken: &[Spoken], seconds: f32) -> Vec<i16> {
            let mut audio = vec![0_i16; sample_at(seconds)];
            for &(_, level, start, end) in spoken {
                audio[sample_at(start)..sample_at(end)].fill(level);
            }
            audio
        }

        fn whole_hypothesis(spoken: &[Spoken], window: &[f32]) -> Hypothesis {
            let (words, timings) = whole_words(spoken, window);
            Hypothesis::with_words(words.join(" "), timings)
        }

        /// A paced run over `seconds` of `spoken`: each decode hears one
        /// stride more.
        fn replay(
            cadence: Cadence,
            spoken: &[Spoken],
            seconds: f32,
            decode: impl FnMut(&[f32]) -> Hypothesis,
        ) -> Run {
            let audio = spoken_audio(spoken, seconds);
            let (mut recording, pace) = paced(strides_of(cadence, &audio));
            run(
                AgreementState::new(cadence),
                &mut recording,
                Some(&pace),
                decode,
                |_| {},
            )
        }

        fn strides(audio: &[i16]) -> Vec<Vec<i16>> {
            audio.chunks(STRIDE).map(<[i16]>::to_vec).collect()
        }

        fn strides_of(cadence: Cadence, audio: &[i16]) -> Vec<Vec<i16>> {
            audio
                .chunks(cadence.stride_samples())
                .map(<[i16]>::to_vec)
                .collect()
        }

        /// The words whose audio lies whole inside `window`, timed from its
        /// opening.
        fn whole_words(spoken: &[Spoken], window: &[f32]) -> (Vec<&'static str>, Vec<TimedWord>) {
            let mut words = Vec::new();
            let mut timings = Vec::new();
            for &(text, level, start, end) in spoken {
                let level = f32::from(level) / crate::audio::PCM_SCALE;
                let Some(opens) = window.iter().position(|&sample| sample == level) else {
                    continue;
                };
                let length = window[opens..]
                    .iter()
                    .take_while(|&&sample| sample == level)
                    .count();
                if length == sample_at(end) - sample_at(start) && opens + length < window.len() {
                    words.push(text);
                    timings.push(TimedWord {
                        start: seconds_at(opens),
                        end: seconds_at(opens + length),
                    });
                }
            }
            (words, timings)
        }

        fn timed_word(first_frame: usize, frames: usize) -> Word {
            let start = (first_frame * TRIM_FRAME_SAMPLES) as u64;
            Word {
                text: "word".to_owned(),
                span: Some(start..start + (frames * TRIM_FRAME_SAMPLES) as u64),
            }
        }

        #[test]
        fn a_pause_no_stride_ends_in_is_cut_and_keeps_the_words_heard_past_it() {
            // At a 500 ms stride, the decode that confirms "one" already hears
            // "two", so no settled decode ends inside the pause between them.
            const SPOKEN: [Spoken; 3] = [
                ("one", 8_000, 0.2, 0.5),
                ("two", 7_000, 1.0, 1.2),
                ("three", 6_000, 1.3, 1.5),
            ];
            let cadence = Cadence::from_millis(500, 20_000);
            let audio = spoken_audio(&SPOKEN, 2.5);
            let (mut recording, pace) = paced(strides_of(cadence, &audio));
            let mut delivered = Vec::new();
            let run = run(
                AgreementState::new(cadence),
                &mut recording,
                Some(&pace),
                |window| whole_hypothesis(&SPOKEN, window),
                |state| delivered.push((state.decoded_end, state.transcript.len())),
            );

            // The pause runs from frame 16, past the frame "one" ends in, to
            // frame 31, where "two" begins. The cut lands at its midpoint.
            let cut = (16 + 15 / 2) * TRIM_FRAME_SAMPLES;
            assert_eq!(run.windows, [8_000, 16_000, 24_000, 32_000, 40_000 - cut]);
            assert_eq!(run.segments, ["one", "two", "three"]);
            assert!(
                delivered.contains(&(32_000, 2)),
                "the decode after the cut confirms the word heard past the pause"
            );
        }

        #[test]
        fn softer_speech_after_a_pause_is_preserved() {
            let mut batches = vec![loud_batch(); 3];
            batches.extend(vec![quiet_batch(); 6]);
            batches.extend(vec![vec![1_500; STRIDE]; 6]);
            let run = drive(
                20_000,
                batches,
                &[
                    "a", "a b", "a b c", "a b c", "a b c", "a b c", "a b c", "a b c", "a b c", "d",
                    "d e", "d e f", "d e f", "d e f", "d e f",
                ],
            );

            assert_eq!(run.transcript(), "a b c d e f");
            assert_eq!(run.segments.join(" "), run.transcript());
        }

        #[test]
        fn a_buffer_after_a_cut_hears_its_lookback_and_reads_the_boundary() {
            // Alone, the second buffer is a sentence: "Two." With "one"
            // before it, the model hears the sentence go on. The words
            // before the cut are not delivered again.
            const SPOKEN: [Spoken; 2] = [("one", 8_000, 0.0, 0.4), ("two", 7_000, 1.5, 1.9)];
            let run = replay(cadence(20_000), &SPOKEN, 3.0, |window| {
                let (words, timings) = whole_words(&SPOKEN, window);
                let text = match words.as_slice() {
                    ["one"] => "one.",
                    ["one", "two"] => "one two",
                    ["two"] => "Two.",
                    _ => "",
                };
                Hypothesis::with_words(text, timings)
            });

            // Only a decode that heard "one" too writes "two" that way.
            assert_eq!(run.segments, ["one.", "two"]);
            assert_eq!(run.ended_before, [None, Some(false)]);
        }

        #[test]
        fn a_buffer_word_the_lookback_decode_times_before_the_cut_goes_out() {
            // Engines time the first word after a pause early. The decode
            // that hears "one" again opens "two" where "one" ends, before
            // the cut, yet "two" follows the delivered word.
            const SPOKEN: [Spoken; 2] = [("one", 8_000, 0.0, 0.4), ("two", 7_000, 1.5, 1.9)];
            let run = replay(cadence(20_000), &SPOKEN, 3.0, |window| {
                let (words, mut timings) = whole_words(&SPOKEN, window);
                if words == ["one", "two"] {
                    timings[1].start = timings[0].end;
                }
                Hypothesis::with_words(words.join(" "), timings)
            });

            assert_eq!(run.segments, ["one", "two"]);
        }

        #[test]
        fn a_reworded_last_delivered_phrase_does_not_split_at_an_earlier_copy() {
            // Once "new" is heard, the decode merges the second "that was"
            // into "that's". The first "that was" reads the same as the
            // delivered one, and sounds a second before it.
            const SPOKEN: [Spoken; 7] = [
                ("that", 8_000, 0.0, 0.3),
                ("was", 7_900, 0.4, 0.7),
                ("there", 7_800, 0.8, 1.1),
                ("that", 7_600, 1.5, 1.7),
                ("was", 7_500, 1.8, 2.1),
                ("new", 7_400, 3.0, 3.3),
                ("words", 7_300, 3.4, 3.7),
            ];
            let run = replay(cadence(20_000), &SPOKEN, 5.0, |window| {
                let (mut words, mut timings) = whole_words(&SPOKEN, window);
                if let Some(that) = words.iter().rposition(|word| *word == "that")
                    && words.contains(&"new")
                {
                    words[that] = "that's";
                    words.remove(that + 1);
                    timings[that].end = timings.remove(that + 1).end;
                }
                Hypothesis::with_words(words.join(" "), timings)
            });

            assert_eq!(run.transcript(), "that was there that was new words");
        }

        #[test]
        fn speech_the_model_skipped_after_a_cut_holds_the_words_after_it() {
            // The cut comes when 140 ms of "missing" has arrived, too little
            // to count as skipped. The buffer it opens must still notice
            // the word, or "two" goes out ahead of it.
            const SPOKEN: [Spoken; 4] = [
                ("one", 8_000, 0.2, 0.5),
                ("missing", 7_000, 1.36, 1.9),
                ("two", 6_000, 2.05, 2.25),
                ("three", 5_000, 2.35, 2.55),
            ];
            let cadence = Cadence::from_millis(500, 20_000);
            let audio = spoken_audio(&SPOKEN, 5.0);
            let (mut recording, pace) = paced(strides_of(cadence, &audio));
            let mut heard_to = 0;
            let mut cut_at = None;
            let run = run(
                AgreementState::new(cadence),
                &mut recording,
                Some(&pace),
                |window| {
                    heard_to += cadence.stride_samples();
                    let (mut words, mut timings) = whole_words(&SPOKEN, window);
                    if heard_to < sample_at(3.5)
                        && let Some(skipped) = words.iter().position(|word| *word == "missing")
                    {
                        words.remove(skipped);
                        timings.remove(skipped);
                    }
                    Hypothesis::with_words(words.join(" "), timings)
                },
                |state| {
                    if state.buffer_start > 0 {
                        cut_at.get_or_insert(state.decoded_end);
                    }
                },
            );

            assert_eq!(cut_at, Some(sample_at(1.5) as u64));
            assert_eq!(run.transcript(), "one missing two three");
        }

        #[test]
        fn a_word_waiting_from_before_the_buffer_keeps_its_audio() {
            // The lookback decode times "two" where "one" ends, before the
            // cut. The next decode is untimed, and capture ends with no
            // more audio, so "two" goes out as the timed decode heard it.
            const SPOKEN: [Spoken; 2] = [("one", 8_000, 0.0, 0.4), ("two", 7_000, 1.5, 1.9)];
            let mut batches = strides(&spoken_audio(&SPOKEN, 2.1));
            batches.push(Vec::new());
            let (mut recording, pace) = paced(batches);
            let mut state = AgreementState::new(cadence(20_000));
            let mut heard_two = false;
            let mut untimed = false;
            let mut released_to = 0;
            let mut segments = Vec::new();
            while let Some(step) = state
                .next_with(&mut recording, &CancelToken::never(), true, |window| {
                    let (words, mut timings) = whole_words(&SPOKEN, window);
                    if words == ["one", "two"] {
                        if std::mem::replace(&mut heard_two, true) {
                            untimed = true;
                            return Ok(Hypothesis::new("one two"));
                        }
                        timings[1].start = timings[0].end;
                    }
                    pace.advance();
                    Ok(Hypothesis::with_words(words.join(" "), timings))
                })
                .unwrap()
            {
                if let Some(TranscriptUpdate::Finalized(segment)) = step.update {
                    let span = segment.span().expect("both words are timed");
                    assert!(
                        released_to <= span.start,
                        "segment {span:?} opens in audio released through {released_to}"
                    );
                    segments.push(segment.into_text());
                }
                // A reader releases audio on every update, as the remote
                // client does.
                released_to = state.retained_from();
                if std::mem::take(&mut untimed) {
                    pace.advance();
                }
            }

            assert_eq!(segments, ["one", "two"]);
        }

        #[test]
        fn a_later_copy_of_the_last_delivered_word_is_not_taken_for_it() {
            // The delivered "now" is timed on into the pause, and the
            // lookback decode times the later "now" back into it, so both
            // sound where the delivered one did. Only the first follows
            // "we should go".
            const SPOKEN: [Spoken; 9] = [
                ("we", 8_000, 0.1, 0.3),
                ("should", 7_900, 0.4, 0.7),
                ("go", 7_800, 0.8, 1.0),
                ("now", 7_700, 1.2, 1.4),
                ("yes", 7_600, 3.1, 3.3),
                ("I", 7_500, 3.35, 3.45),
                ("think", 7_400, 3.5, 3.7),
                ("now", 7_300, 3.8, 4.0),
                ("works", 7_200, 4.1, 4.4),
            ];
            let cadence = Cadence::from_millis(1_000, 20_000);
            let run = replay(cadence, &SPOKEN, 6.0, |window| {
                let (words, mut timings) = whole_words(&SPOKEN, window);
                if words == ["we", "should", "go", "now"] {
                    timings[3].end += 0.8;
                }
                if words.first() == Some(&"we") && words.len() > 7 {
                    timings[7].start = 2.1;
                }
                Hypothesis::with_words(words.join(" "), timings)
            });

            assert_eq!(run.transcript(), "we should go now yes I think now works");
        }

        #[test]
        fn a_pause_inside_the_last_delivered_words_timing_is_not_cut() {
            // "two" is timed from where "one" ends, across the silence
            // before its sound. A cut in that silence leaves the sound in
            // the next buffer, whose decode hears it as "too".
            const SPOKEN: [Spoken; 5] = [
                ("one", 8_000, 0.1, 0.4),
                ("two", 7_000, 1.15, 1.35),
                ("three", 6_000, 3.3, 3.5),
                ("four", 5_000, 3.6, 3.8),
                ("five", 4_000, 3.9, 4.1),
            ];
            let cadence = Cadence::from_millis(1_000, 20_000);
            let run = replay(cadence, &SPOKEN, 6.0, |window| {
                let (mut words, mut timings) = whole_words(&SPOKEN, window);
                if words.contains(&"three") {
                    if let Some(two) = words.iter().position(|word| *word == "two") {
                        words[two] = "too";
                    }
                } else if words.len() == 2 {
                    timings[1].start = timings[0].end;
                }
                Hypothesis::with_words(words.join(" "), timings)
            });

            assert_eq!(run.transcript(), "one two three four five");
        }

        /// Which gap between words each pause cut lands in, and the
        /// transcript, at Parakeet's 500 ms stride. Decode `late` runs
        /// while two strides arrive.
        fn pauses_cut(spoken: &[Spoken], late: Option<usize>) -> (Vec<usize>, String) {
            let cadence = Cadence::from_millis(500, 20_000);
            let audio = spoken_audio(spoken, 9.0);
            let (mut recording, pace) = paced(strides_of(cadence, &audio));
            let mut step = 0;
            let mut gaps = Vec::new();
            let run = run(
                AgreementState::new(cadence),
                &mut recording,
                Some(&pace),
                |window| {
                    if late == Some(step) {
                        pace.advance();
                    }
                    step += 1;
                    whole_hypothesis(spoken, window)
                },
                |state| {
                    let gap = spoken
                        .iter()
                        .take_while(|word| sample_at(word.3) as u64 <= state.buffer_start)
                        .count();
                    if state.buffer_start > 0 && gaps.last() != Some(&gap) {
                        gaps.push(gap);
                    }
                },
            );
            (gaps, run.transcript().to_owned())
        }

        #[test]
        fn a_delayed_decode_cuts_the_same_pauses_when_words_go_out_one_at_a_time() {
            // Each pause runs longer than a cut needs, and the last one
            // runs to the end of the recording. A late decode may move a
            // cut within its pause.
            const SPOKEN: [Spoken; 6] = [
                ("a", 8_000, 0.2, 0.5),
                ("b", 7_000, 1.2, 1.5),
                ("c", 6_000, 2.25, 2.55),
                ("d", 5_000, 3.3, 3.6),
                ("e", 4_000, 4.35, 4.65),
                ("f", 3_000, 5.4, 5.7),
            ];

            let steady = pauses_cut(&SPOKEN, None);
            assert_eq!(steady.0, [1, 2, 3, 4, 5, 6]);
            for late in 0..18 {
                assert_eq!(
                    pauses_cut(&SPOKEN, Some(late)),
                    steady,
                    "decode {late} late"
                );
            }
        }

        #[test]
        fn a_delayed_decode_that_delivers_two_words_together_cuts_no_pause_between_them() {
            // The first decode runs late, so one decode confirms "a" and
            // "b" together. The pause between them then lies before the
            // last delivered word, where a cut would replay "b".
            const SPOKEN: [Spoken; 6] = [
                ("a", 8_000, 0.3, 0.6),
                ("b", 7_000, 1.1, 1.3),
                ("c", 6_000, 2.25, 2.55),
                ("d", 5_000, 3.3, 3.6),
                ("e", 4_000, 4.35, 4.65),
                ("f", 3_000, 5.4, 5.7),
            ];

            let steady = pauses_cut(&SPOKEN, None);
            let late = pauses_cut(&SPOKEN, Some(0));
            assert_eq!(steady.0, [1, 2, 3, 4, 5, 6]);
            assert_eq!(late.0, [2, 3, 4, 5, 6]);
            assert_eq!(late.1, steady.1);
        }

        #[test]
        fn an_untimed_lookback_decode_makes_the_buffers_decode_alone() {
            // The decode that opens on the lookback comes back without
            // timings, so its words can't be split at the cut. Nothing is
            // lost: the buffer decodes alone from then on.
            const SPOKEN: [Spoken; 2] = [("one", 8_000, 0.0, 0.4), ("two", 7_000, 1.5, 1.9)];
            let run = replay(cadence(20_000), &SPOKEN, 3.0, |window| {
                let (words, timings) = whole_words(&SPOKEN, window);
                match words.as_slice() {
                    ["one", "two"] => Hypothesis::new("one two"),
                    ["one"] => Hypothesis::with_words("one.", timings),
                    ["two"] => Hypothesis::with_words("Two.", timings),
                    _ => Hypothesis::new(""),
                }
            });

            assert_eq!(run.segments, ["one.", "Two."]);
            assert_eq!(run.ended_before, [None, None]);
        }

        /// Loud speech, a pause, then `after`, with a decode that hears a
        /// word only in the loud speech.
        fn transcript_after_a_pause(pause: i16, after: Vec<Vec<i16>>, max_ms: usize) -> String {
            let mut batches = vec![loud_batch(); 3];
            batches.extend(vec![vec![pause; STRIDE]; 8]);
            batches.extend(after);
            let (mut recording, pace) = paced(batches);
            let run = run(
                AgreementState::new(cadence(max_ms)),
                &mut recording,
                Some(&pace),
                |window| {
                    let loud = window.iter().any(|sample| sample.abs() > 0.01);
                    Hypothesis::new(if loud { "one" } else { "thank you" })
                },
                |_| {},
            );
            run.transcript().to_owned()
        }

        /// What a 20 s cadence types of `batches`, with a decode that names
        /// a window's word by its loudest sample.
        fn transcript_by_peak(
            batches: Vec<Vec<i16>>,
            word: impl Fn(f32) -> &'static str,
        ) -> String {
            let (mut recording, pace) = paced(batches);
            let run = run(
                AgreementState::new(cadence(20_000)),
                &mut recording,
                Some(&pace),
                |window| {
                    let peak = window
                        .iter()
                        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
                    Hypothesis::new(word(peak))
                },
                |_| {},
            );
            run.transcript().to_owned()
        }

        #[test]
        fn a_pause_that_outlives_the_cap_is_still_a_pause() {
            // A pause cut, then nothing but silence past the cap: the cap
            // cut must not make the next buffer hear the hallucination.
            let after = vec![faint_batch(); 32];
            assert_eq!(transcript_after_a_pause(225, after, 2_000), "one");
        }

        #[test]
        fn soft_speech_after_louder_speech_and_a_pause_is_heard() {
            // Speech at -25 dBFS, a faint pause, then speech 16 dB softer:
            // under TRIM_QUIET_FRACTION of the first, over SILENCE_FLOOR and
            // over the pause. The decode names what it hears by level.
            let mut batches = vec![vec![1_900_i16; STRIDE]; 3];
            batches.extend(vec![faint_batch(); 8]);
            batches.extend(vec![vec![300_i16; STRIDE]; 3]);
            batches.extend(vec![faint_batch(); 8]);
            let transcript = transcript_by_peak(batches, |peak| {
                if peak > 0.03 {
                    "one"
                } else if peak > 0.008 {
                    "two"
                } else {
                    ""
                }
            });

            assert_eq!(transcript, "one two");
        }

        #[test]
        fn quiet_speech_after_a_quieter_pause_is_heard() {
            // A distant voice: both spans of speech stay under
            // SILENCE_FLOOR, and the second rises well over the pause
            // between them.
            let mut batches = vec![vec![220_i16; STRIDE]; 3];
            batches.extend(vec![vec![40_i16; STRIDE]; 8]);
            batches.extend(vec![vec![150_i16; STRIDE]; 3]);
            batches.extend(vec![vec![40_i16; STRIDE]; 8]);
            let transcript = transcript_by_peak(batches, |peak| {
                if peak > 0.0055 {
                    "one"
                } else if peak > 0.003 {
                    "two"
                } else {
                    ""
                }
            });

            assert_eq!(transcript, "one two");
        }

        #[test]
        fn noise_a_little_over_the_pause_stays_the_pause() {
            // Room noise that drifts under 6 dB over the pause it follows
            // still holds no words, so its decode is ignored.
            let after = vec![vec![70_i16; STRIDE]; 8];
            assert_eq!(transcript_after_a_pause(40, after, 20_000), "one");
        }

        #[test]
        fn faint_sound_after_a_near_silent_pause_stays_the_pause() {
            // A pause one bit over silence: sound 20 dB over it is still
            // under any voice.
            let after = vec![vec![10_i16; STRIDE]; 8];
            assert_eq!(transcript_after_a_pause(1, after, 20_000), "one");
        }

        #[test]
        fn a_click_does_not_break_the_pause() {
            let mut click = vec![40_i16; STRIDE];
            click[..TRIM_FRAME_SAMPLES].fill(150);
            let mut after = vec![click];
            after.extend(vec![vec![40_i16; STRIDE]; 6]);
            assert_eq!(transcript_after_a_pause(40, after, 20_000), "one");
        }

        #[test]
        fn noise_creeping_up_over_many_caps_stays_the_pause() {
            // Each second 1.5 to 2 dB louder, until the noise sits 6.5 dB
            // over the pause it opened in.
            let after = [45_i16, 55, 65, 85]
                .into_iter()
                .flat_map(|level| vec![vec![level; STRIDE]; 10])
                .collect();
            assert_eq!(transcript_after_a_pause(40, after, 2_000), "one");
        }

        #[test]
        fn the_last_words_fade_does_not_raise_the_pause() {
            // The fade is quiet beside the loud word, so it opens the pause,
            // yet it sits over the floor.
            let mut batches = vec![loud_batch(); 3];
            batches.push(vec![1_000_i16; STRIDE]);
            batches.extend(vec![vec![40_i16; STRIDE]; 8]);
            batches.extend(vec![vec![150_i16; STRIDE]; 3]);
            batches.extend(vec![vec![40_i16; STRIDE]; 8]);
            let transcript = transcript_by_peak(batches, |peak| {
                if peak > 0.1 {
                    "one"
                } else if peak > 0.003 && peak < 0.006 {
                    "two"
                } else {
                    ""
                }
            });

            assert_eq!(transcript, "one two");
        }

        #[test]
        fn skipped_speech_is_a_run_of_speech_frames_no_word_covers() {
            // Ten words of ten frames, eight frames apart: the margins leave
            // two uncovered frames in every gap, never a run long enough to
            // count as skipped speech.
            let scattered: Vec<Word> = (0..10).map(|word| timed_word(word * 18, 10)).collect();
            assert_eq!(
                Loudness::of(&loud_frames(180)).unheard_speech(0, &scattered, 0),
                None
            );

            // The same words with a thirty-frame gap after the third one.
            let skipped: Vec<Word> = (0..10)
                .map(|word| timed_word(word * 18 + if word >= 3 { 30 } else { 0 }, 10))
                .collect();
            let gap_opens = (3 * 18 - 8 + HEARD_MARGIN_FRAMES) * TRIM_FRAME_SAMPLES;
            assert_eq!(
                Loudness::of(&loud_frames(210)).unheard_speech(0, &skipped, 0),
                Some(gap_opens as u64)
            );
        }

        #[test]
        fn steady_room_noise_is_not_skipped_speech() {
            let mut buffer = frames_at(0.02, 20);
            buffer.extend(loud_frames(2));
            buffer.extend(frames_at(0.02, 30));
            let loudness = Loudness::of(&buffer);
            assert!(
                loudness.squares[0] > SILENCE_FLOOR,
                "the room noise sits over the silence floor"
            );

            assert_eq!(loudness.unheard_speech(0, &[timed_word(20, 2)], 0), None);
        }

        #[test]
        fn soft_speech_the_model_hears_late_survives_the_pause() {
            // Each soft stride opens on a syllable that breaks the quiet run,
            // as real speech does, so only the silence after reads as a
            // pause. The decode hears the soft speech only once the window
            // grows past where that pause would cut.
            let mut soft_stride = vec![400_i16; STRIDE];
            soft_stride[..TRIM_FRAME_SAMPLES].fill(4_000);
            let mut batches = vec![quiet_batch(); 5];
            batches.push(loud_batch());
            batches.extend(vec![soft_stride; 8]);
            batches.extend(vec![quiet_batch(); 20]);
            let (mut recording, pace) = paced(batches);
            let loud = f32::from(loud_batch()[0]) / crate::audio::PCM_SCALE;
            // The silence after the soft speech is long enough to cut by
            // stride 19.
            let hears_soft_speech_from = 22 * STRIDE;
            let one = TimedWord {
                start: 0.5,
                end: 0.6,
            };
            let two = TimedWord {
                start: 0.6,
                end: 1.4,
            };
            let run = run(
                AgreementState::new(cadence(20_000)),
                &mut recording,
                Some(&pace),
                |window| {
                    if !window.contains(&loud) {
                        Hypothesis::new("")
                    } else if window.len() < hears_soft_speech_from {
                        Hypothesis::with_words("one", vec![one])
                    } else {
                        Hypothesis::with_words("one two", vec![one, two])
                    }
                },
                |_| {},
            );

            assert_eq!(run.transcript(), "one two");
        }

        #[test]
        fn a_word_heard_after_soft_background_goes_out_at_once() {
            // Background 16 dB under "a" sits between the words. "b" must
            // not wait for the cap.
            const SPOKEN: [Spoken; 2] = [("a", 8_000, 0.0, 0.4), ("b", 7_000, 2.0, 2.4)];
            let mut audio = spoken_audio(&SPOKEN, 8.0);
            audio[sample_at(0.6)..sample_at(1.6)].fill(1_200);
            let (mut recording, pace) = paced(strides(&audio));
            let mut b_delivered_at = None;
            run(
                AgreementState::new(cadence(20_000)),
                &mut recording,
                Some(&pace),
                |window| whole_hypothesis(&SPOKEN, window),
                |state| {
                    if state.transcript.len() == 2 && b_delivered_at.is_none() {
                        b_delivered_at = Some(state.decoded_end);
                    }
                },
            );

            let b_delivered_at = b_delivered_at.expect("\"b\" is delivered");
            assert!(
                b_delivered_at <= sample_at(3.0) as u64,
                "\"b\" waited until {b_delivered_at}"
            );
        }

        #[test]
        fn a_cap_cut_keeps_a_missed_word_before_skipped_speech() {
            // The first buffer hears only "a": "b" is too short to count as
            // skipped speech, "c" long enough. A cut at "c" would throw "b"
            // away.
            const SPOKEN: [Spoken; 3] = [
                ("a", 8_000, 0.0, 0.4),
                ("b", 6_000, 0.5, 0.8),
                ("c", 7_000, 0.9, 1.6),
            ];
            let audio = spoken_audio(&SPOKEN, 3.0);
            let (mut recording, pace) = paced(strides(&audio));
            // Only the first buffer misses the words after "a". The cap
            // bounds its windows; the windows after the cut open on the
            // lookback and are longer.
            let cap = sample_at(2.0);
            let run = run(
                AgreementState::new(cadence(2_000)),
                &mut recording,
                Some(&pace),
                |window| {
                    let (mut words, mut timings) = whole_words(&SPOKEN, window);
                    if window.len() <= cap && words.first() == Some(&"a") {
                        words.truncate(1);
                        timings.truncate(1);
                    }
                    Hypothesis::with_words(words.join(" "), timings)
                },
                |_| {},
            );

            assert_eq!(run.transcript(), "a b c");
        }

        #[test]
        fn a_cap_cut_keeps_words_heard_after_skipped_speech_at_its_opening() {
            // A later buffer opens just before the chatter, too near to cut
            // before it. "b", heard after the chatter, must still go out at
            // the cap.
            const SPOKEN: [Spoken; 2] = [("a", 8_000, 0.0, 0.4), ("b", 7_000, 2.0, 2.4)];
            let mut audio = spoken_audio(&SPOKEN, 8.0);
            audio[sample_at(0.6)..sample_at(1.6)].fill(3_000);
            let (mut recording, pace) = paced(strides(&audio));
            let run = run(
                AgreementState::new(cadence(4_000)),
                &mut recording,
                Some(&pace),
                |window| whole_hypothesis(&SPOKEN, window),
                |_| {},
            );

            assert_eq!(run.transcript(), "a b");
        }

        #[test]
        fn a_pause_opens_after_the_last_delivered_words_sound() {
            // A word timed from frame 0 to frame 3, whose sound runs on to
            // frame `loud`, then silence.
            let after = |loud: usize, quiet: usize| {
                let mut buffer = loud_frames(loud);
                buffer.extend(frames_at(0.0, quiet));
                Loudness::of(&buffer)
            };
            let word = 0..3;

            let cut = after(6, 16)
                .pause_after(word.clone(), 22)
                .expect("the pause opens inside the word's edge");
            assert_eq!(cut.at, (6 + 16 / 2) * TRIM_FRAME_SAMPLES);
            assert!(
                after(7, 16).pause_after(word.clone(), 23).is_none(),
                "no word accounts for sound past the edge"
            );
            assert!(after(3, 13).pause_after(word.clone(), 16).is_some());
            assert!(
                after(3, 13).pause_after(word, 15).is_none(),
                "the next word arrives before the pause runs long enough"
            );

            // The word's timing ran seven frames into the silence after it.
            let late = 0..10;
            let cut = after(3, 16)
                .pause_after(late, 19)
                .expect("a late timed end still finds the pause");
            assert_eq!(cut.at, (3 + 16 / 2) * TRIM_FRAME_SAMPLES);
        }

        #[test]
        fn a_pause_cut_needs_a_real_pause_at_the_tail() {
            let speech = [0.5_f32; TRIM_FRAME_SAMPLES];
            let silence = [0.0_f32; TRIM_FRAME_SAMPLES];

            let mut buffer = Vec::new();
            buffer.extend_from_slice(&speech);
            buffer.extend_from_slice(&speech);
            buffer.extend_from_slice(&speech);
            buffer.extend_from_slice(&silence.repeat(16)[..]);

            let cut = Loudness::of(&buffer)
                .trailing_pause()
                .expect("five hundred ms of silence is a pause");
            assert_eq!(cut.at, (3 + 16 / 2) * TRIM_FRAME_SAMPLES);

            let mut short = Vec::new();
            short.extend_from_slice(&speech);
            short.extend_from_slice(&silence.repeat(12)[..]);
            assert!(
                Loudness::of(&short).trailing_pause().is_none(),
                "300 ms is not a pause"
            );

            // A pause with speech after it is a dip inside a sentence, and
            // cutting there would re-deliver words the trim forgot.
            let mut mid = Vec::new();
            mid.extend_from_slice(&speech);
            mid.extend_from_slice(&silence.repeat(16)[..]);
            mid.extend_from_slice(&speech);
            assert!(
                Loudness::of(&mid).trailing_pause().is_none(),
                "a pause that does not reach the tail is not a stop"
            );

            assert!(
                Loudness::of(&speech).trailing_pause().is_none(),
                "constant speech has no pause"
            );
        }
    }

    /// A decoder that reads its answer out of the audio itself, driven over
    /// seeded random speech. The scripted tests above pin single behaviors;
    /// these hunt what no script anticipates — duplication, reordering,
    /// unbounded loss — across many trim and corruption patterns.
    mod simulated {
        use super::*;
        use crate::audio::PCM_SCALE;

        /// Xorshift64. Just enough randomness to vary speech; a failing
        /// assert names its seed, so any run replays exactly.
        struct Rng(u64);

        impl Rng {
            fn new(seed: u64) -> Self {
                // Nonzero (xorshift's fixed point) and injective, so every
                // seed the tests sweep is distinct speech.
                Self(seed + 1)
            }

            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }

            fn below(&mut self, bound: u64) -> u64 {
                self.next() % bound
            }

            fn chance(&mut self, percent: u64) -> bool {
                self.below(100) < percent
            }
        }

        /// Word `index` is one stride at amplitude `BASE + STEP * index`, so
        /// the audio alone names it. The quietest word carries
        /// (8000/8640)² ≈ 0.86 of the loudest frame's power, far above
        /// TRIM_QUIET_FRACTION, so no word reads as a pause.
        const BASE_AMPLITUDE: i16 = 8_000;
        const AMPLITUDE_STEP: i16 = 8;

        fn word_name(index: usize) -> String {
            format!("w{index}")
        }

        /// What one seed says: 40–79 words, each followed by a ~10% chance
        /// of 500–800 ms of silence — long enough for the 13-frame quiet
        /// rule, so quiet trims and cap cuts both occur — and a ~10% chance
        /// of being said again after a beat, so a repeated word is an
        /// ordinary word. With `phrases`, the last `CARRIED_WORDS` words are
        /// also said again with a ~5% chance, so a repeated phrase is too.
        struct Speech {
            batches: Vec<Vec<i16>>,
            /// The name of every word said, in order.
            names: Vec<usize>,
        }

        impl Speech {
            fn random(rng: &mut Rng, phrases: bool) -> Self {
                let distinct = 40 + rng.below(40) as usize;
                let mut batches = Vec::new();
                let mut names = Vec::new();
                for name in 0..distinct {
                    batches.push(word_batch(name));
                    names.push(name);
                    if rng.chance(10) {
                        batches.push(quiet_batch());
                        batches.push(word_batch(name));
                        names.push(name);
                    }
                    if phrases && names.len() >= CARRIED_WORDS && rng.chance(5) {
                        let phrase = names[names.len() - CARRIED_WORDS..].to_vec();
                        for (index, &said) in phrase.iter().enumerate() {
                            // Without the pause, a word said twice reads as
                            // one long word.
                            if index > 0 && phrase[index - 1] == said {
                                batches.push(quiet_batch());
                            }
                            batches.push(word_batch(said));
                        }
                        names.extend(phrase);
                    }
                    if rng.chance(10) {
                        for _ in 0..5 + rng.below(4) {
                            batches.push(quiet_batch());
                        }
                    }
                }
                // The speaker stops, so the last word's end is heard too.
                batches.push(quiet_batch());
                Self { batches, names }
            }
        }

        /// Two strides, so a run of words is said at five a second: as fast
        /// as speech gets, but no faster.
        fn word_batch(name: usize) -> Vec<i16> {
            vec![BASE_AMPLITUDE + AMPLITUDE_STEP * name as i16; 2 * STRIDE]
        }

        /// An honest decode of one window: every run of one constant nonzero
        /// amplitude at least half a stride long, ending before the window
        /// does, is a word, and the amplitude names it. i16 through PCM_SCALE
        /// is exact in f32, so equality scanning is safe.
        struct Heard {
            words: Vec<String>,
            timings: Vec<TimedWord>,
        }

        impl Heard {
            fn from_window(window: &[f32]) -> Self {
                let mut words = Vec::new();
                let mut timings = Vec::new();
                let mut start = 0;
                while start < window.len() {
                    let value = window[start];
                    let mut end = start + 1;
                    while end < window.len() && window[end] == value {
                        end += 1;
                    }
                    // A word still running at the window's edge is held
                    // back, as a real model holds a word until it hears
                    // the end of it.
                    if value != 0.0 && end - start >= STRIDE / 2 && end < window.len() {
                        let amplitude = (value * PCM_SCALE) as i16;
                        let index = (amplitude - BASE_AMPLITUDE) / AMPLITUDE_STEP;
                        words.push(word_name(index as usize));
                        let rate = CANONICAL_SAMPLE_RATE as f32;
                        timings.push(TimedWord {
                            start: start as f32 / rate,
                            end: end as f32 / rate,
                        });
                    }
                    start = end;
                }
                Self { words, timings }
            }

            /// The decode as a backend would answer it, with its timings or
            /// without: parakeet times its words, moonshine does not.
            fn into_hypothesis(self, timed: bool) -> Hypothesis {
                if timed {
                    Hypothesis::with_words(self.words.join(" "), self.timings)
                } else {
                    Hypothesis::new(self.words.join(" "))
                }
            }
        }

        /// One way a decode goes wrong. At most one per decode, so the loss
        /// accounting stays trivial.
        #[derive(Clone, Copy, PartialEq)]
        enum Corruption {
            /// The model dropped the window.
            Empty,
            /// The last one to three words are missing.
            Short,
            /// The last one or two words are junk.
            Unstable,
            /// One word in the first half is junk: a revision inside what
            /// may already have been delivered.
            Revision,
            /// A junk word appears in the first half, shifting every word
            /// after it: the model heard one word as two.
            Inserted,
            /// One word in the first half is missing, shifting every word
            /// after it: the model ran two words together.
            Dropped,
            /// Eight to twelve words in a row are missing, timings intact:
            /// the model skipped part of its window.
            Partial,
        }

        impl Corruption {
            fn adds_junk(self) -> bool {
                matches!(self, Self::Unstable | Self::Revision | Self::Inserted)
            }

            /// The corrupted decode. Junk text is fresh per decode, so junk
            /// can never agree across two decodes — it reaches the
            /// transcript only through a cap-cut tail or the finish. Junk
            /// keeps the timing of the word it replaces, as a model's
            /// misheard word would.
            fn apply(self, heard: Heard, rng: &mut Rng) -> Heard {
                let Heard {
                    mut words,
                    mut timings,
                } = heard;
                match self {
                    Self::Empty => {
                        words.clear();
                        timings.clear();
                    }
                    Self::Partial => {
                        let dropped = 8 + rng.below(5) as usize;
                        let from = rng.below((words.len() - dropped) as u64 + 1) as usize;
                        words.drain(from..from + dropped);
                        timings.drain(from..from + dropped);
                    }
                    Self::Short => {
                        let keep = words.len() - (1 + rng.below(3) as usize);
                        words.truncate(keep);
                        timings.truncate(keep);
                    }
                    Self::Unstable => {
                        let junked = (1 + rng.below(2) as usize).min(words.len());
                        let from = words.len() - junked;
                        for word in &mut words[from..] {
                            *word = junk(rng);
                        }
                    }
                    Self::Revision => {
                        let target = rng.below((words.len() / 2) as u64) as usize;
                        words[target] = junk(rng);
                    }
                    // The junk takes the timing of the word it sits before.
                    Self::Inserted => {
                        let target = rng.below((words.len() / 2) as u64) as usize;
                        words.insert(target, junk(rng));
                        let timing = timings[target];
                        timings.insert(target, timing);
                    }
                    Self::Dropped => {
                        let target = rng.below((words.len() / 2) as u64) as usize;
                        words.remove(target);
                        timings.remove(target);
                    }
                }
                Heard { words, timings }
            }
        }

        fn junk(rng: &mut Rng) -> String {
            format!("~{}", rng.next())
        }

        /// Chance, in percent, of each corruption per decode, and whether
        /// the decoder times its words.
        struct Noise {
            timed: bool,
            empty: u64,
            short: u64,
            unstable: u64,
            revision: u64,
            inserted: u64,
            dropped: u64,
            partial: u64,
        }

        const NOISELESS: Noise = Noise {
            timed: true,
            empty: 0,
            short: 0,
            unstable: 0,
            revision: 0,
            inserted: 0,
            dropped: 0,
            partial: 0,
        };

        const NOISY: Noise = Noise {
            timed: true,
            empty: 5,
            short: 10,
            unstable: 15,
            revision: 5,
            inserted: 5,
            dropped: 5,
            partial: 10,
        };

        /// An untimed decoder cannot show where it skipped speech, so a
        /// partial decode is not a fault it can be held to.
        const NOISY_UNTIMED: Noise = Noise {
            timed: false,
            partial: 0,
            ..NOISY
        };

        impl Noise {
            /// Rolls for one decode. A corruption needs enough words to act on.
            fn roll(&self, heard: &Heard, rng: &mut Rng) -> Option<Corruption> {
                let words = heard.words.len();
                if rng.chance(self.empty) {
                    Some(Corruption::Empty)
                } else if words > 2 && rng.chance(self.short) {
                    Some(Corruption::Short)
                } else if words > 0 && rng.chance(self.unstable) {
                    Some(Corruption::Unstable)
                } else if words >= 4 && rng.chance(self.revision) {
                    Some(Corruption::Revision)
                } else if words >= 4 && rng.chance(self.inserted) {
                    Some(Corruption::Inserted)
                } else if words >= 4 && rng.chance(self.dropped) {
                    Some(Corruption::Dropped)
                } else if words >= 14 && rng.chance(self.partial) {
                    Some(Corruption::Partial)
                } else {
                    None
                }
            }
        }

        /// Which truth words a transcript holds, in order, and how many junk
        /// words came with them.
        struct Delivered {
            names: Vec<usize>,
            junk: usize,
        }

        impl Delivered {
            fn parse(transcript: &str) -> Self {
                let mut names = Vec::new();
                let mut junk = 0;
                for word in transcript.split_whitespace() {
                    match word.strip_prefix('w').and_then(|name| name.parse().ok()) {
                        Some(name) => names.push(name),
                        None => junk += 1,
                    }
                }
                Self { names, junk }
            }

            /// Whether the words came out in the order they were said, each
            /// at most once: the delivered names read along the truth,
            /// skipping only what was lost.
            fn in_order_of(&self, truth: &[usize]) -> bool {
                let mut truth = truth.iter();
                self.names
                    .iter()
                    .all(|name| truth.by_ref().any(|said| said == name))
            }
        }

        #[derive(Clone, Copy, Debug)]
        enum Arrival {
            /// Each decode releases the next batch, as live capture does.
            Live,
            /// All audio is there from the start, as with a file.
            Finished,
        }

        struct Sim {
            run: Run,
            injected: Vec<Corruption>,
            truth: Vec<usize>,
        }

        /// One seeded run to completion. Windows respect the cap, and
        /// retention holds after every step. Untimed runs get no repeated
        /// phrases: without timings, two copies of a phrase look the same.
        fn simulate(seed: u64, noise: &Noise, arrival: Arrival) -> Sim {
            let mut rng = Rng::new(seed);
            let speech = Speech::random(&mut rng, noise.timed);
            let truth = speech.names.clone();
            let max_buffer_ms = [1_500, 3_000, 20_000][rng.below(3) as usize];
            let max_window = max_buffer_ms * CANONICAL_SAMPLE_RATE as usize / 1000
                + 2 * STRIDE
                + LOOKBACK_SAMPLES as usize;
            let (mut recording, pace) = match arrival {
                Arrival::Live => {
                    let (recording, pace) = paced(speech.batches);
                    (recording, Some(pace))
                }
                Arrival::Finished => (recorded(&speech.batches.concat()), None),
            };
            let mut injected = Vec::new();
            let mut last_retained = 0;

            let run = run(
                AgreementState::new(cadence(max_buffer_ms)),
                &mut recording,
                pace.as_ref(),
                |window| {
                    assert!(
                        window.len() <= max_window,
                        "seed {seed}: a {} sample window outgrew the {max_window} cap",
                        window.len()
                    );
                    let heard = Heard::from_window(window);
                    let heard = match noise.roll(&heard, &mut rng) {
                        Some(corruption) => {
                            injected.push(corruption);
                            corruption.apply(heard, &mut rng)
                        }
                        None => heard,
                    };
                    heard.into_hypothesis(noise.timed)
                },
                |state| retention_holds(seed, state, &mut last_retained),
            );

            Sim {
                run,
                injected,
                truth,
            }
        }

        /// `retained_from` never moves backwards, and never past the audio of
        /// a segment still waiting to be handed over.
        fn retention_holds(seed: u64, state: &AgreementState, last_retained: &mut u64) {
            let retained = state.retained_from();
            assert!(
                retained >= *last_retained,
                "seed {seed}: retained_from moved backwards ({last_retained} -> {retained})"
            );
            *last_retained = retained;
            if let Some(span) = state.pending_segment.as_ref().and_then(|tail| tail.span()) {
                assert!(
                    retained <= span.start,
                    "seed {seed}: a pending segment's audio at {} was released at {retained}",
                    span.start
                );
            }
        }

        #[test]
        fn a_noiseless_simulated_decoder_delivers_the_truth_exactly() {
            for arrival in [Arrival::Live, Arrival::Finished] {
                for seed in 0..25 {
                    let sim = simulate(seed, &NOISELESS, arrival);
                    let truth: Vec<String> = sim.truth.iter().copied().map(word_name).collect();
                    assert_eq!(
                        sim.run.transcript(),
                        truth.join(" "),
                        "seed {seed}, {arrival:?}: the transcript strayed from the truth"
                    );
                    assert_eq!(
                        sim.run.segments.join(" "),
                        sim.run.transcript(),
                        "seed {seed}, {arrival:?}: delivery and the final transcript disagree"
                    );
                }
            }
        }

        // Live only. A finished recording decodes each window once, so one
        // empty decode loses that window.
        #[test]
        fn noisy_decodes_keep_every_agreement_invariant_across_seeds() {
            for seed in 0..200 {
                invariants_hold(seed, &simulate(seed, &NOISY, Arrival::Live));
            }
        }

        #[test]
        fn an_untimed_decoder_keeps_the_invariants_too() {
            for seed in 0..100 {
                invariants_hold(seed, &simulate(seed, &NOISY_UNTIMED, Arrival::Live));
            }
        }

        fn invariants_hold(seed: u64, sim: &Sim) {
            let delivered = Delivered::parse(sim.run.transcript());
            let junk_sources = sim
                .injected
                .iter()
                .filter(|corruption| corruption.adds_junk())
                .count();
            let truth_words = sim.truth.len();

            assert_eq!(
                sim.run.segments.join(" "),
                sim.run.transcript(),
                "seed {seed}: delivery and the final transcript disagree"
            );
            assert!(
                delivered.in_order_of(&sim.truth),
                "seed {seed}: a word was duplicated or reordered: {}",
                sim.run.transcript()
            );
            assert!(
                delivered.junk <= 2 * junk_sources,
                "seed {seed}: {} junk words from {junk_sources} junk corruptions",
                delivered.junk
            );
            // The bound is loose on purpose. It catches unbounded loss, not
            // the exact cost of each path.
            let missing = truth_words - delivered.names.len();
            assert!(
                missing <= 3 * sim.injected.len(),
                "seed {seed}: lost {missing} of {truth_words} words to {} corruptions",
                sim.injected.len()
            );
        }
    }
}
