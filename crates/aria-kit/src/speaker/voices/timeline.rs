//! Who spoke in each frame, as the overlapping chunks voted.
//!
//! Three counts share one frame index: how many chunks judged a frame, how
//! many voices they heard in it, and how many of them named each voice. They
//! have to grow together, so one type owns all three rather than three
//! vectors growing by hand.
//!
//! Voices are held by position, parallel to the caller's own list. A merge
//! there is a merge here.

use std::ops::Range;

use crate::segmentation::Activity;

#[derive(Default)]
pub(super) struct Timeline {
    /// Chunks that judged each frame.
    judged: Vec<u8>,
    /// Voices those chunks heard in it, counting a chunk once per voice.
    heard: Vec<u8>,
    /// Per voice, the chunks that named it in each frame.
    votes: Vec<Vec<u8>>,
}

impl Timeline {
    /// Frames the timeline reaches.
    pub(super) fn len(&self) -> usize {
        self.judged.len()
    }

    /// Records that one chunk judged `frames`, starting at frame `at`.
    pub(super) fn judge(&mut self, at: usize, frames: &[Activity]) {
        grow(&mut self.judged, at + frames.len());
        grow(&mut self.heard, at + frames.len());
        for (offset, frame) in frames.iter().enumerate() {
            self.judged[at + offset] = self.judged[at + offset].saturating_add(1);
            self.heard[at + offset] = self.heard[at + offset].saturating_add(frame.voices() as u8);
        }
    }

    /// Starts counting for a voice heard for the first time.
    pub(super) fn open_voice(&mut self) {
        self.votes.push(Vec::new());
    }

    /// Records that `voice` is the chunk's `local` speaker over `frames`.
    pub(super) fn vote(&mut self, voice: usize, at: usize, frames: &[Activity], local: usize) {
        let votes = &mut self.votes[voice];
        grow(votes, at + frames.len());
        for (offset, frame) in frames.iter().enumerate() {
            if frame.speaks(local) {
                votes[at + offset] = votes[at + offset].saturating_add(1);
            }
        }
    }

    /// Folds one voice's votes into another's and drops it, so positions
    /// stay parallel to a caller that removed the same entry.
    pub(super) fn absorb(&mut self, keeper: usize, absorbed: usize) {
        let taken = self.votes.remove(absorbed);
        let keeper = match keeper > absorbed {
            true => keeper - 1,
            false => keeper,
        };
        let kept = &mut self.votes[keeper];
        grow(kept, taken.len());
        for (kept, gained) in kept.iter_mut().zip(&taken) {
            *kept = kept.saturating_add(*gained);
        }
    }

    /// Who speaks in one frame: the voices with the most votes, as many of
    /// them as the chunks covering it heard on average.
    ///
    /// `eligible` masks the caller's voices — one too rarely heard to be a
    /// person names nothing, but its votes still count toward the ranking it
    /// loses.
    pub(super) fn speakers_in(&self, frame: usize, eligible: &[bool]) -> Vec<usize> {
        let wanted = self.voices_in(frame);
        if wanted == 0 {
            return Vec::new();
        }
        let mut ranked: Vec<usize> = (0..self.votes.len())
            .filter(|&voice| eligible.get(voice).copied().unwrap_or(false))
            .filter(|&voice| self.votes_at(voice, frame) > 0)
            .collect();
        ranked.sort_by_key(|&voice| std::cmp::Reverse(self.votes_at(voice, frame)));
        ranked.truncate(wanted);
        ranked
    }

    /// Every stretch each eligible voice owns, in one pass over the frames.
    ///
    /// A silence of up to `gap` frames closes rather than splitting a turn in
    /// two; a run under `least` is noise and is dropped.
    pub(super) fn runs(
        &self,
        eligible: &[bool],
        gap: usize,
        least: usize,
    ) -> Vec<(usize, Range<usize>)> {
        let mut open: Vec<Vec<Range<usize>>> = vec![Vec::new(); self.votes.len()];
        for frame in 0..self.len() {
            for voice in self.speakers_in(frame, eligible) {
                match open[voice].last_mut() {
                    Some(run) if frame - run.end <= gap => run.end = frame + 1,
                    _ => open[voice].push(frame..frame + 1),
                }
            }
        }
        open.into_iter()
            .enumerate()
            .flat_map(|(voice, runs)| {
                runs.into_iter()
                    .filter(|run| run.len() >= least)
                    .map(move |run| (voice, run))
            })
            .collect()
    }

    /// How many voices spoke in a frame, averaged over the chunks that
    /// judged it — so one briefly confused chunk cannot invent a person.
    fn voices_in(&self, frame: usize) -> usize {
        let judged = self.judged.get(frame).copied().unwrap_or(0);
        let heard = self.heard.get(frame).copied().unwrap_or(0);
        match judged {
            0 => 0,
            judged => (f32::from(heard) / f32::from(judged) + 0.5) as usize,
        }
    }

    fn votes_at(&self, voice: usize, frame: usize) -> u8 {
        self.votes[voice].get(frame).copied().unwrap_or(0)
    }
}

fn grow(counts: &mut Vec<u8>, through: usize) {
    if counts.len() < through {
        counts.resize(through, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames in which each listed local speaks.
    fn frames(pattern: &[&[usize]]) -> Vec<Activity> {
        pattern
            .iter()
            .map(|speaking| {
                speaking
                    .iter()
                    .fold(Activity::SILENT, |active, &voice| active.speaking(voice))
            })
            .collect()
    }

    /// A timeline of one chunk in which `voices` local speakers were heard.
    fn heard(voices: usize, pattern: &[&[usize]]) -> Timeline {
        let mut timeline = Timeline::default();
        let chunk = frames(pattern);
        timeline.judge(0, &chunk);
        for local in 0..voices {
            timeline.open_voice();
            timeline.vote(local, 0, &chunk, local);
        }
        timeline
    }

    #[test]
    fn a_frame_names_as_many_voices_as_the_chunks_heard() {
        let timeline = heard(2, &[&[0, 1], &[0]]);
        let both = [true, true];
        assert_eq!(timeline.speakers_in(0, &both).len(), 2);
        assert_eq!(timeline.speakers_in(1, &both), Vec::from([0]));
    }

    #[test]
    fn a_voice_the_caller_rules_out_names_nothing() {
        let timeline = heard(2, &[&[0, 1]]);
        let named = timeline.speakers_in(0, &[false, true]);
        assert_eq!(named, Vec::from([1]));
    }

    #[test]
    fn a_short_silence_closes_rather_than_splitting_a_turn() {
        let timeline = heard(1, &[&[0], &[], &[0], &[0]]);
        let runs = timeline.runs(&[true], 1, 1);
        assert_eq!(runs, Vec::from([(0, 0..4)]));
    }

    #[test]
    fn a_run_too_short_to_be_a_turn_is_dropped() {
        let timeline = heard(1, &[&[0], &[], &[], &[0]]);
        // The gap splits them, and neither half reaches two frames.
        assert!(timeline.runs(&[true], 1, 2).is_empty());
    }

    #[test]
    fn absorbing_a_voice_keeps_the_rest_parallel() {
        let mut timeline = heard(3, &[&[0, 1], &[1, 2]]);
        timeline.absorb(0, 1);
        // Voice 1's votes went to voice 0; what was voice 2 is now voice 1.
        assert_eq!(timeline.votes_at(0, 1), 1);
        assert_eq!(timeline.votes_at(1, 1), 1);
        assert_eq!(timeline.votes.len(), 2);
    }

    #[test]
    fn absorbing_a_lower_voice_into_a_higher_one_still_lands() {
        let mut timeline = heard(3, &[&[0], &[1], &[2]]);
        timeline.absorb(2, 0);
        // Removing voice 0 shifts the keeper down to 1.
        assert_eq!(timeline.votes.len(), 2);
        assert_eq!(timeline.votes_at(1, 0), 1, "voice 0's frame came along");
        assert_eq!(timeline.votes_at(1, 2), 1, "and the keeper kept its own");
    }
}
