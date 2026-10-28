//! Whether a recording held speech. A decoder can name words in silence
//! that look like real ones, so the audio decides.

use aria_kit::voice_activity::SpeechMap;

/// Whether the speech map holds speech before `position`. A map that could
/// not be judged counts as speech: losing words is worse than letting words
/// decoded from silence through.
pub(super) fn spoken_by(speech: &SpeechMap, position: u64) -> bool {
    if speech.is_broken() {
        tracing::warn!("the recording could not be judged for speech; its words pass");
        return true;
    }
    speech.speech_before(position) > 0
}

pub(super) fn held_speech(speech: &SpeechMap) -> bool {
    if spoken_by(speech, speech.reached()) {
        return true;
    }
    tracing::info!("no speech detected in the recording; its words are dropped");
    false
}

#[cfg(test)]
mod tests {
    use aria_kit::voice_activity::SpeechDetector;

    use super::*;
    use crate::dictation::testing::Loud;

    #[test]
    fn a_map_that_could_not_be_judged_counts_as_speech() {
        let mut detector = SpeechDetector::new(Loud, 0);
        detector.hear(&[0.0; 16_000]).unwrap();
        let mut speech = detector.map().clone();
        speech.break_off();

        assert!(held_speech(&speech));
    }
}
