use super::{Admission, SpeakerTracker};
use crate::audio::{CaptureError, RecordingReader};
use crate::primitives::error_chain;
use crate::transcription::TranscriptionError;
use crate::voice_activity::{SpeechDetector, SpeechMap, VoiceActivityDetector};

/// Feeds a live recording to the speech detector and then to a tracker, on
/// a fork of its own, so turns are cut where the audio pauses, not where a
/// decoder finalizes. The detector always runs ahead of the tracker.
pub struct TurnFeed<'a, V, K> {
    audio: RecordingReader,
    admission: Admission,
    spoken_to: u64,
    speech: &'a mut SpeechDetector<V>,
    voices: Option<&'a mut K>,
}

impl<'a, V: VoiceActivityDetector, K: SpeakerTracker> TurnFeed<'a, V, K> {
    /// Call before anything reads `recording`, so the fork starts where it
    /// does. Owner-only admission needs a tracker that serves it.
    pub fn start(
        speech: &'a mut SpeechDetector<V>,
        mut voices: Option<&'a mut K>,
        recording: &mut RecordingReader,
        admission: Admission,
    ) -> Result<Self, TranscriptionError> {
        if admission == Admission::OwnerOnly
            && !voices
                .as_deref()
                .is_some_and(|voices| voices.serves(admission))
        {
            return Err(TranscriptionError::SpeakerTracking);
        }
        let audio = recording.fork_window();
        let from = audio.held_from();
        speech.restart(from);
        if let Some(voices) = voices.as_deref_mut() {
            voices.restart(from, admission);
        }
        Ok(Self {
            audio,
            admission,
            spoken_to: from,
            speech,
            voices,
        })
    }

    pub fn admission(&self) -> Admission {
        self.admission
    }

    /// The first sample the feed still holds: where the recording starts,
    /// until the feed has heard some of it.
    pub fn held_from(&self) -> u64 {
        self.audio.held_from()
    }

    pub fn written(&self) -> u64 {
        self.audio.written()
    }

    /// Where the next sample handed to the speech detector sits.
    pub fn spoken_to(&self) -> u64 {
        self.spoken_to
    }

    pub fn capture_ended(&self) -> bool {
        self.audio.ended_at().is_some()
    }

    /// Waits up to `timeout` for audio not yet heard, or for capture to end.
    pub fn wait_for_audio(&self, timeout: std::time::Duration) {
        self.audio.wait_for_audio(timeout);
    }

    pub fn speech(&self) -> &SpeechMap {
        self.speech.map()
    }

    /// The tracker, while one judges voices.
    pub fn voices(&self) -> Option<&K> {
        self.voices.as_deref()
    }

    /// Stops judging voices. The detector goes on, and the audio only the
    /// tracker still needed is freed.
    pub fn drop_voices(&mut self) {
        self.voices = None;
        self.release();
    }

    /// The detector hears whatever audio has arrived, without waiting.
    /// What the tracker has not heard yet stays for it.
    pub fn hear_speech(&mut self) {
        let filled = self.audio.fill_available();
        self.speak(filled, u64::MAX);
        self.release();
    }

    /// The tracker hears up to stream `position`, but never past the audio
    /// the detector has had.
    pub fn hear_voices_through(&mut self, position: u64) {
        if let Some(voices) = self.voices.as_deref_mut() {
            let start = voices.heard_to();
            let end = position.min(self.spoken_to).max(start);
            voices.hear(self.speech.map(), self.audio.span(start..end));
        }
        self.release();
    }

    /// The tracker skips the audio before `position`; the detector still
    /// hears it.
    pub fn skip_voices_to(&mut self, position: u64) {
        let filled = self.audio.fill_available();
        self.speak(filled, position);
        if let Some(voices) = self.voices.as_deref_mut() {
            voices.skip_to(self.speech.map(), position.min(self.spoken_to));
        }
        self.release();
    }

    /// The detector, then the tracker, hear the audio up to stream
    /// `position`. Words a decoder has read never end past where it read,
    /// so their turns are cut by then.
    pub fn hear_through(&mut self, position: u64) {
        let filled = self.audio.fill_through(position).map(drop);
        self.speak(filled, position);
        self.hear_voices_through(position);
    }

    /// Hears the rest of the recording, waiting for capture to end, and
    /// judges what is still open, so every held segment can be decided.
    pub fn hear_rest(&mut self) {
        let filled = self.audio.fill();
        self.speak(filled, u64::MAX);
        self.hear_voices_through(u64::MAX);
        if let Some(voices) = self.voices.as_deref_mut() {
            voices.finish();
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

    /// Frees the audio both the detector and the tracker are done with.
    fn release(&mut self) {
        let heard = self
            .voices
            .as_deref()
            .map_or(self.spoken_to, |voices| voices.heard_to());
        self.audio.release_before(heard.min(self.spoken_to));
    }
}
