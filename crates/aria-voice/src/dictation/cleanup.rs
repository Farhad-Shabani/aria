use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aria_kit::transcription::{
    TranscriptionError,
    layer::{self, Layer},
};
use aria_primitives::cancel::CancelToken;
use serde::Deserialize;

use aria_settings::CleanupTarget;

const MIN_WORDS: usize = 4;
const MAX_INPUT_BYTES: usize = 4 * 1024;
const MAX_GUIDANCE_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_MODEL_LIST_BYTES: usize = 256 * 1024;
// Leaves room for a warm small model to clean a long dictation.
const REQUEST_BUDGET: Duration = Duration::from_secs(3);
// Ollama stops loading a model when its client hangs up, so the warm-up
// waits as long as a load from a cold disk can take.
const WARM_BUDGET: Duration = Duration::from_secs(30);
const DEFAULT_GUIDANCE: &str = include_str!("../../../../prompts/cleanup.md");
const WARM_INPUT: &str = "Hello.";
const OUTPUT_CONTRACT: &str = "Return exactly one JSON object with exactly one field, \"text\", containing the cleaned transcript as a nonempty string. No Markdown fences, explanation, tool calls, or other fields.";

#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
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
    #[error("cleanup response adds words or symbols the transcript doesn't have")]
    Unfaithful,
    #[error("cleanup server doesn't list the model")]
    ModelMissing,
}

impl From<CleanupError> for TranscriptionError {
    fn from(error: CleanupError) -> Self {
        Self::Processing(layer::Error::Failed(Box::new(error)))
    }
}

impl CleanupError {
    pub(crate) fn category(&self) -> &'static str {
        match self {
            Self::PromptRead => "prompt_read",
            Self::PromptTooLarge => "prompt_size",
            Self::InputTooLarge => "input_size",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::HttpStatus => "http_status",
            Self::HeadersTooLarge => "header_size",
            Self::ResponseTooLarge => "response_size",
            Self::InvalidResponse => "invalid_response",
            Self::Unfaithful => "unfaithful",
            Self::ModelMissing => "model_missing",
        }
    }
}

#[derive(Clone)]
pub struct LlmCleanup {
    target: CleanupTarget,
    prompt: String,
    agent: ureq::Agent,
    /// Set while a warm-up runs, so presses never stack them.
    warming: Arc<AtomicBool>,
}

impl LlmCleanup {
    pub fn new(target: &CleanupTarget, home: &Path) -> Result<Self, CleanupError> {
        Ok(Self {
            target: target.clone(),
            prompt: build_prompt(home)?,
            agent: agent(REQUEST_BUDGET),
            warming: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Whether the server answers and lists the model. Spends no tokens.
    pub fn check_ready(target: &CleanupTarget) -> Result<(), CleanupError> {
        let deadline = Instant::now() + REQUEST_BUDGET;
        let mut response = agent(REQUEST_BUDGET)
            .get(target.listing())
            .header("Accept", "application/json")
            .call()
            .map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(CleanupError::HttpStatus);
        }
        let body = read_bounded(
            response.body_mut().as_reader(),
            MAX_MODEL_LIST_BYTES,
            || {
                if Instant::now() >= deadline {
                    Err(CleanupError::Timeout)
                } else {
                    Ok(())
                }
            },
        )?;
        let listed: ModelList =
            serde_json::from_slice(&body).map_err(|_| CleanupError::InvalidResponse)?;
        // Ollama lists "llama3.2:latest" and serves it as "llama3.2".
        let model = target.model();
        let latest = format!("{model}:latest");
        let serves = |id: &str| id == model || (!model.contains(':') && id == latest);
        if listed.data.iter().flatten().any(|entry| serves(&entry.id)) {
            Ok(())
        } else {
            Err(CleanupError::ModelMissing)
        }
    }

    fn rewrite(
        &self,
        text: &str,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<String, TranscriptionError> {
        let mut request = self.request(text, 2048);
        request["response_format"] = serde_json::json!({ "type": "json_object" });
        let body = self.post(request.to_string(), cancel, deadline)?;
        let result = parse_response(&body)
            .and_then(|output| {
                if adds_words(text, &output) || adds_keystrokes(text, &output) {
                    Err(CleanupError::Unfaithful)
                } else {
                    Ok(output)
                }
            })
            .map_err(Into::into);
        check_budget(cancel, deadline)?;
        result
    }

    /// The warm-up shares this shape, so the server caches the same prefix
    /// the cleanup request then reuses.
    fn request(&self, text: &str, max_tokens: u32) -> serde_json::Value {
        serde_json::json!({
            "model": self.target.model(),
            "messages": [
                { "role": "system", "content": self.prompt },
                { "role": "user", "content": text }
            ],
            "stream": false,
            "temperature": 0,
            "max_tokens": max_tokens
        })
    }

    fn post(
        &self,
        request: String,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<Vec<u8>, TranscriptionError> {
        check_budget(cancel, deadline)?;
        let mut response = self
            .agent
            .post(self.target.endpoint())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send(request)
            .map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(CleanupError::HttpStatus.into());
        }
        read_bounded(response.body_mut().as_reader(), MAX_RESPONSE_BYTES, || {
            check_budget(cancel, deadline)
        })
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

    /// Asks the server for one token with the cleanup prompt, on its own
    /// thread, so the model loads and the prompt is cached while the user
    /// speaks. Its own budget bounds the thread, and only one runs at a
    /// time; nothing waits for it.
    fn prepare(&self) {
        if self.warming.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut cleanup = self.clone();
        cleanup.agent = agent(WARM_BUDGET);
        let spawned = std::thread::Builder::new()
            .name("aria-cleanup-warm".to_owned())
            .spawn(move || {
                let _running = Warming(Arc::clone(&cleanup.warming));
                let started = Instant::now();
                let request = cleanup.request(WARM_INPUT, 1);
                let result = cleanup.post(
                    request.to_string(),
                    &CancelToken::never(),
                    started + WARM_BUDGET,
                );
                let category = match &result {
                    Ok(_) => "none",
                    Err(error) => failure_category(error).unwrap_or("other"),
                };
                tracing::debug!(
                    category,
                    elapsed_ms = started.elapsed().as_millis(),
                    "cleanup model warm-up ended"
                );
            });
        if spawned.is_err() {
            self.warming.store(false, Ordering::Release);
            tracing::warn!("cleanup model warm-up could not start");
        }
    }
}

/// Clears the warm-up flag however the warm-up thread ends.
struct Warming(Arc<AtomicBool>);

impl Drop for Warming {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The log category of a failed cleanup, or `None` when the failure
/// isn't cleanup's own.
pub(super) fn failure_category(error: &TranscriptionError) -> Option<&'static str> {
    match error {
        TranscriptionError::Processing(layer::Error::Failed(error)) => error
            .downcast_ref::<CleanupError>()
            .map(CleanupError::category),
        TranscriptionError::Processing(layer::Error::TooLarge) => Some("processing_size"),
        _ => None,
    }
}

fn agent(budget: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .max_idle_connections(0)
        .http_status_as_error(false)
        .max_response_header_size(8 * 1024)
        .timeout_global(Some(budget))
        .build()
        .into()
}

/// Reads at most `limit` bytes, asking `check` before each chunk whether to
/// go on.
fn read_bounded<E: From<CleanupError>>(
    mut reader: impl Read,
    limit: usize,
    mut check: impl FnMut() -> Result<(), E>,
) -> Result<Vec<u8>, E> {
    let mut body = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        check()?;
        let count = reader
            .read(&mut buffer)
            .map_err(|error| E::from(transport_error(error.into())))?;
        if count == 0 {
            return Ok(body);
        }
        if body.len() + count > limit {
            return Err(CleanupError::ResponseTooLarge.into());
        }
        body.extend_from_slice(&buffer[..count]);
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
    let path = home
        .join(aria_settings::PROMPTS_DIR)
        .join(aria_settings::CLEANUP_PROMPT_FILE);
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

/// Whether `output` brings in what `input` doesn't have: a model that
/// answered the transcript, followed it, or translated it. Cleanup only
/// punctuates, recases and drops words, so each output word must use up a
/// matching input word, with one new word in ten tolerated. Digits pass
/// free, because spoken numbers come back as digits in any language.
fn adds_words(input: &str, output: &str) -> bool {
    let mut unused: HashMap<String, usize> = HashMap::new();
    for word in words(input) {
        *unused.entry(word).or_default() += 1;
    }
    let available: usize = unused.values().sum();
    let mut total = 0;
    let mut added = 0;
    for word in words(output) {
        total += 1;
        match unused.get_mut(&word) {
            Some(count) if *count > 0 => *count -= 1,
            _ if is_number(&word) => {}
            _ => added += 1,
        }
    }
    total > available || added * 10 > total
}

/// Whether `output` brings in a control character, such as a newline that
/// presses Enter when typed. A symbol alone types as text and runs nothing.
fn adds_keystrokes(input: &str, output: &str) -> bool {
    output.chars().any(|c| c.is_control() && !input.contains(c))
}

/// Lowercase runs of letters and digits, apostrophes dropped, so "Let's"
/// and "lets" match.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '’')
        .map(|word| {
            word.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
}

/// Digits, or an ordinal such as "5th".
fn is_number(word: &str) -> bool {
    let digits = word.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &word[digits.len()..];
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && ["", "st", "nd", "rd", "th"].contains(&suffix)
}

#[derive(Deserialize)]
struct ModelList {
    /// Ollama sends `null` while it has no models.
    data: Option<Vec<ListedModel>>,
}

#[derive(Deserialize)]
struct ListedModel {
    id: String,
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
