use std::ops::Range;
use std::time::Duration;

use super::{Embedding, Judgment, LabeledTurn, SpeakerTracker, Timeline, Turn};
use crate::audio::canonical_samples;
use crate::primitives::error_chain;
use crate::segmentation::{Activity, ChunkShape, SegmentationError, Segmenter};
use crate::speaker::changes::{Continuity, Evidence};
use crate::speaker::{Diarizer, Embedder, Label, Role, SpeakerName};
use crate::voice_activity::{LEAD_IN_FRAMES, Progress, SpeechMap};

mod reading;
#[cfg(test)]
mod tests;

use reading::{LEAST_READ_SAMPLES, Reading, read};

/// How much new audio a turn takes before its voice is checked again.
const CHECK_EVERY_SAMPLES: usize = canonical_samples(Duration::from_millis(500)) as usize;
/// A turn this long that has not passed as the owner's is another voice's.
/// No check scores more of a turn than this.
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

/// Cuts one recording into turns and judges each one. The diarizer scores
/// whose voice a window is. The segmenter, when there is one, checks a
/// speaker change inside a turn before the turn splits, which owner-only
/// admission needs.
pub struct VoiceprintTracker<'a, E, S = NoSegmenter> {
    diarizer: &'a mut Diarizer<E>,
    labeling: Labeling,
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
    continuity: Option<Continuity<S>>,
}

/// Which voices a tracker labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Labeling {
    /// Enrolled voices only, by name.
    Enrolled,
    /// Every voice, numbering those nobody enrolled. Each check's embedding
    /// stays in the timeline, so a session can relabel its voices once it
    /// ends.
    Numbered,
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
    /// The sum of the embeddings heard since the turn was last labeled,
    /// each weighted by the speech it added.
    embedding: Vec<f32>,
    /// How much speech the recording held when the turn was last labeled.
    labeled: u64,
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

/// Why no tracker can admit only the owner's words.
#[derive(Debug, thiserror::Error)]
pub enum OwnerTrackingError {
    #[error("no owner is enrolled")]
    NoOwner,
    #[error("the segmenter cannot check speaker changes")]
    Segmenter(#[source] SegmentationError),
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

impl<'a, E: Embedder> VoiceprintTracker<'a, E> {
    /// A tracker that names enrolled voices but cannot admit only the
    /// owner. The first sample heard sits at `at`.
    pub fn new(diarizer: &'a mut Diarizer<E>, at: u64) -> Self {
        Self::build(diarizer, None, Labeling::Enrolled, at)
    }

    /// A tracker that labels every voice, numbering those nobody enrolled.
    /// It has no segmenter, so a speaker change splits a turn unchecked.
    pub fn numbering(diarizer: &'a mut Diarizer<E>, at: u64) -> Self {
        Self::build(diarizer, None, Labeling::Numbered, at)
    }
}

impl<'a, E: Embedder, S: Segmenter> VoiceprintTracker<'a, E, S> {
    /// A tracker that can admit only the owner's words: it needs an
    /// enrolled owner, and a segmenter to check speaker changes.
    pub fn owner_only(
        diarizer: &'a mut Diarizer<E>,
        segmenter: S,
        at: u64,
    ) -> Result<Self, OwnerTrackingError> {
        if diarizer.owners() == 0 {
            return Err(OwnerTrackingError::NoOwner);
        }
        let continuity = Continuity::new(segmenter, at).map_err(OwnerTrackingError::Segmenter)?;
        Ok(Self::build(
            diarizer,
            Some(continuity),
            Labeling::Enrolled,
            at,
        ))
    }

    fn build(
        diarizer: &'a mut Diarizer<E>,
        continuity: Option<Continuity<S>>,
        labeling: Labeling,
        at: u64,
    ) -> Self {
        Self {
            diarizer,
            labeling,
            frame_samples: 0,
            scored: FrameSpeech::default(),
            rejoin: false,
            frame: Vec::new(),
            lead_in: Vec::new(),
            timeline: Timeline {
                reached: at,
                ..Timeline::default()
            },
            open: None,
            continuity,
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
        self.timeline.labels.push(LabeledTurn {
            span: self.position()..at,
            label: None,
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

    fn push_frame(&mut self, speech: &SpeechMap, samples: &[f32]) {
        let start = self.position();
        let end = start + samples.len() as u64;
        if end > speech.reached() {
            // Past where the detector failed, nothing can be told apart.
            debug_assert!(speech.is_broken(), "a frame is heard before it is scored");
            self.timeline.break_off();
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

    /// How much silence the open turn ends in so far.
    fn trailing_silence(&self) -> usize {
        usize::try_from(self.scored.trailing_silence).unwrap_or(usize::MAX)
    }

    /// Checks land every [`CHECK_EVERY_SAMPLES`] of turn audio, however the
    /// audio was split into pushes.
    fn hear(&mut self, mut samples: &[f32], silence: usize) {
        while !samples.is_empty() && !self.timeline.is_broken() {
            let room = self.open.as_ref().map_or(samples.len(), |open| {
                CHECK_EVERY_SAMPLES.saturating_sub(open.unchecked).max(1)
            });
            let (now, rest) = samples.split_at(room.min(samples.len()));
            samples = rest;
            if let Some(continuity) = &mut self.continuity
                && let Err(error) = continuity.push(self.timeline.reached, now)
            {
                tracing::warn!(error = %error_chain(&error), "voice continuity could not retain audio");
                self.timeline.break_off();
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
            if let Some(turn) = self.timeline.labels.last_mut() {
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
        // The lead-in holds only audio heard since the last turn or a skip,
        // so the turn cannot reach back past either.
        let audio = std::mem::take(&mut self.lead_in);
        let start = at.saturating_sub(audio.len() as u64);
        self.timeline.turns.push(Turn {
            span: start..at,
            judgment: Judgment::Pending,
        });
        self.timeline.labels.push(LabeledTurn {
            span: start..at,
            label: None,
        });
        self.open = Some(OpenTurn {
            audio,
            unchecked: 0,
            scored: false,
            doubted: false,
            anchor: None,
            proposed: None,
            embedding: Vec::new(),
            labeled: self
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
            && let Some(judgment) = open.verdict(turn_length(turn), true, self.diarizer.owners())
        {
            turn.judgment = judgment;
        }
        self.settle_unverified();
        self.timeline.open = self.open.is_some();
    }

    /// A turn that closed too short to judge by its voice is another voice's
    /// when the segmenter hears the voice change from the owner's latest
    /// turn. It never becomes the owner's this way: that takes the owner's
    /// voiceprint.
    fn settle_unverified(&mut self) {
        let Some(turn) = self.timeline.turns.last() else {
            return;
        };
        if turn.judgment != Judgment::Unverified {
            return;
        }
        let candidate = turn.span.clone();
        let Some(owner) = self
            .timeline
            .turns
            .iter()
            .rev()
            .skip(1)
            .find(|turn| turn.judgment == Judgment::Owner)
            .map(|turn| {
                turn.span
                    .end
                    .saturating_sub(RECENT_SAMPLES as u64)
                    .max(turn.span.start)..turn.span.end
            })
        else {
            return;
        };
        let Some(continuity) = &mut self.continuity else {
            return;
        };
        if owner.is_empty() || continuity.audio(&owner).is_err() {
            return;
        }
        match continuity.verify(Some(&owner), &candidate, TURN_JUDGED_SAMPLES) {
            Ok(Evidence::Replaced { .. }) => {
                if let Some(turn) = self.timeline.turns.last_mut() {
                    turn.judgment = Judgment::Other;
                }
            }
            Ok(Evidence::Reassess(_) | Evidence::Unresolved) => {}
            Err(error) => {
                tracing::warn!(error = %error_chain(&error), "voice continuity could not be judged");
                self.timeline.break_off();
            }
        }
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
        let voice = score(self.diarizer, &mut self.timeline, spoken);
        let score = voice.as_ref().and_then(|voice| voice.owner);
        let matched_guest = voice
            .as_ref()
            .is_some_and(|voice| voice.matched_role == Some(Role::Guest));
        let reading = score.map(|score| read(score, spoken.len()));
        let passed = reading == Some(Reading::Owner);
        let open = self.open.as_mut().expect("the checked turn is open");
        // An earlier check heard another voice in this turn, so a passing one
        // means the owner began partway through it.
        let owner_began = passed && open.doubted;
        // Once doubted, the anchor stays on the voice that raised the doubt.
        // A later window can reach into the owner's speech.
        if voice.is_some() && !input.is_empty() && !open.doubted {
            open.anchor = Some(input.clone());
        }
        open.scored |= score.is_some();
        open.doubted |= matched_guest
            || reading == Some(Reading::Other)
            || score.is_some_and(|score| !score.is_finite());
        let verdict = open.verdict(length, closing, self.diarizer.owners());
        if let Some(voice) = voice {
            let heard = self.assign_label(input, &voice);
            self.label_turn(&voice.embedding, heard);
        }
        let turn = self
            .timeline
            .turns
            .last_mut()
            .expect("the checked turn is open");
        // Once the owner began, the verified change decides the doubted part.
        if !owner_began {
            if passed {
                turn.judgment = Judgment::Owner;
            } else if let Some(judgment) = verdict {
                turn.judgment = judgment;
            }
        }
        log_check(turn, score, length);
        if owner_began {
            let doubted = self.timeline.turns.len() - 1;
            self.propose_owner_start(silence);
            // A split the segmenter could not verify leaves the doubted part
            // behind the owner's proposed start.
            if doubted + 1 < self.timeline.turns.len()
                && self.timeline.turns[doubted].judgment == Judgment::Pending
            {
                self.timeline.turns[doubted].judgment = Judgment::Other;
            }
            return;
        }
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
        let recent_end = self
            .position()
            .saturating_sub(silence.min(open.audio.len()) as u64);
        let window = recent_end.saturating_sub(recent_len as u64)..recent_end;
        let Some(voice) = score(self.diarizer, &mut self.timeline, recent) else {
            return;
        };
        let similarity = voice.owner;
        tracing::debug!(
            similarity,
            at_ms = crate::audio::canonical_duration(self.position()).as_millis(),
            judged = ?judged,
            "speaker change watch"
        );
        let at = window.start;
        let heard = self.assign_label(window, &voice);
        let label_changed = self
            .timeline
            .labels
            .last()
            .is_some_and(|turn| turn.label != heard);
        if can_split && label_changed {
            self.split_label(at, heard.clone());
        }
        self.label_turn(&voice.embedding, heard);
        match judged {
            Judgment::Owner
                if can_split
                    && similarity
                        .is_some_and(|score| read(score, recent_len) == Reading::Other) =>
            {
                // Not before audio an earlier check heard as the owner's.
                let unstable = self
                    .position()
                    .saturating_sub(silence as u64)
                    .saturating_sub(self.timeline.stable_to);
                let changed = recent_len.min(unstable as usize);
                self.split(voiced_len - changed, Judgment::Pending, similarity, true);
                return;
            }
            Judgment::Other
                if can_split
                    && similarity
                        .is_some_and(|score| read(score, recent_len) == Reading::Owner) =>
            {
                if self.propose_owner_start(silence) {
                    return;
                }
            }
            Judgment::Pending | Judgment::Unverified | Judgment::Owner | Judgment::Other => {}
        }
        if can_split && judged == Judgment::Owner {
            self.advance_stable_to_check(silence, similarity);
        }
    }

    /// Proposes where the owner began in the open turn: at a pause, or else
    /// within the newest second. `false` when that second is not the owner's.
    fn propose_owner_start(&mut self, silence: usize) -> bool {
        if let Some((from, similarity)) = self.owner_since_a_pause(silence) {
            self.propose_owner(from, silence, Some(similarity));
            return true;
        }
        let Some(open) = self.open.as_ref() else {
            return false;
        };
        let voiced = spoken(&open.audio, silence);
        let newest = tail(voiced, LEAST_READ_SAMPLES);
        let Some(newest_voice) = score(self.diarizer, &mut self.timeline, newest) else {
            return false;
        };
        if !newest_voice
            .owner
            .is_some_and(|score| read(score, newest.len()) == Reading::Owner)
        {
            return false;
        }
        self.propose_owner(voiced.len() - newest.len(), silence, newest_voice.owner);
        true
    }

    fn propose_owner(&mut self, from: usize, silence: usize, similarity: Option<f32>) {
        if self.continuity.is_none() {
            self.split(from, Judgment::Owner, similarity, false);
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
        } else if !self.timeline.is_broken() {
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
            self.split_label(from, None);
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
        if self.timeline.is_broken() {
            return;
        }
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if let Some(judgment) = open.verdict(length, closing, self.diarizer.owners())
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
            self.timeline.break_off();
            return None;
        };
        let evidence = match continuity.verify(anchor.as_ref(), candidate, TURN_JUDGED_SAMPLES) {
            Ok(evidence) => evidence,
            Err(error) => {
                tracing::warn!(error = %error_chain(&error), "voice continuity could not be judged");
                self.timeline.break_off();
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
            self.timeline.break_off();
            return None;
        }
        let reading = read(similarity, (range.end - range.start) as usize);
        let judgment = if reading == Reading::Owner {
            Judgment::Owner
        } else if reassessed
            || reading == Reading::Other
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
            self.timeline.break_off();
            return None;
        };
        let audio = match continuity.audio(range) {
            Ok(audio) => audio,
            Err(error) => {
                tracing::warn!(error = %error_chain(&error), "voice evidence was not retained");
                self.timeline.break_off();
                return None;
            }
        };
        score(self.diarizer, &mut self.timeline, audio)
    }

    fn apply_change(&mut self, judgment: Judgment, range: Range<u64>, voice: Scored) {
        if judgment == Judgment::Owner {
            // Evidence from the pending turn's start judges all of it: none of
            // its audio has been let out, so nothing stable is revised.
            let replace = self.timeline.turns.last().is_some_and(|turn| {
                turn.judgment == Judgment::Pending && range.start <= turn.span.start
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
            self.split_label(at, voice.name.map(Label::Enrolled));
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
        self.split(from, judgment, similarity, false);
    }

    /// Who the audio over `span` sounds like: an enrolled voice, or, when
    /// the tracker numbers voices, a numbered one. The voice speaking last
    /// keeps its number through a close call.
    fn assign_label(&mut self, span: Range<u64>, voice: &Scored) -> Option<Label> {
        if self.labeling == Labeling::Numbered {
            self.timeline.embeddings.push(Embedding {
                span,
                vector: voice.embedding.clone(),
            });
        }
        if let Some(name) = &voice.name {
            return Some(Label::Enrolled(name.clone()));
        }
        if self.labeling == Labeling::Enrolled {
            return None;
        }
        let speaking = self
            .timeline
            .labels
            .iter()
            .rev()
            .find_map(|turn| turn.label.as_ref());
        Some(self.diarizer.label(&voice.embedding, speaking))
    }

    /// Labels the open turn by its whole voice so far. `heard` is the label
    /// of its latest window, which a turn nobody enrolled keeps.
    fn label_turn(&mut self, embedding: &[f32], heard: Option<Label>) {
        let (Some(open), Some(turn)) = (self.open.as_mut(), self.timeline.labels.last_mut()) else {
            return;
        };
        let speech = self.scored.speech_before;
        let weight = speech.saturating_sub(open.labeled);
        open.labeled = speech;
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
            turn.label = match self.diarizer.matched_profile(&voice) {
                Some(profile) => Some(Label::Enrolled(profile.name.clone())),
                None => heard.filter(|label| label.enrolled().is_none()),
            };
        }
    }

    fn split_label(&mut self, at: u64, label: Option<Label>) {
        let end = self.position();
        if let Some(turn) = self.timeline.labels.last_mut() {
            let at = at.max(turn.span.start).min(end);
            turn.span.end = at;
            self.timeline.labels.push(LabeledTurn {
                span: at..end,
                label,
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
            let Some(voice) = score(self.diarizer, &mut self.timeline, &voiced[index..]) else {
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
    /// new turn there. `doubted` when `score` read as another voice.
    fn split(&mut self, from: usize, judgment: Judgment, score: Option<f32>, doubted: bool) {
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
        open.doubted = doubted;
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
    diarizer: &mut Diarizer<E>,
    timeline: &mut Timeline,
    audio: &[f32],
) -> Option<Scored> {
    if timeline.is_broken() {
        return None;
    }
    let voice = match diarizer.embed(audio) {
        Ok(voice) => voice?,
        Err(error) => {
            tracing::warn!(error = %error_chain(&error), "the voice timeline could not score audio");
            timeline.break_off();
            return None;
        }
    };
    let profile = diarizer.matched_profile(&voice);
    Some(Scored {
        owner: diarizer.owner_similarity(&voice),
        name: profile.map(|profile| profile.name.clone()),
        matched_role: profile.map(|profile| profile.role),
        embedding: voice,
    })
}

impl<E, S> super::sealed::Sealed for VoiceprintTracker<'_, E, S> {}

impl<E: Embedder, S: Segmenter> SpeakerTracker for VoiceprintTracker<'_, E, S> {
    fn heard_to(&self) -> u64 {
        self.position() + self.frame.len() as u64
    }

    fn hear(&mut self, speech: &SpeechMap, mut audio: &[f32]) {
        self.frame_samples = speech.frame_samples();
        while !audio.is_empty() && !self.timeline.is_broken() {
            let taken = audio.len().min(self.frame_samples - self.frame.len());
            self.frame.extend_from_slice(&audio[..taken]);
            audio = &audio[taken..];
            if self.frame.len() == self.frame_samples {
                let mut frame = std::mem::take(&mut self.frame);
                self.push_frame(speech, &frame);
                frame.clear();
                self.frame = frame;
            }
        }
    }

    fn skip_to(&mut self, speech: &SpeechMap, at: u64) {
        // Frames keep to the speech map's grid.
        Self::skip_to(self, speech.frame_start(at));
    }

    fn finish(&mut self) {
        if self.timeline.is_broken() {
            return;
        }
        self.timeline.progress = Progress::Finished;
        let frame = std::mem::take(&mut self.frame);
        self.hear(&frame, self.trailing_silence());
        self.close_turn(self.trailing_silence());
    }

    fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    fn forget_before(&mut self, at: u64) -> u64 {
        // The open turn keeps at most a judged turn's length of audio.
        let at = at.min(self.heard_to().saturating_sub(TURN_JUDGED_SAMPLES as u64));
        // The open turn is still being judged, so nothing from its start on
        // is forgotten.
        let open = self
            .open
            .as_ref()
            .and(self.timeline.turns.last())
            .map_or(at, |turn| turn.span.start);
        self.timeline.forget_before(at.min(open));
        at
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
