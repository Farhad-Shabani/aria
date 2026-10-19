use std::collections::VecDeque;

use crate::cancel::CancelToken;

use super::{TranscriptUpdate, TranscriptionError, check_cancelled};

mod hesitations;
pub use hesitations::RemoveHesitations;
pub mod vocabulary;
pub use vocabulary::Vocabulary;

#[cfg(test)]
mod tests;

const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("processed transcription exceeds 64 KiB")]
    TooLarge,
    #[error("a transcription layer failed")]
    Failed(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait Layer: Send + Sync {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError>;

    fn enabled(&self) -> bool {
        true
    }
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
}

impl<L: Layer> Layer for &L {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        (**self).apply(text, cancel)
    }

    fn enabled(&self) -> bool {
        (**self).enabled()
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
            apply(layers, &mut segment.text, cancel)?;
            segment.text = segment.text.trim().to_owned();
            if segment.text.is_empty() {
                return Ok(());
            }
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            self.text.push_str(&segment.text);
        }
        self.pending.push_back(update);
        Ok(())
    }
}
