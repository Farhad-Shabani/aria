//! What dictation does to its words once the gate lets them out.

use std::time::Instant;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::check_cancelled;
use aria_kit::transcription::layer::{
    self, Backtrack, Layer, Layered, RemoveHesitations, RemoveRepeats,
};

use crate::transcription::{Transcript, TranscriptionError};
use crate::vocabulary::Vocabulary;

use super::{CleanupError, LlmCleanup};

/// The layers configuration can add after the deterministic passes.
pub(crate) type ConfiguredLayers = Layered<Option<LlmCleanup>, ()>;

/// The passes dictation runs on its text. Each released segment gets the
/// cheap ones. A transcript delivered whole gets every one.
/// The vocabulary comes with each press, since it follows the project.
#[derive(Default)]
pub(crate) struct Rewrites<L = ()> {
    /// Drops fillers and doubled words.
    disfluencies: bool,
    /// Applies "scratch that" and the like, which need the whole text.
    backtrack: bool,
    /// Runs last, on the whole transcript only. A failure keeps the text
    /// the other passes made.
    llm: L,
}

impl Rewrites {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

impl<L: Layer> Rewrites<L> {
    pub(crate) fn removing_disfluencies(mut self, enabled: bool) -> Self {
        self.disfluencies = enabled;
        self
    }

    pub(crate) fn backtracking(mut self, enabled: bool) -> Self {
        self.backtrack = enabled;
        self
    }

    pub(crate) fn with<N: Layer>(self, layer: N) -> Rewrites<Layered<N, L>> {
        Rewrites {
            disfluencies: self.disfluencies,
            backtrack: self.backtrack,
            llm: Layered::new(self.llm, layer),
        }
    }

    /// Calls each layer directly rather than through `Layered`, whose 64 KiB
    /// bound would stop deterministic cleanup on a long transcript.
    pub(crate) fn segment(
        &self,
        text: &mut String,
        vocabulary: Option<&Vocabulary>,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        check_cancelled(cancel)?;
        if self.disfluencies {
            RemoveHesitations.apply(text, cancel)?;
            RemoveRepeats.apply(text, cancel)?;
        }
        if let Some(corrected) = vocabulary.and_then(|vocabulary| vocabulary.correct(text)) {
            *text = corrected;
        }
        check_cancelled(cancel)
    }

    /// Corrects the whole transcript once the press ends.
    pub(crate) fn finish(
        &self,
        transcript: Transcript,
        vocabulary: Option<&Vocabulary>,
        cancel: &CancelToken,
    ) -> Result<Transcript, TranscriptionError> {
        let mut text = transcript.into_text();
        if self.backtrack {
            Backtrack.apply(&mut text, cancel)?;
        }
        self.segment(&mut text, vocabulary, cancel)?;
        // Cleanup can leave nothing, and then this fails as `NoSpeech`.
        let transcript = Transcript::new(text)?;
        if !self.llm.enabled() {
            return Ok(transcript);
        }
        let started = Instant::now();
        let mut text = transcript.as_str().to_owned();
        let result = self.llm.apply(&mut text, cancel);
        check_cancelled(cancel)?;
        let failure = match &result {
            Err(TranscriptionError::Processing(layer::Error::Failed(error))) => error
                .downcast_ref::<CleanupError>()
                .map(CleanupError::category),
            Err(TranscriptionError::Processing(layer::Error::TooLarge)) => Some("processing_size"),
            _ => None,
        };
        if let Some(category) = failure {
            tracing::warn!(
                category,
                elapsed_ms = started.elapsed().as_millis(),
                "dictation cleanup failed; delivering deterministic text"
            );
            return Ok(transcript);
        }
        result?;
        Transcript::new(text)
    }
}
