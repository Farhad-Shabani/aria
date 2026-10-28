use std::time::Duration;

use super::{SpeakerTracker, Timeline};
use crate::audio::{CaptureError, RecordingReader, RecordingWaker, canonical_samples};
use crate::primitives::error_chain;
use crate::voice_activity::{SpeechDetector, SpeechMap, VoiceActivityDetector};

/// Feeds one recording to its speech detector and then to its tracker, on
/// a fork of its own, so turns are cut where the audio pauses, not where a
/// decoder finalizes. The tracker hears only what the detector has scored.
pub struct TurnFeed<V, K> {
    audio: RecordingReader,
    speech: SpeechDetector<V>,
    /// Where the next sample handed to the detector sits.
    spoken_to: u64,
    tracker: Option<K>,
}

/// How far [`TurnFeed::hear_ahead`] got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ahead {
    Heard,
    /// Nothing was left to hear: wait with [`TurnFeed::wait_for_audio`].
    Idle,
    /// Capture ended, and the detector and any tracker heard all of it.
    Ended,
}

impl<V: VoiceActivityDetector, K: SpeakerTracker> TurnFeed<V, K> {
    /// Call before anything reads `recording`, so the fork starts where it
    /// does. `speech` and `tracker` start where `recording` holds from.
    pub fn start(
        recording: &mut RecordingReader,
        speech: SpeechDetector<V>,
        tracker: Option<K>,
    ) -> Self {
        let audio = recording.fork_window();
        let from = audio.held_from();
        debug_assert_eq!(
            speech.map().reached(),
            from,
            "the detector starts at the fork"
        );
        debug_assert!(
            tracker
                .as_ref()
                .is_none_or(|tracker| tracker.heard_to() == from),
            "the tracker starts at the fork"
        );
        Self {
            audio,
            speech,
            spoken_to: from,
            tracker,
        }
    }

    /// The first sample the feed still holds: where the recording starts,
    /// until the feed has heard some of it.
    pub fn held_from(&self) -> u64 {
        self.audio.held_from()
    }

    pub fn speech(&self) -> &SpeechMap {
        self.speech.map()
    }

    /// `None` when no tracker runs.
    pub fn timeline(&self) -> Option<&Timeline> {
        self.tracker.as_ref().map(SpeakerTracker::timeline)
    }

    /// Stops judging voices. The detector goes on, and the audio only the
    /// tracker still needed is freed.
    pub fn drop_tracker(&mut self) {
        self.tracker = None;
        self.release();
    }

    /// Paced by a decoder: the detector, then the tracker, hear the audio up
    /// to stream `position`. Words a decoder has read never end past where
    /// it read, so their turns are cut by then. Returns whether the
    /// recording reached `position`.
    pub fn hear_through(&mut self, position: u64) -> bool {
        let filled = self.audio.fill_through(position);
        let reached = filled.as_ref().is_ok_and(|reached| *reached);
        self.speak(filled.map(drop), position);
        self.track_through(position);
        reached
    }

    /// Paced by itself: the detector takes all the audio that has arrived,
    /// the tracker at most `piece` of it.
    pub fn hear_ahead(&mut self, piece: Duration) -> Ahead {
        let before = self.heard_to();
        let filled = self.audio.fill_available();
        self.speak(filled, u64::MAX);
        let through = self.tracker.as_ref().map_or(self.spoken_to, |tracker| {
            tracker.heard_to() + canonical_samples(piece)
        });
        self.track_through(through);
        // Judged by what moved, not by why: a stage that stops hearing, for
        // whatever reason, cannot keep the feed spinning.
        if self.heard_to() != before {
            return Ahead::Heard;
        }
        if self.audio.ended_at().is_some() {
            return Ahead::Ended;
        }
        Ahead::Idle
    }

    /// Waits up to `timeout` for audio not heard yet, for capture to end,
    /// or for `stopped`. A stop that rings [`Self::waker`] ends the wait at
    /// once; one that is only polled is seen at the timeout.
    pub fn wait_for_audio(&self, timeout: Duration, stopped: impl Fn() -> bool) {
        self.audio.wait_for_audio(timeout, stopped);
    }

    /// Wakes a [`Self::wait_for_audio`] in progress.
    pub fn waker(&self) -> RecordingWaker {
        self.audio.waker()
    }

    /// Where the detector and the tracker will hear next.
    fn heard_to(&self) -> (u64, Option<u64>) {
        let tracked = self.tracker.as_ref().map(SpeakerTracker::heard_to);
        (self.spoken_to, tracked)
    }

    /// Hears the rest of the recording, waiting for capture to end, and
    /// judges what is still open, so every held segment can be decided.
    pub fn hear_rest(&mut self) {
        let filled = self.audio.fill();
        self.speak(filled, u64::MAX);
        self.track_through(u64::MAX);
        if let Some(tracker) = &mut self.tracker {
            tracker.finish();
        }
    }

    fn speak(&mut self, filled: Result<(), CaptureError>, position: u64) {
        // A failed capture ends the recording; what it held is still heard.
        if let Err(error) = filled {
            tracing::debug!(error = %error_chain(&error), "the voice feed heard capture end early");
        }
        let end = position.min(self.audio.held_to()).max(self.spoken_to);
        if !self.speech.map().is_broken()
            && let Err(error) = self.speech.hear(self.audio.span(self.spoken_to..end))
        {
            tracing::warn!(
                error = %error_chain(&error),
                "voice-activity inference failed; no more speech can be told apart"
            );
        }
        self.spoken_to = end;
    }

    /// The tracker hears up to stream `position`, but never past the audio
    /// the detector has had.
    fn track_through(&mut self, position: u64) {
        if let Some(tracker) = &mut self.tracker {
            let start = tracker.heard_to();
            let end = position.min(self.spoken_to).max(start);
            tracker.hear(self.speech.map(), self.audio.span(start..end));
        }
        self.release();
    }

    /// Frees the audio both the detector and the tracker are done with.
    fn release(&mut self) {
        let heard = self
            .tracker
            .as_ref()
            .map_or(self.spoken_to, SpeakerTracker::heard_to);
        self.audio.release_before(heard.min(self.spoken_to));
    }
}
