//! Bounds the settings file checks and the features enforce.

use std::net::SocketAddr;
use std::time::Duration;

pub const MAX_DICTATION_TIMEOUT: Duration = Duration::from_secs(660);

/// The most `chat.timeout` may ask for.
pub(crate) const MAX_CHAT_TIMEOUT: Duration = Duration::from_secs(570);

pub fn is_valid_model_id(model_id: &str) -> bool {
    const MAX_MODEL_ID_BYTES: usize = 200;
    model_id.len() <= MAX_MODEL_ID_BYTES
        && !model_id.starts_with('-')
        && !model_id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// Why a `dictation.cleanup_llm` table cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CleanupConfigError {
    #[error(
        "cleanup endpoint must be HTTP with a literal loopback address, explicit nonzero port, and /v1/chat/completions path"
    )]
    Endpoint,
    #[error("cleanup model must contain 1–256 bytes without whitespace or control characters")]
    Model,
}

pub fn validate_cleanup_llm(endpoint: &str, model: &str) -> Result<(), CleanupConfigError> {
    let address = endpoint
        .strip_prefix("http://")
        .and_then(|rest| rest.strip_suffix("/v1/chat/completions"))
        .and_then(|authority| authority.parse::<SocketAddr>().ok())
        .filter(|address| address.ip().is_loopback() && address.port() != 0);
    if address.is_none() {
        return Err(CleanupConfigError::Endpoint);
    }
    if model.is_empty()
        || model.len() > 256
        || model.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(CleanupConfigError::Model);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_accepts_only_explicit_loopback_chat_endpoints() {
        for endpoint in [
            "http://127.0.0.1:8080/v1/chat/completions",
            "http://[::1]:8080/v1/chat/completions",
        ] {
            validate_cleanup_llm(endpoint, "local/model:1").unwrap();
        }
        for endpoint in [
            "http://localhost:8080/v1/chat/completions",
            "https://127.0.0.1:8080/v1/chat/completions",
            "http://192.168.0.1:8080/v1/chat/completions",
            "http://127.0.0.1/v1/chat/completions",
            "http://127.0.0.1:0/v1/chat/completions",
            "http://user@127.0.0.1:80/v1/chat/completions",
            "http://127.0.0.1:80/v1/chat/completions?key=secret",
            "http://127.0.0.1:80/v1/chat/completions#x",
            "http://127.0.0.1:80/other",
            "http://[::ffff:127.0.0.1]:80/v1/chat/completions",
        ] {
            assert!(
                matches!(
                    validate_cleanup_llm(endpoint, "local"),
                    Err(CleanupConfigError::Endpoint)
                ),
                "{endpoint}"
            );
        }
        for model in ["", " ", "model\nsecret", &"a".repeat(257)] {
            assert!(matches!(
                validate_cleanup_llm("http://127.0.0.1:80/v1/chat/completions", model),
                Err(CleanupConfigError::Model)
            ));
        }
    }
}
