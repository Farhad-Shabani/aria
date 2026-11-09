//! A client for a model server on this machine that speaks the OpenAI
//! chat-completions API, such as Ollama or llama-server.

use std::io::{BufRead, BufReader, Read};
use std::time::{Duration, Instant};

use aria_primitives::cancel::CancelToken;
use serde::Deserialize;

const MAX_MODEL_LIST_BYTES: usize = 256 * 1024;

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

/// One server's chat-completions endpoint and model list. The caller
/// decides which addresses are allowed, and hands both in checked.
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

    /// Sends one request with `"stream": false` and returns the text of
    /// its one assistant message. A reply over `max_bytes` fails.
    pub fn complete(
        &self,
        request: &serde_json::Value,
        max_bytes: usize,
        deadline: Instant,
        cancel: &CancelToken,
    ) -> Result<String, ProviderError> {
        check(deadline, cancel)?;
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

impl Client {
    /// Sends one request with `"stream": true` and returns the text of its
    /// one assistant message. Reads the reply as server-sent events, so
    /// `cancel` takes effect at the next event and hangs up, which stops
    /// the server generating. A reply over `max_bytes` fails.
    pub fn stream(
        &self,
        request: &serde_json::Value,
        max_bytes: usize,
        deadline: Instant,
        cancel: &CancelToken,
    ) -> Result<String, ProviderError> {
        check(deadline, cancel)?;
        let mut response = agent(deadline)
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .send(request.to_string())
            .map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(ProviderError::HttpStatus);
        }
        let mut events = BufReader::new(
            response
                .body_mut()
                .as_reader()
                .take(u64::try_from(max_bytes).unwrap_or(u64::MAX) + 1),
        );
        let mut reply = Reply::default();
        let mut line = Vec::new();
        let mut read = 0;
        loop {
            check(deadline, cancel)?;
            line.clear();
            let count = events
                .read_until(b'\n', &mut line)
                .map_err(|error| transport_error(error.into()))?;
            read += count;
            if read > max_bytes {
                return Err(ProviderError::ResponseTooLarge);
            }
            if count == 0 {
                return reply.finish();
            }
            let Some(data) = line.strip_prefix(b"data:") else {
                continue;
            };
            let data = data.trim_ascii();
            if data == b"[DONE]" {
                return reply.finish();
            }
            reply.push(data)?;
        }
    }
}

/// A streamed reply as its events arrive.
#[derive(Default)]
struct Reply {
    text: String,
    finish_reason: Option<String>,
}

impl Reply {
    fn push(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        let chunk: Chunk =
            serde_json::from_slice(data).map_err(|_| ProviderError::InvalidResponse)?;
        // A usage report comes without choices.
        if chunk.choices.is_empty() {
            return Ok(());
        }
        let [choice]: [ChunkChoice; 1] = chunk
            .choices
            .try_into()
            .map_err(|_| ProviderError::InvalidResponse)?;
        let delta = choice.delta;
        if self.finish_reason.is_some()
            || delta
                .role
                .as_deref()
                .is_some_and(|role| role != "assistant")
            || delta.tool_calls.is_some()
            || delta.function_call.is_some()
            || delta.refusal.is_some()
        {
            return Err(ProviderError::InvalidResponse);
        }
        if let Some(content) = delta.content {
            self.text.push_str(&content);
        }
        self.finish_reason = choice.finish_reason;
        Ok(())
    }

    fn finish(self) -> Result<String, ProviderError> {
        match self.finish_reason.as_deref() {
            Some("stop") => Ok(self.text),
            Some("length") => Err(ProviderError::Truncated),
            // A reply that ends without saying why was cut off.
            None => Err(ProviderError::Transport),
            Some(_) => Err(ProviderError::InvalidResponse),
        }
    }
}

/// An agent that stops at `deadline`, follows no redirect, and ignores any
/// proxy setting, so a request never leaves the address it was given.
fn agent(deadline: Instant) -> ureq::Agent {
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
        check(deadline, cancel)?;
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

fn check(deadline: Instant, cancel: &CancelToken) -> Result<(), ProviderError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(ProviderError::Timeout);
    }
    Ok(())
}

fn transport_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::Timeout(_) => ProviderError::Timeout,
        ureq::Error::LargeResponseHeader(_, _) => ProviderError::HeadersTooLarge,
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => ProviderError::HttpStatus,
        _ => ProviderError::Transport,
    }
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
struct Chunk {
    choices: Vec<ChunkChoice>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    finish_reason: Option<String>,
    delta: Delta,
}

#[derive(Deserialize)]
struct Delta {
    role: Option<String>,
    content: Option<String>,
    tool_calls: Option<serde_json::Value>,
    function_call: Option<serde_json::Value>,
    refusal: Option<serde_json::Value>,
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
    use super::*;

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

    /// Serves one connection on a loopback port with `respond`, and
    /// returns a client for it.
    fn serve(
        respond: impl FnOnce(std::net::TcpStream) + Send + 'static,
    ) -> (Client, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = std::io::Read::read(&mut stream, &mut request);
            respond(stream);
        });
        let client = Client::new(
            &format!("http://{address}/v1/chat/completions"),
            &format!("http://{address}/v1/models"),
        );
        (client, worker)
    }

    fn event(delta: &str, finish_reason: Option<&str>) -> String {
        let chunk = serde_json::json!({
            "choices": [{ "delta": { "role": "assistant", "content": delta }, "finish_reason": finish_reason }]
        });
        format!("data: {chunk}\n\n")
    }

    const EVENT_HEADERS: &str =
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

    fn stream(client: &Client, cancel: &CancelToken) -> Result<String, ProviderError> {
        let request = serde_json::json!({ "stream": true });
        client.stream(
            &request,
            64 * 1024,
            Instant::now() + Duration::from_secs(5),
            cancel,
        )
    }

    #[test]
    fn a_streamed_reply_joins_its_events_until_it_stops() {
        let (client, worker) = serve(|mut stream| {
            let body = format!(
                "{EVENT_HEADERS}: comment\n\n{}{}{}data: {{\"choices\":[]}}\n\ndata: [DONE]\n\n",
                event("Hel", None),
                event("lo", None),
                event("", Some("stop")),
            );
            std::io::Write::write_all(&mut stream, body.as_bytes()).unwrap();
        });

        assert_eq!(
            stream(&client, &CancelToken::never()),
            Ok("Hello".to_owned())
        );
        worker.join().unwrap();
    }

    #[test]
    fn a_streamed_reply_that_hits_its_token_limit_or_just_ends_fails() {
        for (tail, expected) in [
            (event("", Some("length")), ProviderError::Truncated),
            (String::new(), ProviderError::Transport),
        ] {
            let (client, worker) = serve(move |mut stream| {
                let body = format!("{EVENT_HEADERS}{}{tail}", event("Hel", None));
                std::io::Write::write_all(&mut stream, body.as_bytes()).unwrap();
                // Ends the reply without resetting a request still unread.
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                let _ = std::io::copy(&mut stream, &mut std::io::sink());
            });

            assert_eq!(stream(&client, &CancelToken::never()), Err(expected));
            worker.join().unwrap();
        }
    }

    #[test]
    fn cancelling_a_stream_hangs_up_at_the_next_event() {
        let cancel = CancelToken::new();
        let canceller = cancel.clone();
        let (client, worker) = serve(move |mut stream| {
            let first = format!("{EVENT_HEADERS}{}", event("Hel", None));
            std::io::Write::write_all(&mut stream, first.as_bytes()).unwrap();
            canceller.cancel();
            // The client hangs up, so writing soon fails instead of
            // running on.
            let started = Instant::now();
            while std::io::Write::write_all(&mut stream, event("lo", None).as_bytes()).is_ok() {
                assert!(started.elapsed() < Duration::from_secs(5), "never hung up");
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        assert_eq!(stream(&client, &cancel), Err(ProviderError::Cancelled));
        worker.join().unwrap();
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
}
