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

/// Why a `dictation.cleanup.llm` table cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CleanupConfigError {
    #[error(
        "cleanup endpoint must be HTTP with a literal loopback address, explicit nonzero port, and /v1/chat/completions path"
    )]
    Endpoint,
    #[error("cleanup model must contain 1–256 bytes without whitespace or control characters")]
    Model,
}

/// A `dictation.cleanup.llm` table that can be used: a loopback chat endpoint
/// and a model name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupTarget {
    endpoint: String,
    /// The address of the same server's model list.
    listing: String,
    model: String,
}

impl CleanupTarget {
    pub fn new(endpoint: &str, model: &str) -> Result<Self, CleanupConfigError> {
        let Some(address) = endpoint
            .strip_prefix("http://")
            .and_then(|rest| rest.strip_suffix("/v1/chat/completions"))
            .and_then(|authority| authority.parse::<SocketAddr>().ok())
            .filter(|address| address.ip().is_loopback() && address.port() != 0)
        else {
            return Err(CleanupConfigError::Endpoint);
        };
        if model.is_empty()
            || model.len() > 256
            || model.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(CleanupConfigError::Model);
        }
        Ok(Self {
            endpoint: endpoint.to_owned(),
            listing: format!("http://{address}/v1/models"),
            model: model.to_owned(),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn listing(&self) -> &str {
        &self.listing
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_listing_is_on_the_same_server() {
        for (endpoint, listing) in [
            (
                "http://127.0.0.1:11434/v1/chat/completions",
                "http://127.0.0.1:11434/v1/models",
            ),
            (
                "http://[::1]:8080/v1/chat/completions",
                "http://[::1]:8080/v1/models",
            ),
        ] {
            assert_eq!(
                CleanupTarget::new(endpoint, "m").unwrap().listing(),
                listing
            );
        }
    }

    #[test]
    fn configuration_accepts_only_explicit_loopback_chat_endpoints() {
        for endpoint in [
            "http://127.0.0.1:8080/v1/chat/completions",
            "http://[::1]:8080/v1/chat/completions",
        ] {
            CleanupTarget::new(endpoint, "local/model:1").unwrap();
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
                    CleanupTarget::new(endpoint, "local"),
                    Err(CleanupConfigError::Endpoint)
                ),
                "{endpoint}"
            );
        }
        for model in ["", " ", "model\nsecret", &"a".repeat(257)] {
            assert!(matches!(
                CleanupTarget::new("http://127.0.0.1:80/v1/chat/completions", model),
                Err(CleanupConfigError::Model)
            ));
        }
    }
}
