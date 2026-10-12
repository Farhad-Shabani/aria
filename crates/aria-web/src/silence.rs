//! Deciding when the speaker has stopped.
//!
//! Only the wasm binding drives this, but the logic is plain arithmetic, so a
//! native build still compiles it to run the tests.
#![cfg_attr(not(target_family = "wasm"), allow(dead_code))]

use aria_kit::audio::CANONICAL_SAMPLE_RATE;

/// Frames at or below this RMS level count as quiet.
const QUIET_RMS: f32 = 0.01;

/// The longest pause a caller may ask to wait through.
const MAX_TRAILING_SILENCE_MS: u32 = 30_000;

/// Calls an utterance over once the speaker has been quiet long enough.
///
/// The gate starts counting silence only after detecting speech.
///
/// Loudness alone decides, so steady noise — a fan, a keyboard — holds it open.
pub(crate) struct SilenceGate {
    heard_speech: bool,
    quiet_samples: u32,
    enough: u32,
}

impl SilenceGate {
    /// Accepts pauses from 1 ms through [`MAX_TRAILING_SILENCE_MS`].
    pub(crate) fn new(trailing_silence_ms: u32) -> Result<Self, SilenceGateError> {
        if trailing_silence_ms == 0 || trailing_silence_ms > MAX_TRAILING_SILENCE_MS {
            return Err(SilenceGateError::OutOfRange {
                actual: trailing_silence_ms,
                maximum: MAX_TRAILING_SILENCE_MS,
            });
        }
        Ok(Self {
            heard_speech: false,
            quiet_samples: 0,
            enough: CANONICAL_SAMPLE_RATE / 1_000 * trailing_silence_ms,
        })
    }

    /// Takes one frame and tracks how long the quiet has run.
    pub(crate) fn hear(&mut self, samples: &[f32]) {
        if loud(samples) {
            self.heard_speech = true;
            self.quiet_samples = 0;
            return;
        }
        if self.heard_speech {
            self.quiet_samples = self.quiet_samples.saturating_add(samples.len() as u32);
        }
    }

    /// Whether the speaker has spoken and then stopped.
    pub(crate) fn stopped(&self) -> bool {
        self.heard_speech && self.quiet_samples >= self.enough
    }
}

/// Compares mean square against the squared threshold, which answers the same
/// question as a root-mean-square without the square root.
fn loud(samples: &[f32]) -> bool {
    if samples.is_empty() {
        return false;
    }
    let sum: f32 = samples.iter().map(|sample| sample * sample).sum();
    sum / samples.len() as f32 > QUIET_RMS * QUIET_RMS
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SilenceGateError {
    #[error("trailing silence must be between 1 and {maximum} ms; got {actual}")]
    OutOfRange { actual: u32, maximum: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 128;

    fn quiet() -> Vec<f32> {
        vec![0.0; FRAME]
    }

    fn speech() -> Vec<f32> {
        vec![0.5; FRAME]
    }

    /// Frames of silence needed to reach the configured pause.
    fn frames_for(milliseconds: u32) -> usize {
        (CANONICAL_SAMPLE_RATE / 1_000 * milliseconds) as usize / FRAME
    }

    #[test]
    fn silence_before_the_first_word_never_ends_the_utterance() {
        let mut gate = SilenceGate::new(1_500).expect("a sane pause is accepted");

        for _ in 0..frames_for(1_500) * 10 {
            gate.hear(&quiet());
        }

        assert!(!gate.stopped(), "leading silence must not end anything");
    }

    #[test]
    fn silence_after_speech_ends_the_utterance() {
        let mut gate = SilenceGate::new(1_500).expect("a sane pause is accepted");
        gate.hear(&speech());
        assert!(!gate.stopped());

        for _ in 0..=frames_for(1_500) {
            gate.hear(&quiet());
        }

        assert!(gate.stopped(), "the pause has run long enough");
    }

    #[test]
    fn speech_resets_a_pause_that_had_started() {
        let mut gate = SilenceGate::new(1_500).expect("a sane pause is accepted");
        gate.hear(&speech());
        for _ in 0..frames_for(1_500) - 1 {
            gate.hear(&quiet());
        }

        gate.hear(&speech());
        gate.hear(&quiet());

        assert!(!gate.stopped(), "the pause starts over");
    }

    #[test]
    fn a_pause_it_cannot_honour_is_refused() {
        assert!(SilenceGate::new(0).is_err());
        assert!(SilenceGate::new(MAX_TRAILING_SILENCE_MS + 1).is_err());
        assert!(SilenceGate::new(u32::MAX).is_err());
    }
}
