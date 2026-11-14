use aria_primitives::Cause;
use std::collections::VecDeque;

use crate::cancel::CancelToken;

use super::{TranscriptUpdate, TranscriptionError, check_cancelled};

mod backtrack;
pub use backtrack::Backtrack;
mod deferred;
pub use deferred::{DeferredWords, Unheld};
mod disfluencies;
pub use disfluencies::{RemoveHesitations, RemoveRepeats};
mod number_words;
mod numbers;
mod symbol_words;
mod symbols;
pub use symbols::JoinSymbols;
pub mod vocabulary;
pub use vocabulary::Vocabulary;

#[cfg(test)]
mod tests;

const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("processed transcript exceeds 64 KiB")]
    TooLarge,
    #[error("a transcription layer failed")]
    Failed(#[source] Cause),
}

pub trait Layer: Send + Sync {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError>;

    fn enabled(&self) -> bool {
        true
    }

    /// Gets ready for an `apply` that is coming soon, such as loading a
    /// model while the user still speaks. Returns at once; any work runs
    /// on its own.
    fn prepare(&self) {}
}

pub struct Layered<L, I> {
    pub(super) inner: I,
    pub(super) layer: L,
}

impl<L: Layer, I: Layer> Layered<L, I> {
    pub fn new(inner: I, layer: L) -> Self {
        Self { inner, layer }
    }
}

impl<L: Layer, I: Layer> Layer for Layered<L, I> {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        apply(&self.inner, text, cancel)?;
        apply(&self.layer, text, cancel)
    }

    fn enabled(&self) -> bool {
        self.inner.enabled() || self.layer.enabled()
    }

    fn prepare(&self) {
        self.inner.prepare();
        self.layer.prepare();
    }
}

impl Layer for () {
    fn apply(&self, _text: &mut String, _cancel: &CancelToken) -> Result<(), TranscriptionError> {
        Ok(())
    }

    fn enabled(&self) -> bool {
        false
    }
}

impl<L: Layer> Layer for Option<L> {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        if let Some(layer) = self {
            apply(layer, text, cancel)?;
        }
        Ok(())
    }

    fn enabled(&self) -> bool {
        self.as_ref().is_some_and(Layer::enabled)
    }

    fn prepare(&self) {
        if let Some(layer) = self {
            layer.prepare();
        }
    }
}

impl<L: Layer> Layer for &L {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        (**self).apply(text, cancel)
    }

    fn enabled(&self) -> bool {
        (**self).enabled()
    }

    fn prepare(&self) {
        (**self).prepare();
    }
}

impl Layer for Vocabulary {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        if let Some(corrected) = self.correct_checked(text, &|bytes| {
            check_cancelled(cancel)?;
            check_size(bytes)
        })? {
            *text = corrected;
        }
        Ok(())
    }
}

impl Vocabulary {
    /// Corrects dictated `text` in one pass. Each step claims words, and no
    /// later step rewrites a claim:
    ///
    /// 1. Vocabulary terms.
    /// 2. The spoken forms of personal rules.
    /// 3. Spoken numbers, written as digits.
    /// 4. Spoken symbols, which join their neighbors, claimed or not.
    /// 5. Personal rules' fuzzy fixes, on what nothing claimed.
    ///
    /// `before` is text already typed: a symbol `text` opens with joins its
    /// last word.
    pub fn correct_spoken(&self, text: &str, before: &str) -> Option<String> {
        let before = before.split_whitespace().next_back();
        let claim = |text, words| symbols::claim(text, numbers::claim(text, words), before);
        self.correct_with(text, claim, &|_| Ok::<_, std::convert::Infallible>(()))
            .unwrap_or_else(|never| match never {})
    }
}

fn apply<L: Layer>(
    layer: &L,
    text: &mut String,
    cancel: &CancelToken,
) -> Result<(), TranscriptionError> {
    check_cancelled(cancel)?;
    if !layer.enabled() {
        return Ok(());
    }
    check_size(text.len())?;
    layer.apply(text, cancel)?;
    check_cancelled(cancel)?;
    check_size(text.len())
}

fn check_size(bytes: usize) -> Result<(), TranscriptionError> {
    if bytes > MAX_TEXT_BYTES {
        return Err(TranscriptionError::Processing(Error::TooLarge));
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct Output {
    pub text: String,
    pub pending: VecDeque<TranscriptUpdate>,
    pub finished: bool,
    pub retained_from: Option<u64>,
}

impl Output {
    pub(super) fn push<L: Layer>(
        &mut self,
        mut update: TranscriptUpdate,
        layers: &L,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        if let TranscriptUpdate::Finalized(segment) = &mut update {
            // Matching stays within one segment. A phrase split across
            // segments would need bounded holdback.
            let mut text = segment.as_str().to_owned();
            apply(layers, &mut text, cancel)?;
            segment.rewrite(text.trim().to_owned());
            if segment.as_str().is_empty() {
                return Ok(());
            }
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            self.text.push_str(segment.as_str());
        }
        self.pending.push_back(update);
        Ok(())
    }
}
