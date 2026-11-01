//! Speaker fakes the hearing and dictation tests share.

use aria_kit::speaker::{
    Diarizer, Embedder, LevelSegmenter, Profile, Role, SpeakerError, VoiceprintTracker,
    enrolled_voice,
};

/// The owner's voice.
pub(crate) const OWN: [f32; 2] = [1.0, 0.0];

/// A speaker model that hears one fixed voice in every window.
pub(crate) struct Voice(pub(crate) [f32; 2]);

impl Embedder for Voice {
    fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
        Ok(Some(self.0.to_vec()))
    }

    fn dimension(&self) -> usize {
        2
    }
}

/// The owner's profile, the only one enrolled.
pub(crate) fn alex() -> Vec<Profile> {
    vec![enrolled_voice("alex", Role::Owner, OWN.to_vec())]
}

/// A diarizer that hears `embedder` in every window of sound, with only
/// [`alex`] enrolled.
pub(crate) fn diarizer<E: Embedder>(embedder: E) -> Diarizer<E> {
    Diarizer::new(embedder, alex())
}

/// The tracker an owner-only press runs, starting at `at`.
pub(crate) fn tracker<E: Embedder>(
    diarizer: &mut Diarizer<E>,
    at: u64,
) -> VoiceprintTracker<'_, E, LevelSegmenter> {
    VoiceprintTracker::owner_only(diarizer, LevelSegmenter, at)
        .expect("alex is the owner, and a level segmenter suits any embedder")
}
