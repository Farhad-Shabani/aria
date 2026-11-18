//! What dictation does to its words once the gate lets them out.

use std::sync::LazyLock;
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

    /// Drops fillers from `text`. Calls each layer directly rather than
    /// through `Layered`, whose 64 KiB bound would stop deterministic cleanup
    /// on a long transcript.
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
        self.clean(&mut text, cleanup, cancel)?;
        if let Some(corrected) = spoken(vocabulary).correct_spoken(&text, "") {
            text = corrected.text;
        }
        check_cancelled(cancel)?;
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

/// The vocabulary a press corrects with. A press with none still writes
/// numbers and joins spoken symbols.
pub(crate) fn spoken(vocabulary: Option<&Vocabulary>) -> &Vocabulary {
    static NO_TERMS: LazyLock<Vocabulary> = LazyLock::new(Vocabulary::default);
    vocabulary.unwrap_or(&NO_TERMS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation::testing::FinalLayer;

    const BACKTRACK: Cleanup = Cleanup {
        enabled: true,
        fillers: true,
        backtrack: true,
        llm: true,
    };

    fn finished(heard: &str, vocabulary: Option<&Vocabulary>) -> String {
        Rewrites::new()
            .finish(
                Transcript::new(heard).unwrap(),
                Cleanup::default(),
                vocabulary,
                &CancelToken::never(),
            )
            .unwrap()
            .into_text()
    }

    #[test]
    fn a_transcript_joins_spoken_symbols_without_a_vocabulary() {
        assert_eq!(finished("call user underscore id.", None), "call user_id.");
    }

    #[test]
    fn a_transcript_joins_a_hyphen_after_its_vocabulary() {
        let vocabulary = Vocabulary::new(["aria-stt"]).unwrap();
        assert_eq!(
            finished("aria dash stt hyphen test", Some(&vocabulary)),
            "aria-stt-test"
        );
    }
    /// Each choice comes with each press, and the gate decides for all of
    /// them, a model configured at startup included.
    #[test]
    fn each_press_follows_the_cleanup_it_was_admitted_with() {
        fn finish<L: Layer>(
            rewrites: &Rewrites<L>,
            enabled: bool,
            fillers: bool,
            backtrack: bool,
            llm: bool,
        ) -> String {
            rewrites
                .finish(
                    Transcript::new("um meet at 2 actually 3").unwrap(),
                    Cleanup {
                        enabled,
                        fillers,
                        backtrack,
                        llm,
                    },
                    None,
                    &CancelToken::never(),
                )
                .unwrap()
                .into_text()
        }
        let rules = Rewrites::new().with(());
        for (enabled, fillers, backtrack, expected) in [
            (false, true, true, "um meet at 2 actually 3"),
            (true, false, false, "um meet at 2 actually 3"),
            (true, true, false, "meet at 2 actually 3"),
            (true, false, true, "um meet at 3"),
            (true, true, true, "meet at 3"),
        ] {
            assert_eq!(
                finish(&rules, enabled, fillers, backtrack, true),
                expected,
                "enabled {enabled}, fillers {fillers}, backtrack {backtrack}"
            );
        }

        let model = Rewrites::new().with(FinalLayer(|text: &mut String, _: &CancelToken| {
            *text = "from the model".to_owned();
            Ok(())
        }));
        assert_eq!(
            finish(&model, false, true, true, true),
            "um meet at 2 actually 3"
        );
        assert_eq!(finish(&model, true, true, true, false), "meet at 3");
        assert_eq!(finish(&model, true, false, false, true), "from the model");
    }

    #[test]
    fn backtracking_is_local_to_each_completed_transcript() {
        let rewrites = Rewrites::new();
        for text in ["meet at 2", "actually 3", "scratch", "that to Sara"] {
            assert_eq!(
                rewrites
                    .finish(
                        Transcript::new(text).unwrap(),
                        BACKTRACK,
                        None,
                        &CancelToken::never()
                    )
                    .unwrap()
                    .as_str(),
                text
            );
        }
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            rewrites.finish(
                Transcript::new("meet at 2 actually 3").unwrap(),
                BACKTRACK,
                None,
                &cancel
            ),
            Err(TranscriptionError::Cancelled)
        ));
    }

    #[test]
    fn oversized_completion_keeps_text_without_contacting_the_model() {
        let home = tempfile::tempdir().unwrap();
        let cleanup = LlmCleanup::new(
            &aria_settings::LlmCleanupTarget::new(
                aria_settings::LocalProvider::new("http://127.0.0.1:1/v1/chat/completions")
                    .unwrap(),
                "local",
            )
            .unwrap(),
            home.path(),
        )
        .unwrap();
        let rewrites = Rewrites::new().with(Some(cleanup));
        for bytes in [4097, 65537] {
            let prefix = "a".repeat(bytes);
            let text = format!("{prefix} meet at 2 actually 3");
            let transcript = rewrites
                .finish(
                    Transcript::new(&text).unwrap(),
                    BACKTRACK,
                    None,
                    &CancelToken::never(),
                )
                .unwrap();
            assert_eq!(transcript.as_str(), format!("{prefix} meet at 3"));
        }
    }

    #[test]
    fn cleaning_drops_hesitations_and_repeats_only_when_on() {
        for (enabled, expected) in [(false, "send the the um report"), (true, "send the report")] {
            let transcript = Rewrites::new()
                .finish(
                    Transcript::new("send the the um report").unwrap(),
                    Cleanup {
                        enabled,
                        fillers: true,
                        backtrack: false,
                        llm: true,
                    },
                    None,
                    &CancelToken::never(),
                )
                .unwrap();

            assert_eq!(transcript.as_str(), expected);
        }
    }

    #[test]
    fn a_whole_transcript_of_only_fillers_is_no_speech() {
        let result = Rewrites::new().finish(
            Transcript::new("um").unwrap(),
            Cleanup {
                enabled: true,
                fillers: true,
                backtrack: false,
                llm: true,
            },
            None,
            &CancelToken::never(),
        );

        assert!(matches!(result, Err(TranscriptionError::NoSpeech)));
    }
}
