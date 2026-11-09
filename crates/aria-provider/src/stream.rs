use std::io::{BufRead, BufReader, Read};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use aria_primitives::cancel::CancelToken;
use serde::Deserialize;

use crate::client::{agent, check_budget};
use crate::error::transport_error;
use crate::{Client, ProviderError};

/// How soon a cancel returns while the server has sent nothing.
const CANCEL_POLL: Duration = Duration::from_millis(50);

impl Client {
    /// Sends `request`, which must set `"stream": true`, and returns the
    /// text of its one assistant message. Reads the reply as server-sent
    /// events. A reply over `max_bytes` fails.
    ///
    /// Cancelling returns at once, even before the server sends anything.
    /// The request runs on its own thread, which hangs up at the next event
    /// so the server stops generating, and ends by `deadline` at the latest.
    pub fn stream(
        &self,
        request: &serde_json::Value,
        max_bytes: usize,
        deadline: Instant,
        cancel: &CancelToken,
    ) -> Result<String, ProviderError> {
        check_budget(deadline, cancel)?;
        let (done, result) = mpsc::channel();
        let (client, request, worker_cancel) = (self.clone(), request.clone(), cancel.clone());
        std::thread::Builder::new()
            .name("aria-provider-stream".to_owned())
            .spawn(move || {
                let _ =
                    done.send(client.read_stream(&request, max_bytes, deadline, &worker_cancel));
            })
            .map_err(|_| ProviderError::Transport)?;
        loop {
            match result.recv_timeout(CANCEL_POLL) {
                Ok(reply) => return reply,
                Err(RecvTimeoutError::Timeout) => check_budget(deadline, cancel)?,
                Err(RecvTimeoutError::Disconnected) => return Err(ProviderError::Transport),
            }
        }
    }

    fn read_stream(
        &self,
        request: &serde_json::Value,
        max_bytes: usize,
        deadline: Instant,
        cancel: &CancelToken,
    ) -> Result<String, ProviderError> {
        let mut response = agent(deadline)
            .post(self.endpoint())
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
            check_budget(deadline, cancel)?;
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

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
    fn cancelling_returns_before_the_server_sends_anything() {
        let cancel = CancelToken::new();
        let canceller = cancel.clone();
        let (client, worker) = serve(move |stream| {
            canceller.cancel();
            // Silent until the client hangs up or the deadline passes.
            let _ = std::io::copy(&mut &stream, &mut std::io::sink());
        });

        let started = Instant::now();
        assert_eq!(stream(&client, &cancel), Err(ProviderError::Cancelled));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "waited for the server"
        );
        drop(worker);
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
}
