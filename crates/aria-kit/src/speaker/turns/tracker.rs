use std::ops::Range;
use std::time::Duration;

use super::{Admission, Judgment, NamedTurn, SpeakerTracker, Timeline, Turn};
use crate::audio::canonical_samples;
use crate::primitives::error_chain;
use crate::segmentation::{Activity, ChunkShape, SegmentationError, Segmenter};
use crate::speaker::changes::{Continuity, Evidence};
use crate::speaker::{Diarizer, Embedder, OWN_VOICE_THRESHOLD, Profile, Role, SpeakerName};
use crate::voice_activity::{LEAD_IN_FRAMES, SpeechMap};

#[cfg(test)]
mod tests;

/// How much new audio a turn takes before its voice is checked again.
const CHECK_EVERY_SAMPLES: usize = canonical_samples(Duration::from_millis(500)) as usize;
/// A turn this long that has not passed as the owner's is another voice's.
/// The owner's own voice scores near the bar on a second of speech, so a
/// failing score is not trusted before this.
const TURN_JUDGED_SAMPLES: usize = canonical_samples(Duration::from_secs(3)) as usize;
/// The window at the end of a judged turn, checked for a speaker change
/// without a pause between. Audio this recent is not stable: a change found
/// later can still move it into a new turn.
const RECENT_SAMPLES: usize = canonical_samples(Duration::from_millis(1_500)) as usize;
/// A turn ends where its utterance does.
const TURN_GAP_SAMPLES: usize = canonical_samples(crate::voice_activity::UTTERANCE_GAP) as usize;
/// At or above this, the recent window of a turn holds at most its newest
/// [`CHECK_EVERY_SAMPLES`] of another voice, so the audio before that is
/// stable as the owner's. Measured on two live voices: a 1.5 s window
/// ending in 0.4 s of another voice scored 0.46–0.47, in 0.5 s 0.43–0.44.
const STABLE_THRESHOLD: f32 = 0.45;
/// Below this, the recent window of the owner's turn is someone else.
/// Measured on 1.5 s windows: the owner scored 0.39 or more, another voice
/// 0.32 or less.
const SPEAKER_CHANGE_THRESHOLD: f32 = 0.3;

/// Cuts a recording into turns and judges each one. `E` scores whose voice
/// a window is. `S`, once given, checks a speaker change inside a turn,
/// which owner-only admission needs.
pub struct VoiceprintTracker<E, S = NoSegmenter> {
    voices: Diarizer<E>,
    /// The speech map's frame size, taken from the map heard last.
    frame_samples: usize,
    /// What the speech map said about the frame being heard.
    scored: FrameSpeech,
    /// Skipped into audio the speech map may hold an utterance in: the
    /// next frame of speech opens a turn.
    rejoin: bool,
    frame: Vec<f32>,
    lead_in: Vec<f32>,
    timeline: Timeline,
    open: Option<OpenTurn>,
    admission: Admission,
    continuity: Option<Continuity<S>>,
}

/// What the speech map said about a frame, once it was scored.
#[derive(Debug, Clone, Default)]
struct FrameSpeech {
    /// Samples of speech before the frame's end.
    speech_before: u64,
    /// The silence the open utterance ends in, at the frame's end.
    trailing_silence: u64,
    /// The open utterance's pauses, newest first.
    pauses: Vec<Range<u64>>,
}

/// What the open turn needs beyond its [`Turn`].
struct OpenTurn {
    /// The turn's latest audio, ending at the last sample pushed. Pauses
    /// inside the turn stay in: the embedder drops silence itself.
    audio: Vec<f32>,
    unchecked: usize,
    /// Whether a check scored the turn against the owner's voiceprint.
    scored: bool,
    /// Whether a check heard the turn far from the owner's voice, or as an
    /// enrolled guest's. A doubted turn is not admitted for being short.
    doubted: bool,
    /// The span last heard as this turn's voice, which the segmenter
    /// compares a later speaker change against.
    anchor: Option<Range<u64>>,
    /// Where a turn proposed as the owner's starts, until the segmenter
    /// confirms the speaker change or the turn is judged without it.
    proposed: Option<u64>,
    /// The sum of the embeddings heard since the turn was last named, each
    /// weighted by the speech it added.
    embedding: Vec<f32>,
    /// How much speech the recording held when the turn was last named.
    named: u64,
}

impl OpenTurn {
    /// The judgment of a pending turn the owner's voice has not passed, or
    /// `None` while it stays pending.
    fn verdict(&self, length: usize, closing: bool, owners: usize) -> Option<Judgment> {
        let judged = length >= TURN_JUDGED_SAMPLES;
        let rejected = if self.proposed.is_some() {
            judged
        } else {
            self.doubted || (judged && self.scored) || owners == 0
        };
        if rejected && (closing || judged) {
            Some(Judgment::Other)
        } else if closing {
            Some(Judgment::Unverified)
        } else {
            None
        }
    }
}

/// The segmenter of a tracker that never checks a speaker change, so it can
/// name voices but not admit only the owner. Nothing can build one.
pub enum NoSegmenter {}

impl Segmenter for NoSegmenter {
    fn shape(&self) -> ChunkShape {
        match *self {}
    }

    fn segment(&mut self, _: &[f32]) -> Result<Vec<Activity>, SegmentationError> {
        match *self {}
    }
}

impl<E: Embedder, S: Segmenter> VoiceprintTracker<E, S> {
    pub fn new(voices: Diarizer<E>) -> Self {
        Self {
            voices,
            frame_samples: 0,
            scored: FrameSpeech::default(),
            rejoin: false,
            frame: Vec::new(),
            lead_in: Vec::new(),
            timeline: Timeline::default(),
            open: None,
            admission: Admission::Everyone,
            continuity: None,
        }
    }

    pub fn with_segmenter(mut self, segmenter: S) -> Result<Self, SegmentationError> {
        self.continuity = Some(Continuity::new(segmenter)?);
        Ok(self)
    }

    fn can_filter(&self) -> bool {
        self.voices.owners() > 0 && self.continuity.is_some()
    }

    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Forgets the last recording. Positions count from `at`, where the next
    /// push starts in the new recording's stream.
    fn restart(&mut self, at: u64) {
        self.scored = FrameSpeech::default();
        self.rejoin = false;
        self.frame.clear();
        self.lead_in.clear();
        self.timeline = Timeline {
            reached: at,
            ..Timeline::default()
        };
        self.open = None;
        if let Some(continuity) = &mut self.continuity {
            continuity.restart(at);
        }
    }

    /// Where the next pushed sample lands in the stream.
    fn position(&self) -> u64 {
        self.timeline.reached
    }

    fn skip_to(&mut self, at: u64) {
        if at <= self.position() + self.frame.len() as u64 {
            return;
        }
        if let Some(turn) = self.timeline.turns.last_mut()
            && turn.judgment == Judgment::Pending
        {
            turn.judgment = Judgment::Other;
        }
        self.timeline.turns.push(Turn {
            span: self.position()..at,
            judgment: Judgment::Other,
        });
        self.timeline.names.push(NamedTurn {
            span: self.position()..at,
            name: None,
        });
        self.scored = FrameSpeech::default();
        self.rejoin = true;
        self.frame.clear();
        self.lead_in.clear();
        self.open = None;
        self.timeline.open = false;
        self.timeline.reached = at;
        self.timeline.stable_to = at;
        if let Some(continuity) = &mut self.continuity {
            continuity.restart(at);
        }
    }

    /// Takes the recording's audio in order, in pieces of any size.
    fn push(&mut self, speech: &SpeechMap, mut samples: &[f32]) {
        self.frame_samples = speech.frame_samples();
        while !samples.is_empty() && !self.timeline.broken {
            let taken = samples.len().min(self.frame_samples - self.frame.len());
            self.frame.extend_from_slice(&samples[..taken]);
            samples = &samples[taken..];
            if self.frame.len() == self.frame_samples {
                let mut frame = std::mem::take(&mut self.frame);
                self.push_frame(speech, &frame);
                frame.clear();
                self.frame = frame;
            }
        }
    }

    fn push_frame(&mut self, speech: &SpeechMap, samples: &[f32]) {
        let start = self.position();
        let end = start + samples.len() as u64;
        if end > speech.reached() {
            // Past where the detector failed, nothing can be told apart.
            debug_assert!(speech.is_broken(), "a frame is heard before it is scored");
            self.timeline.broken = true;
            return;
        }
        self.scored = FrameSpeech {
            speech_before: speech.speech_before(end),
            trailing_silence: speech.trailing_silence(end),
            pauses: speech.pauses_before(end).collect(),
        };
        let rejoined = self.rejoin && speech.speech(&(start..end)) > 0;
        if speech.opens_at(start) || rejoined {
            self.rejoin = false;
            self.open_turn(start);
            self.hear(samples, 0);
        } else if speech.closes_at(end) {
            self.hear(samples, TURN_GAP_SAMPLES);
            self.close_turn(TURN_GAP_SAMPLES);
        } else {
            self.hear(samples, self.trailing_silence());
        }
    }

    /// Judges the open turn on what it had. Call once the recording ends.
    fn finish(&mut self) {
        self.timeline.finished = true;
        if self.timeline.broken {
            return;
        }
        let frame = std::mem::take(&mut self.frame);
        self.hear(&frame, self.trailing_silence());
        self.close_turn(self.trailing_silence());
    }

    /// How much silence the open turn ends in so far.
    fn trailing_silence(&self) -> usize {
        usize::try_from(self.scored.trailing_silence).unwrap_or(usize::MAX)
    }

    /// Checks land every [`CHECK_EVERY_SAMPLES`] of turn audio, however the
    /// audio was split into pushes.
    fn hear(&mut self, mut samples: &[f32], silence: usize) {
        while !samples.is_empty() && !self.timeline.broken {
            let room = self.open.as_ref().map_or(samples.len(), |open| {
                CHECK_EVERY_SAMPLES.saturating_sub(open.unchecked).max(1)
            });
            let (now, rest) = samples.split_at(room.min(samples.len()));
            samples = rest;
            if self.admission == Admission::OwnerOnly
                && let Some(continuity) = &mut self.continuity
                && let Err(error) = continuity.push(self.timeline.reached, now)
            {
                tracing::warn!(error = %error_chain(&error), "voice continuity could not retain audio");
                self.timeline.broken = true;
                return;
            }
            self.timeline.reached += now.len() as u64;
            let position = self.position();
            let Some(open) = self.open.as_mut() else {
                self.keep_lead_in(now);
                self.timeline.stable_to = position;
                continue;
            };
            open.audio.extend_from_slice(now);
            let excess = open.audio.len().saturating_sub(TURN_JUDGED_SAMPLES);
            open.audio.drain(..excess);
            open.unchecked += now.len();
            let due = open.unchecked >= CHECK_EVERY_SAMPLES;
            if let Some(turn) = self.timeline.turns.last_mut() {
                turn.span.end = position;
            }
            if let Some(turn) = self.timeline.names.last_mut() {
                turn.span.end = position;
            }
            if due {
                self.check(false, silence.saturating_sub(samples.len()));
            }
            self.advance_stable_to_recent();
        }
    }

    /// Keeps the newest few frames heard outside a turn, for the next turn
    /// to open with.
    fn keep_lead_in(&mut self, audio: &[f32]) {
        self.lead_in.extend_from_slice(audio);
        let excess = self
            .lead_in
            .len()
            .saturating_sub(LEAD_IN_FRAMES * self.frame_samples);
        self.lead_in.drain(..excess);
    }

    /// Audio older than the recent window is stable, except a pending turn
    /// proposed as the owner's, which stays open to a change until verified.
    fn advance_stable_to_recent(&mut self) {
        let stable_to = self
            .timeline
            .stable_to
            .max(self.position().saturating_sub(RECENT_SAMPLES as u64));
        let pending = self
            .timeline
            .turns
            .last()
            .is_some_and(|turn| turn.judgment == Judgment::Pending);
        let proposed = self.open.as_ref().and_then(|open| open.proposed);
        self.timeline.stable_to = match proposed {
            Some(from) if pending => stable_to.min(from),
            _ => stable_to,
        };
    }

    /// A window that scores this clearly as the owner's holds at most its
    /// newest check of another voice, so the audio before that is stable.
    fn advance_stable_to_check(&mut self, silence: usize, score: Option<f32>) {
        if score.is_some_and(|score| score >= STABLE_THRESHOLD) {
            let checked_to = self
                .position()
                .saturating_sub((silence + CHECK_EVERY_SAMPLES) as u64);
            self.timeline.stable_to = self.timeline.stable_to.max(checked_to);
        }
    }

    /// Opens a turn a few frames before the frame that called it speech, as
    /// the microphone does, so a soft onset is not cut off.
    fn open_turn(&mut self, at: u64) {
        // The lead-in holds only audio heard since the last turn, a restart,
        // or a skip, so the turn cannot reach back past any of them.
        let audio = std::mem::take(&mut self.lead_in);
        let start = at.saturating_sub(audio.len() as u64);
        self.timeline.turns.push(Turn {
            span: start..at,
            judgment: Judgment::Pending,
        });
        self.timeline.names.push(NamedTurn {
            span: start..at,
            name: None,
        });
        self.open = Some(OpenTurn {
            audio,
            unchecked: 0,
            scored: false,
            doubted: false,
            anchor: None,
            proposed: None,
            embedding: Vec::new(),
            named: self
                .scored
                .speech_before
                .saturating_sub(self.frame_samples as u64),
        });
        self.timeline.open = true;
    }

    /// `silence` is how much of the turn's end is the silence that closed
    /// it: a voice is judged on its speech, not on the quiet after it.
    fn close_turn(&mut self, silence: usize) {
        if self.open.as_ref().is_some_and(|open| open.unchecked > 0) {
            self.check(true, silence);
        }
        if let Some(open) = self.open.take()
            && let Some(turn) = self.timeline.turns.last_mut()
            && turn.judgment == Judgment::Pending
            && let Some(judgment) = open.verdict(turn_length(turn), true, self.voices.owners())
        {
            turn.judgment = judgment;
        }
        self.timeline.open = self.open.is_some();
    }

    /// Checks the open turn's voice. A pending turn passes or, once old
    /// enough, fails. A judged turn keeps its judgment, and is split where
    /// its recent window sounds like a different voice. The turn's last
    /// `silence` samples are left out of every window scored: silence
    /// dilutes a voice, and a pause is not a change of voice.
    fn check(&mut self, closing: bool, silence: usize) {
        let (Some(open), Some(turn)) = (self.open.as_mut(), self.timeline.turns.last()) else {
            return;
        };
        open.unchecked = 0;
        let length = turn_length(turn);
        if open.proposed.is_some() {
            self.check_proposed(closing, length, silence);
            return;
        }
        match turn.judgment {
            Judgment::Pending => self.judge_pending(closing, length, silence),
            judged if !closing => {
                self.watch_for_change(judged, silence, length >= RECENT_SAMPLES);
            }
            Judgment::Unverified | Judgment::Owner | Judgment::Other => {}
        }
    }

    fn judge_pending(&mut self, closing: bool, length: usize, silence: usize) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let spoken = spoken(&open.audio, silence);
        let input_end = self
            .position()
            .saturating_sub(silence.min(open.audio.len()) as u64);
        let input = input_end.saturating_sub(spoken.len() as u64)..input_end;
        let voice = score(&mut self.voices, &mut self.timeline, spoken);
        let score = voice.as_ref().and_then(|voice| voice.owner);
        let matched_guest = voice
            .as_ref()
            .is_some_and(|voice| voice.matched_role == Some(Role::Guest));
        let open = self.open.as_mut().expect("the checked turn is open");
        if voice.is_some() && !input.is_empty() {
            open.anchor = Some(input);
        }
        open.scored |= score.is_some();
        open.doubted |= matched_guest
            || score.is_some_and(|score| !score.is_finite() || score < SPEAKER_CHANGE_THRESHOLD);
        let verdict = open.verdict(length, closing, self.voices.owners());
        if let Some(voice) = voice {
            self.name(&voice.embedding);
        }
        let turn = self
            .timeline
            .turns
            .last_mut()
            .expect("the checked turn is open");
        if score.is_some_and(|score| score >= OWN_VOICE_THRESHOLD) {
            turn.judgment = Judgment::Owner;
        } else if let Some(judgment) = verdict {
            turn.judgment = judgment;
        }
        log_check(turn, score, length);
        // The bar was measured on windows this long; a longer one dilutes
        // another voice at its end.
        if length <= RECENT_SAMPLES {
            self.advance_stable_to_check(silence, score);
        }
    }

    /// Toward another voice, a split lands early, where the recent window
    /// starts, so none of that voice passes as the owner's. Toward the
    /// owner, it lands late, at the pause the owner began after or else at
    /// the newest check.
    fn watch_for_change(&mut self, judged: Judgment, silence: usize, can_split: bool) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let voiced = spoken(&open.audio, silence);
        let recent = tail(voiced, RECENT_SAMPLES);
        let voiced_len = voiced.len();
        let recent_len = recent.len();
        let Some(voice) = score(&mut self.voices, &mut self.timeline, recent) else {
            return;
        };
        let similarity = voice.owner;
        tracing::debug!(
            similarity,
            at_ms = crate::audio::canonical_duration(self.position()).as_millis(),
            judged = ?judged,
            "speaker change watch"
        );
        let name_changed = self
            .timeline
            .names
            .last()
            .is_some_and(|turn| turn.name != voice.name);
        if can_split && name_changed {
            let at = self
                .position()
                .saturating_sub((silence + recent_len) as u64);
            self.split_name(at, voice.name);
        }
        self.name(&voice.embedding);
        match judged {
            Judgment::Owner
                if can_split
                    && similarity.is_some_and(|score| score < SPEAKER_CHANGE_THRESHOLD) =>
            {
                // Not before audio an earlier check heard as the owner's.
                let unstable = self
                    .position()
                    .saturating_sub(silence as u64)
                    .saturating_sub(self.timeline.stable_to);
                let changed = recent_len.min(unstable as usize);
                self.split(voiced_len - changed, Judgment::Pending, similarity);
                return;
            }
            Judgment::Other
                if can_split && similarity.is_some_and(|score| score >= OWN_VOICE_THRESHOLD) =>
            {
                if let Some((from, similarity)) = self.owner_since_a_pause(silence) {
                    self.propose_owner(from, silence, Some(similarity));
                    return;
                }
                let Some(open) = self.open.as_ref() else {
                    return;
                };
                let voiced = spoken(&open.audio, silence);
                let newest = tail(voiced, CHECK_EVERY_SAMPLES);
                if let Some(newest_voice) = score(&mut self.voices, &mut self.timeline, newest)
                    && newest_voice
                        .owner
                        .is_some_and(|score| score >= OWN_VOICE_THRESHOLD)
                {
                    self.propose_owner(voiced.len() - newest.len(), silence, newest_voice.owner);
                    return;
                }
            }
            Judgment::Pending | Judgment::Unverified | Judgment::Owner | Judgment::Other => {}
        }
        if can_split && judged == Judgment::Owner {
            self.advance_stable_to_check(silence, similarity);
        }
    }

    fn propose_owner(&mut self, from: usize, silence: usize, similarity: Option<f32>) {
        if self.admission != Admission::OwnerOnly {
            self.split(from, Judgment::Owner, similarity);
            return;
        }
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let end = self
            .position()
            .saturating_sub(silence.min(open.audio.len()) as u64);
        let start = self.position() - open.audio.len() as u64 + from as u64;
        let candidate = start..end;
        if let Some((judgment, range, voice)) = self.verified_change(&candidate) {
            self.apply_change(judgment, range, voice);
        } else if !self.timeline.broken {
            let anchor = self.open.as_ref().and_then(|open| open.anchor.clone());
            self.split_at(candidate.start, Judgment::Pending, None);
            let from = self
                .timeline
                .turns
                .last()
                .map_or(candidate.start, |turn| turn.span.start);
            if let Some(open) = self.open.as_mut() {
                // The split forgot the anchor; the change is still verified
                // against the voice heard before it.
                open.anchor = anchor;
                open.proposed = Some(from);
            }
            self.split_name(from, None);
        }
    }

    fn check_proposed(&mut self, closing: bool, length: usize, silence: usize) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let Some(from) = open.proposed else {
            return;
        };
        let end = self
            .position()
            .saturating_sub(silence.min(open.audio.len()) as u64);
        let candidate = from.max(end.saturating_sub(RECENT_SAMPLES as u64))..end;
        if !candidate.is_empty()
            && let Some((judgment, range, voice)) = self.verified_change(&candidate)
        {
            self.apply_change(judgment, range, voice);
            return;
        }
        if self.timeline.broken {
            return;
        }
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if let Some(judgment) = open.verdict(length, closing, self.voices.owners())
            && let Some(turn) = self.timeline.turns.last_mut()
        {
            turn.judgment = judgment;
        }
        if closing {
            open.proposed = None;
        }
    }

    fn verified_change(
        &mut self,
        candidate: &Range<u64>,
    ) -> Option<(Judgment, Range<u64>, Scored)> {
        let anchor = self.open.as_ref()?.anchor.clone();
        let Some(continuity) = &mut self.continuity else {
            tracing::warn!("own-voice filtering has no continuity verifier");
            self.timeline.broken = true;
            return None;
        };
        let evidence = match continuity.verify(anchor.as_ref(), candidate, TURN_JUDGED_SAMPLES) {
            Ok(evidence) => evidence,
            Err(error) => {
                tracing::warn!(error = %error_chain(&error), "voice continuity could not be judged");
                self.timeline.broken = true;
                return None;
            }
        };
        let (range, reassessed) = match evidence {
            Evidence::Reassess(range) => (range, true),
            Evidence::Replaced { anchor, candidate } => {
                self.score_range(&anchor)?;
                (candidate, false)
            }
            Evidence::Unresolved => return None,
        };
        let voice = self.score_range(&range)?;
        let similarity = voice.owner?;
        if !similarity.is_finite() {
            tracing::warn!("voice continuity returned an invalid owner score");
            self.timeline.broken = true;
            return None;
        }
        let judgment = if similarity >= OWN_VOICE_THRESHOLD {
            Judgment::Owner
        } else if reassessed
            || similarity < SPEAKER_CHANGE_THRESHOLD
            || voice.matched_role == Some(Role::Guest)
            || range.end - range.start >= TURN_JUDGED_SAMPLES as u64
        {
            Judgment::Other
        } else {
            return None;
        };
        Some((judgment, range, voice))
    }

    fn score_range(&mut self, range: &Range<u64>) -> Option<Scored> {
        let Some(continuity) = &self.continuity else {
            self.timeline.broken = true;
            return None;
        };
        let audio = match continuity.audio(range) {
            Ok(audio) => audio,
            Err(error) => {
                tracing::warn!(error = %error_chain(&error), "voice evidence was not retained");
                self.timeline.broken = true;
                return None;
            }
        };
        score(&mut self.voices, &mut self.timeline, audio)
    }

    fn apply_change(&mut self, judgment: Judgment, range: Range<u64>, voice: Scored) {
        if judgment == Judgment::Owner {
            let at = range.start.max(self.timeline.stable_to);
            let replace =
                self.timeline.turns.last().is_some_and(|turn| {
                    turn.judgment == Judgment::Pending && at <= turn.span.start
                });
            if let Some(turn) = self.timeline.turns.last_mut()
                && turn.judgment == Judgment::Pending
            {
                turn.judgment = if replace {
                    Judgment::Owner
                } else {
                    Judgment::Other
                };
            }
            if replace {
                if let Some(open) = self.open.as_mut() {
                    open.proposed = None;
                    open.anchor = None;
                    open.scored = true;
                    open.doubted = false;
                }
            } else {
                self.split_at(range.start, judgment, voice.owner);
            }
            let at = self
                .timeline
                .turns
                .last()
                .map_or(range.start, |turn| turn.span.start);
            self.split_name(at, voice.name);
        } else {
            if let Some(turn) = self.timeline.turns.last_mut() {
                turn.judgment = Judgment::Other;
            }
            if let Some(open) = self.open.as_mut() {
                open.proposed = None;
                open.anchor = Some(range);
            }
        }
    }

    fn split_at(&mut self, at: u64, judgment: Judgment, similarity: Option<f32>) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let retained_from = self.position() - open.audio.len() as u64;
        let from = at
            .saturating_sub(retained_from)
            .min(open.audio.len() as u64) as usize;
        self.split(from, judgment, similarity);
    }

    fn name(&mut self, embedding: &[f32]) {
        let (Some(open), Some(turn)) = (self.open.as_mut(), self.timeline.names.last_mut()) else {
            return;
        };
        let speech = self.scored.speech_before;
        let weight = speech.saturating_sub(open.named);
        open.named = speech;
        if weight == 0 {
            return;
        }
        if open.embedding.is_empty() {
            open.embedding.resize(embedding.len(), 0.0);
        }
        for (sum, value) in open.embedding.iter_mut().zip(embedding) {
            *sum += value * weight as f32;
        }
        if let Some(voice) = crate::speaker::normalized(open.embedding.clone()) {
            turn.name = self
                .voices
                .matched_profile(&voice)
                .map(|profile| profile.name.clone());
        }
    }

    fn split_name(&mut self, at: u64, name: Option<SpeakerName>) {
        let end = self.position();
        if let Some(turn) = self.timeline.names.last_mut() {
            let at = at.max(turn.span.start).min(end);
            turn.span.end = at;
            self.timeline.names.push(NamedTurn {
                span: at..end,
                name,
            });
        }
        if let Some(open) = self.open.as_mut() {
            open.embedding.clear();
        }
    }

    /// Where in the open turn's audio the owner's new turn begins, with the
    /// score of its audio, when it began at a pause: the owner
    /// usually starts after one. The start moves back over each pause in
    /// the window a check heard the owner in while the audio from there
    /// still sounds clearly like the owner. A window too short to embed is
    /// skipped, not judged.
    fn owner_since_a_pause(&mut self, silence: usize) -> Option<(usize, f32)> {
        let position = self.position();
        let open = self.open.as_ref()?;
        let voiced = spoken(&open.audio, silence);
        let earliest = voiced.len().saturating_sub(RECENT_SAMPLES);
        let mut found = None;
        for pause in self.scored.pauses.clone() {
            let behind = usize::try_from(position.saturating_sub(pause.end)).unwrap_or(usize::MAX);
            // Behind the audio the open turn still holds, or the window
            // the check heard the owner in: no evidence that far back.
            let Some(index) = open
                .audio
                .len()
                .checked_sub(behind)
                .filter(|index| *index >= earliest)
            else {
                break;
            };
            if voiced.len().saturating_sub(index) < CHECK_EVERY_SAMPLES {
                continue;
            }
            let Some(voice) = score(&mut self.voices, &mut self.timeline, &voiced[index..]) else {
                break;
            };
            let Some(similarity) = voice
                .owner
                .filter(|similarity| *similarity >= STABLE_THRESHOLD)
            else {
                break;
            };
            found = Some((index, similarity));
        }
        found
    }

    /// Ends the open turn at `from`, an index into its audio, and opens a
    /// new turn there.
    fn split(&mut self, from: usize, judgment: Judgment, score: Option<f32>) {
        let end = self.position();
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let retained_from = end - open.audio.len() as u64;
        let stable = self.timeline.stable_to.saturating_sub(retained_from) as usize;
        let from = from.max(stable).min(open.audio.len());
        if from == open.audio.len() {
            return;
        }
        open.audio.drain(..from);
        open.unchecked = 0;
        open.anchor = None;
        open.proposed = None;
        open.scored = score.is_some();
        open.doubted =
            score.is_some_and(|score| !score.is_finite() || score < SPEAKER_CHANGE_THRESHOLD);
        let at = end - open.audio.len() as u64;
        if let Some(turn) = self.timeline.turns.last_mut() {
            turn.span.end = at;
        }
        let turn = Turn {
            span: at..end,
            judgment,
        };
        log_check(&turn, score, open.audio.len());
        self.timeline.turns.push(turn);
    }
}

fn turn_length(turn: &Turn) -> usize {
    (turn.span.end - turn.span.start) as usize
}

/// `audio` without the `silence` it ends in.
fn spoken(audio: &[f32], silence: usize) -> &[f32] {
    &audio[..audio.len().saturating_sub(silence)]
}

/// The last `samples` of `audio`, or all of it when shorter.
fn tail(audio: &[f32], samples: usize) -> &[f32] {
    &audio[audio.len().saturating_sub(samples)..]
}

struct Scored {
    owner: Option<f32>,
    name: Option<SpeakerName>,
    matched_role: Option<Role>,
    embedding: Vec<f32>,
}

fn score<E: Embedder>(
    voices: &mut Diarizer<E>,
    timeline: &mut Timeline,
    audio: &[f32],
) -> Option<Scored> {
    if timeline.broken {
        return None;
    }
    let voice = match voices.embed(audio) {
        Ok(voice) => voice?,
        Err(error) => {
            tracing::warn!(error = %error_chain(&error), "the voice timeline could not score audio");
            timeline.broken = true;
            return None;
        }
    };
    let profile = voices.matched_profile(&voice);
    Some(Scored {
        owner: voices.owner_similarity(&voice),
        name: profile.map(|profile| profile.name.clone()),
        matched_role: profile.map(|profile| profile.role),
        embedding: voice,
    })
}

impl<E, S> super::sealed::Sealed for VoiceprintTracker<E, S> {}

impl<E: Embedder, S: Segmenter> SpeakerTracker for VoiceprintTracker<E, S> {
    fn serves(&self, admission: Admission) -> bool {
        match admission {
            Admission::OwnerOnly => self.can_filter(),
            Admission::Everyone => self.voices.knows_anyone(),
        }
    }

    fn enroll(&mut self, profiles: Vec<Profile>) {
        self.voices.set_enrolled(profiles);
    }

    fn restart(&mut self, at: u64, admission: Admission) {
        Self::restart(self, at);
        self.admission = admission;
    }

    fn heard_to(&self) -> u64 {
        self.position() + self.frame.len() as u64
    }

    fn hear(&mut self, speech: &SpeechMap, audio: &[f32]) {
        self.push(speech, audio);
    }

    fn skip_to(&mut self, speech: &SpeechMap, at: u64) {
        // Frames keep to the speech map's grid.
        Self::skip_to(self, speech.frame_start(at));
    }

    fn finish(&mut self) {
        Self::finish(self);
    }

    fn timeline(&self) -> &Timeline {
        &self.timeline
    }
}

fn log_check(turn: &Turn, score: Option<f32>, heard: usize) {
    tracing::debug!(
        similarity = score,
        heard_ms = crate::audio::canonical_duration(heard as u64).as_millis(),
        turn_from_ms = crate::audio::canonical_duration(turn.span.start).as_millis(),
        turn_to_ms = crate::audio::canonical_duration(turn.span.end).as_millis(),
        judgment = ?turn.judgment,
        "own-voice check"
    );
}
