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

/// Why `local_provider.endpoint` cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "local provider endpoint must be HTTP with a literal loopback address, explicit nonzero port, and /v1/chat/completions path"
)]
pub struct LocalProviderError;

/// A model server on this machine that speaks the OpenAI chat-completions
/// API, shared by every feature that runs a local model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProvider {
    endpoint: String,
    /// The address of the same server's model list.
    listing: String,
}

impl LocalProvider {
    pub fn new(endpoint: &str) -> Result<Self, LocalProviderError> {
        let address = endpoint
            .strip_prefix("http://")
            .and_then(|rest| rest.strip_suffix("/v1/chat/completions"))
            .and_then(|authority| authority.parse::<SocketAddr>().ok())
            .filter(|address| address.ip().is_loopback() && address.port() != 0)
            .ok_or(LocalProviderError)?;
        Ok(Self {
            endpoint: endpoint.to_owned(),
            listing: format!("http://{address}/v1/models"),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn listing(&self) -> &str {
        &self.listing
    }
}

/// What cleanup's local model runs on: the local provider and the model
/// `dictation.cleanup.llm.model` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmCleanupTarget {
    provider: LocalProvider,
    model: String,
}

impl LlmCleanupTarget {
    pub fn new(provider: LocalProvider, model: &str) -> Result<Self, InvalidModelId> {
        if model.is_empty() || !is_valid_model_id(model) {
            return Err(InvalidModelId);
        }
        Ok(Self {
            provider,
            model: model.to_owned(),
        })
    }

    pub fn provider(&self) -> &LocalProvider {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

/// A model ID that is empty or fails [`is_valid_model_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "model IDs must be 1 to 200 bytes, contain no whitespace or control characters, and not start with a dash"
)]
pub struct InvalidModelId;

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
            assert_eq!(LocalProvider::new(endpoint).unwrap().listing(), listing);
        }
    }

    #[test]
    fn the_local_provider_accepts_only_explicit_loopback_chat_endpoints() {
        for endpoint in [
            "http://127.0.0.1:8080/v1/chat/completions",
            "http://[::1]:8080/v1/chat/completions",
        ] {
            LocalProvider::new(endpoint).unwrap();
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
            assert_eq!(
                LocalProvider::new(endpoint),
                Err(LocalProviderError),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn a_cleanup_model_must_be_a_nonempty_model_id() {
        let provider = LocalProvider::new("http://127.0.0.1:80/v1/chat/completions").unwrap();
        LlmCleanupTarget::new(provider.clone(), "local/model:1").unwrap();
        for model in ["", " ", "model\nsecret", "-flag", &"a".repeat(201)] {
            assert_eq!(
                LlmCleanupTarget::new(provider.clone(), model),
                Err(InvalidModelId),
                "{model:?}"
            );
        }
    }
}
