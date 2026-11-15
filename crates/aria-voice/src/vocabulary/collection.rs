use std::io;

use aria_kit::cancel::{CancelToken, Deadline};

use super::{Discovery, Source};
use aria_settings::Directory;

pub trait Collector: Send + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn cache_key(&self) -> Option<String>;

    // A scan cut short by a limit or by `deadline` returns `Ok` with a
    // partial `coverage.completeness`. An error leaves the cached generation
    // in place.
    fn collect(
        &mut self,
        selected: &Directory,
        previous: &[Source],
        deadline: Deadline,
        cancel: &CancelToken,
    ) -> Result<Discovery, Error<Self::Error>>;
}

#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    #[error("vocabulary discovery requires an absolute directory")]
    InvalidDirectory,
    #[error("vocabulary discovery was cancelled")]
    Cancelled,
    #[error("the selected folder was replaced during vocabulary discovery")]
    Unstable,
    #[error("vocabulary extractor returned invalid output")]
    InvalidOutput,
    #[error("vocabulary extraction failed")]
    Extractor(#[source] E),
    #[error("vocabulary filesystem operation failed")]
    Io(#[from] io::Error),
}
