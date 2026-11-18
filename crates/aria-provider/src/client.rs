use std::io::Read;
use std::time::{Duration, Instant};

use aria_primitives::cancel::CancelToken;
use serde::Deserialize;

use crate::ProviderError;
use crate::error::transport_error;

const MAX_MODEL_LIST_BYTES: usize = 256 * 1024;

/// One server's chat-completions endpoint and model list. The caller
/// decides which addresses are allowed and checks both first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    endpoint: String,
    listing: String,
}

impl Client {
    pub fn new(endpoint: &str, listing: &str) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            listing: listing.to_owned(),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The IDs of the models the server lists. Spends no tokens.
    pub fn models(&self, deadline: Instant) -> Result<Vec<String>, ProviderError> {
        let mut response = agent(deadline)
            .get(&self.listing)
            .header("Accept", "application/json")
            .call()
            .map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(ProviderError::HttpStatus);
        }
        let body = read_bounded(
            response.body_mut().as_reader(),
            MAX_MODEL_LIST_BYTES,
            deadline,
            &CancelToken::never(),
        )?;
        let listed: ModelList =
            serde_json::from_slice(&body).map_err(|_| ProviderError::InvalidResponse)?;
        Ok(listed
            .data
            .into_iter()
            .flatten()
            .map(|entry| entry.id)
            .collect())
    }

    /// Whether the server lists `model`. Spends no tokens.
    pub fn serves(&self, model: &str, deadline: Instant) -> Result<bool, ProviderError> {
        // Ollama lists "llama3.2:latest" and serves it as "llama3.2".
        let latest = format!("{model}:latest");
        let serves = |id: &str| id == model || (!model.contains(':') && id == latest);
        Ok(self.models(deadline)?.iter().any(|id| serves(id)))
    }

    /// Sends `request`, which must set `"stream": false`, and returns the
    /// text of its one assistant message. A reply over `max_bytes` fails.
    pub fn complete(
        &self,
        request: &serde_json::Value,
        max_bytes: usize,
        deadline: Instant,
        cancel: &CancelToken,
    ) -> Result<String, ProviderError> {
        check_budget(deadline, cancel)?;
        let mut response = agent(deadline)
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send(request.to_string())
            .map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(ProviderError::HttpStatus);
        }
        let body = read_bounded(response.body_mut().as_reader(), max_bytes, deadline, cancel)?;
        parse_reply(&body)
    }
}

/// An agent that stops at `deadline`, follows no redirect, and ignores any
/// proxy setting, so a request never leaves the address it was given.
pub(crate) fn agent(deadline: Instant) -> ureq::Agent {
    let budget = deadline.saturating_duration_since(Instant::now());
    ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .max_idle_connections(0)
        .http_status_as_error(false)
        .max_response_header_size(8 * 1024)
        .timeout_global(Some(budget.max(Duration::from_millis(1))))
        .build()
        .into()
}

/// Reads at most `limit` bytes, checking the deadline and `cancel` before
/// each chunk.
fn read_bounded(
    mut reader: impl Read,
    limit: usize,
    deadline: Instant,
    cancel: &CancelToken,
) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        check_budget(deadline, cancel)?;
        let count = reader
            .read(&mut buffer)
            .map_err(|error| transport_error(error.into()))?;
        if count == 0 {
            return Ok(body);
        }
        if body.len() + count > limit {
            return Err(ProviderError::ResponseTooLarge);
        }
        body.extend_from_slice(&buffer[..count]);
    }
}

pub(crate) fn check_budget(deadline: Instant, cancel: &CancelToken) -> Result<(), ProviderError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(ProviderError::Timeout);
    }
    Ok(())
}

fn parse_reply(body: &[u8]) -> Result<String, ProviderError> {
    let completion: Completion =
        serde_json::from_slice(body).map_err(|_| ProviderError::InvalidResponse)?;
    let [choice]: [Choice; 1] = completion
        .choices
        .try_into()
        .map_err(|_| ProviderError::InvalidResponse)?;
    let message = choice.message;
    if message.role != "assistant"
        || message.tool_calls.is_some()
        || message.function_call.is_some()
        || message.refusal.is_some()
    {
        return Err(ProviderError::InvalidResponse);
    }
    match choice.finish_reason.as_str() {
        "stop" => Ok(message.content),
        "length" => Err(ProviderError::Truncated),
        _ => Err(ProviderError::InvalidResponse),
    }
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

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    fn answer_once(response: String) -> (Client, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut length = 0;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                line.clear();
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            // The client may hang up once a reply breaks a limit.
            let _ = stream.write_all(response.as_bytes());
        });
        let client = Client::new(
            &format!("{address}/v1/chat/completions"),
            &format!("{address}/v1/models"),
        );
        (client, worker)
    }

    fn serves(listed: &str, model: &str) -> bool {
        let (client, worker) = answer_once(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{listed}",
            listed.len()
        ));
        let served = client
            .serves(model, Instant::now() + Duration::from_secs(3))
            .unwrap();
        worker.join().unwrap();
        served
    }

    fn reply(content: &str) -> serde_json::Value {
        serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": content }
            }]
        })
    }

    fn parse(value: &serde_json::Value) -> Result<String, ProviderError> {
        parse_reply(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn a_request_never_leaves_the_address_it_was_given() {
        let agent = agent(Instant::now() + Duration::from_secs(1));
        assert!(agent.config().proxy().is_none());
        assert_eq!(agent.config().max_redirects(), 0);
    }

    #[test]
    fn a_reply_is_one_finished_assistant_message_without_tools() {
        assert_eq!(parse(&reply("hi")), Ok("hi".to_owned()));
        for field in ["tool_calls", "function_call", "refusal"] {
            let mut invalid = reply("hi");
            invalid["choices"][0]["message"][field] = serde_json::json!([]);
            assert_eq!(
                parse(&invalid),
                Err(ProviderError::InvalidResponse),
                "{field}"
            );
        }
        for reason in ["tool_calls", "content_filter", ""] {
            let mut invalid = reply("hi");
            invalid["choices"][0]["finish_reason"] = reason.into();
            assert_eq!(
                parse(&invalid),
                Err(ProviderError::InvalidResponse),
                "{reason}"
            );
        }
        let mut cut = reply("h");
        cut["choices"][0]["finish_reason"] = "length".into();
        assert_eq!(parse(&cut), Err(ProviderError::Truncated));
        let mut twice = reply("hi");
        twice["choices"] = serde_json::json!([twice["choices"][0], twice["choices"][0]]);
        assert_eq!(parse(&twice), Err(ProviderError::InvalidResponse));
        assert_eq!(
            parse_reply(br#"{"choices":[]}"#),
            Err(ProviderError::InvalidResponse)
        );
    }

    #[test]
    fn a_reply_that_breaks_a_limit_fails_by_its_category() {
        for (reply, expected) in [
            (
                "HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1/secret\r\nContent-Length: 0\r\n\r\n"
                    .to_owned(),
                "http_status",
            ),
            (
                "HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n".to_owned(),
                "http_status",
            ),
            (
                format!(
                    "HTTP/1.1 200 OK\r\nX-Large: {}\r\nContent-Length: 0\r\n\r\n",
                    "a".repeat(9 * 1024)
                ),
                "header_size",
            ),
            (
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    32 * 1024 + 1,
                    "a".repeat(32 * 1024 + 1)
                ),
                "response_size",
            ),
        ] {
            let (client, worker) = answer_once(reply);
            let error = client
                .complete(
                    &serde_json::json!({ "model": "local", "stream": false }),
                    32 * 1024,
                    Instant::now() + Duration::from_secs(3),
                    &CancelToken::never(),
                )
                .unwrap_err();
            assert!(!format!("{error:?}").contains("secret"));
            assert_eq!(error.category(), expected);
            worker.join().unwrap();
        }
    }

    #[test]
    fn a_bare_model_name_is_served_by_its_latest_tag_alone() {
        assert!(serves(r#"{"data":[{"id":"llama3.2:latest"}]}"#, "llama3.2"));
        assert!(!serves(
            r#"{"data":[{"id":"llama3.2:latest"}]}"#,
            "llama3.2:1b"
        ));
    }

    #[test]
    fn a_null_model_list_serves_nothing() {
        assert!(!serves(r#"{"object":"list","data":null}"#, "qwen2.5:3b"));
    }
}
