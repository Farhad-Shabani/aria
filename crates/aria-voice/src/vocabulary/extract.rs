use std::ops::Range;

use aria_kit::cancel::CancelToken;

use super::{SourceKind, select};

pub struct TextInput<'a> {
    pub kind: SourceKind,
    pub text: &'a str,
}

pub trait Extractor: Send + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn cache_key(&self) -> &str;
    fn extract(
        &mut self,
        input: TextInput<'_>,
        limit: usize,
        cancel: &CancelToken,
    ) -> Result<Vec<Range<usize>>, Self::Error>;
}

#[derive(Default)]
pub struct LocalExtractor;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("vocabulary extraction was cancelled")]
    Cancelled,
}

impl Extractor for LocalExtractor {
    type Error = Error;

    fn cache_key(&self) -> &str {
        "local-names-1"
    }

    fn extract(
        &mut self,
        input: TextInput<'_>,
        limit: usize,
        cancel: &CancelToken,
    ) -> Result<Vec<Range<usize>>, Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(if limit > 0 && select::eligible(input.text) {
            std::iter::once(0..input.text.len()).collect()
        } else {
            Vec::new()
        })
    }
}
