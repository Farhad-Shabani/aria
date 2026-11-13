//! What dictation does to its words once the gate lets them out.

use std::time::Instant;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::check_cancelled;
use aria_kit::transcription::layer::{Backtrack, Layer, Layered, RemoveHesitations, RemoveRepeats};

use super::{LlmCleanup, LlmCleanupError, llm};
use crate::config::DictationConfig;
use crate::stt::{Transcript, TranscriptionError};
use crate::vocabulary::Vocabulary;

/// The layers the settings can add after the deterministic passes.
pub type ConfiguredLayers = Layered<Option<LlmCleanup>, ()>;

/// The cleanups one dictation runs, read as it was admitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cleanup {
    /// Off, no cleanup runs.
    pub enabled: bool,
    /// Drops fillers and doubled words.
    pub fillers: bool,
    /// Applies "scratch that" and the like, which need the whole text.
    pub backtrack: bool,
    /// Runs the local model, when one is configured.
    pub llm: bool,
}

/// The passes dictation runs on its text. Each released segment gets the
/// cheap ones. A transcript delivered whole gets every one.
/// What each press runs and its vocabulary come with the press.
#[derive(Default)]
pub struct Rewrites<L = ()> {
    /// Runs last, on the whole transcript only. A failure keeps the text
    /// the other passes made. Unused while cleanup is off.
    llm: L,
}

impl Rewrites {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Rewrites<ConfiguredLayers> {
    pub fn load(config: &DictationConfig, home: &std::path::Path) -> Result<Self, LlmCleanupError> {
        let llm = config
            .cleanup_llm
            .as_ref()
            .map(|target| LlmCleanup::new(target, home))
            .transpose()?;
        Ok(Rewrites::new().with(llm))
    }
}

impl<L: Layer> Rewrites<L> {
    pub(crate) fn with<N: Layer>(self, layer: N) -> Rewrites<Layered<N, L>> {
        Rewrites {
            llm: Layered::new(self.llm, layer),
        }
    }

    /// Loads the cleanup model while the user speaks, for a press whose
    /// transcript will reach it.
    pub(crate) fn prepare(&self, cleanup: Cleanup) {
        if self.reaches_model(cleanup) {
            self.llm.prepare();
        }
    }

    /// Calls each layer directly rather than through `Layered`, whose 64 KiB
    /// bound would stop deterministic cleanup on a long transcript. `typed`
    /// is what the press typed before `text`, which no rewrite changes.
    pub(crate) fn segment(
        &self,
        text: &mut String,
        typed: &str,
        cleanup: Cleanup,
        vocabulary: Option<&Vocabulary>,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        self.clean(text, cleanup, cancel)?;
        self.correct(text, typed, vocabulary);
        check_cancelled(cancel)
    }

    pub(crate) fn clean(
        &self,
        text: &mut String,
        cleanup: Cleanup,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        check_cancelled(cancel)?;
        if cleanup.enabled && cleanup.fillers {
            RemoveHesitations.apply(text, cancel)?;
            RemoveRepeats.apply(text, cancel)?;
        }
        Ok(())
    }

    pub(crate) fn correct(&self, text: &mut String, typed: &str, vocabulary: Option<&Vocabulary>) {
        // Spoken symbols join even when the press has no vocabulary.
        let no_terms = Vocabulary::default();
        if let Some(corrected) = vocabulary.unwrap_or(&no_terms).correct_spoken(text, typed) {
            *text = corrected;
        }
    }

    /// Whether a transcript delivered whole goes to the cleanup model.
    fn reaches_model(&self, cleanup: Cleanup) -> bool {
        cleanup.enabled && cleanup.llm && self.llm.enabled()
    }

    /// Corrects the whole transcript once the press ends.
    pub(crate) fn finish(
        &self,
        transcript: Transcript,
        cleanup: Cleanup,
        vocabulary: Option<&Vocabulary>,
        cancel: &CancelToken,
    ) -> Result<Transcript, TranscriptionError> {
        let mut text = transcript.into_text();
        if cleanup.enabled && cleanup.backtrack {
            Backtrack.apply(&mut text, cancel)?;
        }
        self.segment(&mut text, "", cleanup, vocabulary, cancel)?;
        // Cleanup can leave nothing, and then this fails as `NoSpeech`.
        let transcript = Transcript::new(text)?;
        if !self.reaches_model(cleanup) {
            return Ok(transcript);
        }
        let started = Instant::now();
        let mut text = transcript.as_str().to_owned();
        let result = self.llm.apply(&mut text, cancel);
        check_cancelled(cancel)?;
        let failure = result.as_ref().err().and_then(llm::failure_category);
        if let Some(category) = failure {
            tracing::warn!(
                category,
                elapsed_ms = started.elapsed().as_millis(),
                "dictation cleanup failed; delivering deterministic text"
            );
            return Ok(transcript);
        }
        result?;
        tracing::info!(
            changed = text != transcript.as_str(),
            elapsed_ms = started.elapsed().as_millis(),
            "dictation cleanup model applied"
        );
        Transcript::new(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(heard: &str, vocabulary: Option<&Vocabulary>) -> String {
        let mut text = heard.to_owned();
        Rewrites::new()
            .segment(
                &mut text,
                "",
                Cleanup::default(),
                vocabulary,
                &CancelToken::never(),
            )
            .unwrap();
        text
    }

    #[test]
    fn a_segment_joins_spoken_symbols_without_a_vocabulary() {
        assert_eq!(segment("call user underscore id.", None), "call user_id.");
    }

    #[test]
    fn a_segment_joins_a_hyphen_after_its_vocabulary() {
        let vocabulary = Vocabulary::new(["aria-stt"]).unwrap();
        assert_eq!(
            segment("aria dash stt hyphen test", Some(&vocabulary)),
            "aria-stt-test"
        );
    }
}
