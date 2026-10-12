//! Speaking synthesized audio through the default output device.

use super::SpeakerOutput;
use crate::cancel::CancelToken;

use super::{Engine, SpeakingRate, SpeechText, Synthesis, SynthesisError, Voice, stream};

impl<T: Engine> Synthesis<T> {
    /// Speaks with the engine's default voice at its natural pace and waits
    /// until the utterance has been heard.
    pub fn speak(&self, text: &SpeechText) -> Result<(), SynthesisError> {
        self.speak_with(text, None, SpeakingRate::standard(), &CancelToken::never())
    }

    /// Speaks one utterance and waits until it has been heard.
    pub fn speak_with(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError> {
        if cancel.is_cancelled() {
            return Err(SynthesisError::Cancelled);
        }

        if self.engine.plays_audio_itself() {
            let (writer, speech) = stream(self.engine.sample_rate());
            let outcome = self
                .engine
                .synthesize_with(text, voice, rate, writer, cancel);
            drop(speech);
            return outcome;
        }

        let output = SpeakerOutput::new().map_err(|source| SynthesisError::Playback { source })?;
        self.start_with(text, voice, rate, cancel)?
            .play_on(&output, || {})
    }
}
