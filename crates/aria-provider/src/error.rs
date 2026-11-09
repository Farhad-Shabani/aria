#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("model server request was cancelled")]
    Cancelled,
    #[error("model server request timed out")]
    Timeout,
    #[error("model server transport failed")]
    Transport,
    #[error("model server returned an unsuccessful HTTP status")]
    HttpStatus,
    #[error("model server response headers exceed 8 KiB")]
    HeadersTooLarge,
    #[error("model server response exceeds its size limit")]
    ResponseTooLarge,
    #[error("model server reply is not one assistant message")]
    InvalidResponse,
    #[error("model server reply stopped at its token limit")]
    Truncated,
}

impl ProviderError {
    pub fn category(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::HttpStatus => "http_status",
            Self::HeadersTooLarge => "header_size",
            Self::ResponseTooLarge => "response_size",
            Self::InvalidResponse => "invalid_response",
            Self::Truncated => "truncated",
        }
    }
}

pub(crate) fn transport_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::Timeout(_) => ProviderError::Timeout,
        ureq::Error::LargeResponseHeader(_, _) => ProviderError::HeadersTooLarge,
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => ProviderError::HttpStatus,
        _ => ProviderError::Transport,
    }
}
