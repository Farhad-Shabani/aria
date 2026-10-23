use std::io;

use aria_kit::cancel::CancelToken;

use super::{Directory, Discovery, Source};

pub(crate) trait Collector: Send + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn cache_key(&self) -> Option<String>;

    // Partial evidence must not replace a complete cached generation.
    fn collect(
        &mut self,
        selected: &Directory,
        previous: &[Source],
        cancel: &CancelToken,
    ) -> Result<Discovery, Error<Self::Error>>;
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error<E> {
    #[error("vocabulary discovery requires an absolute directory")]
    InvalidDirectory,
    #[error("vocabulary discovery exceeded a resource limit")]
    TooLarge,
    #[error("vocabulary discovery was cancelled")]
    Cancelled,
    #[error("project files changed during vocabulary discovery")]
    Unstable,
    #[error("vocabulary extractor returned invalid output")]
    InvalidOutput,
    #[error("vocabulary extraction failed")]
    Extractor(#[source] E),
    #[error("vocabulary filesystem operation failed")]
    Io(#[from] io::Error),
}
