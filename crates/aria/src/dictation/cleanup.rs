use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use aria_kit::transcription::{
    TranscriptionError,
    layer::{self, Layer},
};
use aria_primitives::cancel::CancelToken;
use serde::Deserialize;

use crate::settings::{CleanupConfigError, validate_cleanup_llm};

const MIN_WORDS: usize = 4;
const MAX_INPUT_BYTES: usize = 4 * 1024;
const MAX_GUIDANCE_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024;
const REQUEST_BUDGET: Duration = Duration::from_millis(750);
const DEFAULT_GUIDANCE: &str = include_str!("../../../../prompts/cleanup.md");
const OUTPUT_CONTRACT: &str = "Return exactly one JSON object with exactly one field, \"text\", containing the cleaned transcript as a nonempty string. No Markdown fences, explanation, tool calls, or other fields.";

#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error(transparent)]
    Config(#[from] CleanupConfigError),
    #[error("cleanup prompt cannot be read as UTF-8")]
    PromptRead,
    #[error("cleanup prompt exceeds 16 KiB")]
    PromptTooLarge,
    #[error("cleanup input exceeds 4 KiB")]
    InputTooLarge,
    #[error("cleanup request timed out")]
    Timeout,
    #[error("cleanup transport failed")]
    Transport,
    #[error("cleanup server returned an unsuccessful HTTP status")]
    HttpStatus,
    #[error("cleanup response headers exceed 8 KiB")]
    HeadersTooLarge,
    #[error("cleanup response exceeds 32 KiB")]
    ResponseTooLarge,
    #[error("cleanup response violates the output contract")]
    InvalidResponse,
}

impl From<CleanupError> for TranscriptionError {
    fn from(error: CleanupError) -> Self {
        Self::Processing(layer::Error::Failed(Box::new(error)))
    }
}

impl CleanupError {
    pub(crate) fn category(&self) -> &'static str {
        match self {
            Self::Config(CleanupConfigError::Endpoint) => "endpoint",
            Self::Config(CleanupConfigError::Model) => "model",
            Self::PromptRead => "prompt_read",
            Self::PromptTooLarge => "prompt_size",
            Self::InputTooLarge => "input_size",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::HttpStatus => "http_status",
            Self::HeadersTooLarge => "header_size",
            Self::ResponseTooLarge => "response_size",
            Self::InvalidResponse => "invalid_response",
        }
    }
}

pub struct LlmCleanup {
    endpoint: String,
    model: String,
    prompt: String,
    agent: ureq::Agent,
}

impl LlmCleanup {
    pub fn new(endpoint: &str, model: &str, home: &Path) -> Result<Self, CleanupError> {
        validate_cleanup_llm(endpoint, model)?;
        let prompt = build_prompt(home)?;
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .max_idle_connections(0)
            .http_status_as_error(false)
            .max_response_header_size(8 * 1024)
            .timeout_global(Some(REQUEST_BUDGET))
            .build()
            .into();
        Ok(Self {
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
            prompt,
            agent,
        })
    }

    fn rewrite(
        &self,
        text: &str,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<String, TranscriptionError> {
        let request = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": self.prompt },
                { "role": "user", "content": text }
            ],
            "stream": false,
            "temperature": 0,
            "max_tokens": 2048,
            "response_format": { "type": "json_object" }
        });
        let request = request.to_string();
        check_budget(cancel, deadline)?;
        let mut response = self
            .agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send(request)
            .map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(CleanupError::HttpStatus.into());
        }
        let mut reader = response.body_mut().as_reader();
        let mut body = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            check_budget(cancel, deadline)?;
            let count = reader
                .read(&mut buffer)
                .map_err(|error| transport_error(error.into()))?;
            if count == 0 {
                break;
            }
            if body.len() + count > MAX_RESPONSE_BYTES {
                return Err(CleanupError::ResponseTooLarge.into());
            }
            body.extend_from_slice(&buffer[..count]);
        }
        let result = parse_response(&body).map_err(Into::into);
        check_budget(cancel, deadline)?;
        result
    }
}

impl Layer for LlmCleanup {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        let deadline = Instant::now() + REQUEST_BUDGET;
        check_budget(cancel, deadline)?;
        let result = if text.len() > MAX_INPUT_BYTES {
            Err(CleanupError::InputTooLarge.into())
        } else {
            // A word count is a rough way to skip tiny commands.
            let words = text
                .split_whitespace()
                .filter(|word| word.chars().any(char::is_alphanumeric));
            if words.take(MIN_WORDS).count() < MIN_WORDS {
                return Ok(());
            }
            self.rewrite(text, cancel, deadline)
        };
        check_budget(cancel, deadline)?;
        *text = result?;
        Ok(())
    }
}

fn check_budget(cancel: &CancelToken, deadline: Instant) -> Result<(), TranscriptionError> {
    if cancel.is_cancelled() {
        return Err(TranscriptionError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(CleanupError::Timeout.into());
    }
    Ok(())
}

fn transport_error(error: ureq::Error) -> CleanupError {
    match error {
        ureq::Error::Timeout(_) => CleanupError::Timeout,
        ureq::Error::LargeResponseHeader(_, _) => CleanupError::HeadersTooLarge,
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => CleanupError::HttpStatus,
        _ => CleanupError::Transport,
    }
}

fn build_prompt(home: &Path) -> Result<String, CleanupError> {
    let path = home.join(".aria/prompts/cleanup.md");
    let mut bytes = Vec::new();
    match std::fs::File::open(path) {
        Ok(file) => {
            file.take((MAX_GUIDANCE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| CleanupError::PromptRead)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(CleanupError::PromptRead),
    }
    if bytes.len() > MAX_GUIDANCE_BYTES {
        return Err(CleanupError::PromptTooLarge);
    }
    let override_text = String::from_utf8(bytes).map_err(|_| CleanupError::PromptRead)?;
    let guidance = if override_text.trim().is_empty() {
        DEFAULT_GUIDANCE
    } else {
        &override_text
    };
    let guidance = without_html_comments(guidance);
    Ok(format!("{guidance}\n\n{OUTPUT_CONTRACT}"))
}

fn without_html_comments(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        kept.push_str(&rest[..start]);
        rest = match rest[start..].find("-->") {
            Some(end) => &rest[start + end + "-->".len()..],
            None => "",
        };
    }
    kept.push_str(rest);
    kept.trim().to_owned()
}

#[derive(Deserialize)]
struct Completion {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    finish_reason: String,
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    role: String,
    content: String,
    tool_calls: Option<serde_json::Value>,
    function_call: Option<serde_json::Value>,
    refusal: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rewritten {
    text: String,
}

fn parse_response(body: &[u8]) -> Result<String, CleanupError> {
    let completion: Completion =
        serde_json::from_slice(body).map_err(|_| CleanupError::InvalidResponse)?;
    let [choice]: [Choice; 1] = completion
        .choices
        .try_into()
        .map_err(|_| CleanupError::InvalidResponse)?;
    if choice.finish_reason != "stop"
        || choice.message.role != "assistant"
        || choice.message.tool_calls.is_some()
        || choice.message.function_call.is_some()
        || choice.message.refusal.is_some()
    {
        return Err(CleanupError::InvalidResponse);
    }
    let output: Rewritten =
        serde_json::from_str(&choice.message.content).map_err(|_| CleanupError::InvalidResponse)?;
    if output.text.trim().is_empty() || output.text.len() > MAX_OUTPUT_BYTES {
        return Err(CleanupError::InvalidResponse);
    }
    Ok(output.text)
}

#[cfg(test)]
mod tests;
