//! A model on the local provider. It answers from what it knows and the
//! asks of its session. It has no tools, so it never reads the project.

use std::time::{Duration, Instant};

use aria_primitives::cancel::CancelToken;
use aria_provider::{Client, ProviderError};
use aria_settings::LocalProvider;
use serde_json::{Value, json};

use super::prompt_run::{SCHEMA, answer_text};
use super::{
    Assistant, AssistantError, AssistantModel, AssistantStatus, PromptRun, Session, SessionSpec,
    StepSink,
};

/// How long a readiness check or a model list may take.
const LIST_BUDGET: Duration = Duration::from_secs(3);
/// Far more than any reply. Counts the event stream's bytes, not just the reply.
const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;
const NO_ENDPOINT: &str = "set provider.endpoint in the settings file";
const NO_MODEL: &str =
    "add local-model = \"MODEL_ID\" under [assistant.models] in the settings file";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub request: String,
    pub reply: String,
}

pub trait SessionHistory: Send + Sync {
    /// The earlier asks of the session `resume` names, oldest first.
    fn recall(&self, resume: &str) -> Vec<Ask>;
}

pub struct LocalAssistant<H> {
    client: Option<Client>,
    model: Option<String>,
    history: H,
}

impl<H: SessionHistory> LocalAssistant<H> {
    /// `model` is the one `[assistant.models] local-model` names.
    pub fn new(provider: Option<&LocalProvider>, model: Option<String>, history: H) -> Self {
        Self {
            client: provider.map(|provider| Client::new(provider.endpoint(), provider.listing())),
            model,
            history,
        }
    }

    fn client(&self) -> Result<&Client, AssistantError> {
        self.client
            .as_ref()
            .ok_or(AssistantError::NotConfigured { fix: NO_ENDPOINT })
    }
}

impl<H: SessionHistory> Assistant for LocalAssistant<H> {
    type Session = LocalSession;

    fn models(&self, _cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
        let ids = self
            .client()?
            .models(Instant::now() + LIST_BUDGET)
            .map_err(|error| provider_error(error, LIST_BUDGET))?;
        Ok(ids
            .into_iter()
            .map(|id| AssistantModel {
                name: id.clone(),
                id,
            })
            .filter(AssistantModel::is_valid)
            .collect())
    }

    fn check_ready(&self, _cancel: &CancelToken) -> AssistantStatus {
        let Some(client) = &self.client else {
            return AssistantStatus::NotConfigured { fix: NO_ENDPOINT };
        };
        let deadline = Instant::now() + LIST_BUDGET;
        let unreachable = || AssistantStatus::Unreachable {
            endpoint: client.endpoint().to_owned(),
        };
        let Some(model) = &self.model else {
            // A server that isn't running is the problem to fix first. Any
            // answer, even an error, shows that it runs.
            return match client.models(deadline) {
                Err(ProviderError::Transport | ProviderError::Timeout) => unreachable(),
                _ => AssistantStatus::ModelUnset { fix: NO_MODEL },
            };
        };
        match client.serves(model, deadline) {
            Ok(true) => AssistantStatus::Ready,
            Ok(false) => AssistantStatus::ModelMissing {
                model: model.clone(),
            },
            Err(ProviderError::Transport | ProviderError::Timeout) => unreachable(),
            Err(error) => AssistantStatus::Unchecked(AssistantError::Provider(error)),
        }
    }

    fn open(&self, spec: SessionSpec<'_>) -> LocalSession {
        LocalSession {
            client: self.client.clone(),
            model: spec
                .model_id
                .map(str::to_owned)
                .or_else(|| self.model.clone()),
            earlier: spec
                .resume
                .map(|resume| self.history.recall(resume))
                .unwrap_or_default(),
            resume: spec.resume.map(str::to_owned),
        }
    }

    fn run_prompt(
        &self,
        request: &PromptRun<'_>,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<String, AssistantError> {
        let model = request
            .model_id
            .or(self.model.as_deref())
            .ok_or(AssistantError::ModelUnset { fix: NO_MODEL })?;
        let prompt = format!(
            "{}\nAnswer with one JSON object that matches this schema: {SCHEMA}",
            request.prompt()
        );
        let mut body = completion(model, vec![message("user", &prompt)]);
        body["response_format"] = json!({ "type": "json_object" });
        let content = stream(self.client()?, &body, cancel, timeout)?;
        let answer = serde_json::from_str(&content)
            .map_err(|_| AssistantError::Malformed("the answer is not JSON".to_owned()))?;
        answer_text(answer)
    }
}

/// One assistant session with the local model. The model keeps nothing
/// between requests, so each one carries the session's recent asks.
pub struct LocalSession {
    client: Option<Client>,
    model: Option<String>,
    earlier: Vec<Ask>,
    /// `None` until the first reply of a new session.
    resume: Option<String>,
}

impl Session for LocalSession {
    fn prompt(
        &mut self,
        text: &str,
        _steps: &mut dyn StepSink,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<String, AssistantError> {
        let client = self
            .client
            .as_ref()
            .ok_or(AssistantError::NotConfigured { fix: NO_ENDPOINT })?;
        let model = self
            .model
            .as_deref()
            .ok_or(AssistantError::ModelUnset { fix: NO_MODEL })?;
        let mut messages = Vec::with_capacity(self.earlier.len() * 2 + 1);
        for ask in &self.earlier {
            messages.push(message("user", &ask.request));
            messages.push(message("assistant", &ask.reply));
        }
        messages.push(message("user", text));
        let reply = stream(client, &completion(model, messages), cancel, timeout)?;
        if reply.trim().is_empty() {
            return Err(AssistantError::Malformed("the reply is empty".to_owned()));
        }
        if self.resume.is_none() {
            self.resume = Some(new_resume_id()?);
        }
        self.earlier.push(Ask {
            request: text.to_owned(),
            reply: reply.clone(),
        });
        Ok(reply)
    }

    fn resume_id(&self) -> Option<&str> {
        self.resume.as_deref()
    }
}

fn completion(model: &str, messages: Vec<Value>) -> Value {
    json!({ "model": model, "messages": messages, "stream": true })
}

fn message(role: &str, content: &str) -> Value {
    json!({ "role": role, "content": content })
}

fn stream(
    client: &Client,
    body: &Value,
    cancel: &CancelToken,
    timeout: Duration,
) -> Result<String, AssistantError> {
    client
        .stream(body, MAX_STREAM_BYTES, Instant::now() + timeout, cancel)
        .map_err(|error| provider_error(error, timeout))
}

fn provider_error(error: ProviderError, budget: Duration) -> AssistantError {
    match error {
        ProviderError::Cancelled => AssistantError::Cancelled,
        ProviderError::Timeout => AssistantError::Timeout(budget),
        error => AssistantError::Provider(error),
    }
}

/// A session's resume ID: random, so it names nothing about what was said.
fn new_resume_id() -> Result<String, AssistantError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| AssistantError::Failed(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::mpsc;

    use aria_settings::PromptKind;

    use super::*;
    use crate::chat::Step;

    /// Answers each connection with `respond(path, body)`, and sends every
    /// request body it read to the returned receiver.
    fn serve(
        respond: impl Fn(&str, &Value) -> String + Send + 'static,
    ) -> (LocalProvider, mpsc::Receiver<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, received) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let path = request_line.split_whitespace().nth(1).unwrap().to_owned();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let reply = respond(&path, &body);
                let _ = sent.send(body);
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        let provider =
            LocalProvider::new(&format!("http://{address}/v1/chat/completions")).unwrap();
        (provider, received)
    }

    fn json_reply(body: &Value) -> String {
        let body = body.to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn streamed(content: &str) -> String {
        let chunk = json!({
            "choices": [{ "delta": { "role": "assistant", "content": content }, "finish_reason": "stop" }]
        });
        let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn listing(ids: &[&str]) -> String {
        let data: Vec<Value> = ids.iter().map(|id| json!({ "id": id })).collect();
        json_reply(&json!({ "object": "list", "data": data }))
    }

    struct IgnoredSteps;

    impl StepSink for IgnoredSteps {
        fn step(&mut self, _step: Step) {}
    }

    struct ScriptedHistory {
        resume: &'static str,
        asks: Vec<Ask>,
    }

    impl ScriptedHistory {
        fn empty() -> Self {
            Self {
                resume: "",
                asks: Vec::new(),
            }
        }
    }

    impl SessionHistory for ScriptedHistory {
        fn recall(&self, resume: &str) -> Vec<Ask> {
            if resume == self.resume {
                self.asks.clone()
            } else {
                Vec::new()
            }
        }
    }

    fn ask(session: &mut LocalSession, text: &str) -> Result<String, AssistantError> {
        session.prompt(
            text,
            &mut IgnoredSteps,
            &CancelToken::never(),
            Duration::from_secs(5),
        )
    }

    fn spec<'a>(resume: Option<&'a str>) -> SessionSpec<'a> {
        SessionSpec {
            workdir: Path::new("/"),
            model_id: None,
            resume,
        }
    }

    #[test]
    fn a_resumed_session_sends_its_earlier_asks_before_the_request() {
        let (provider, requests) = serve(|_path, _body| streamed("Tuesday."));
        let history = ScriptedHistory {
            resume: "c1",
            asks: vec![Ask {
                request: "When is the review?".to_owned(),
                reply: "Monday.".to_owned(),
            }],
        };
        let local = LocalAssistant::new(Some(&provider), Some("qwen".to_owned()), history);

        let mut session = local.open(spec(Some("c1")));
        assert_eq!(ask(&mut session, "And the launch?").unwrap(), "Tuesday.");

        let request = requests.recv().unwrap();
        assert_eq!(request["model"], "qwen");
        assert_eq!(request["stream"], true);
        assert!(request.get("tools").is_none());
        assert_eq!(
            request["messages"],
            json!([
                { "role": "user", "content": "When is the review?" },
                { "role": "assistant", "content": "Monday." },
                { "role": "user", "content": "And the launch?" }
            ])
        );
        assert_eq!(session.resume_id(), Some("c1"));
    }

    #[test]
    fn a_new_session_gets_a_resume_id_only_once_it_replies() {
        let (provider, _requests) = serve(|_path, _body| streamed("Hi."));
        let local = LocalAssistant::new(
            Some(&provider),
            Some("qwen".to_owned()),
            ScriptedHistory::empty(),
        );

        let mut session = local.open(spec(None));
        assert_eq!(session.resume_id(), None);
        ask(&mut session, "Hello").unwrap();

        let id = session.resume_id().expect("an id after the reply");
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_prompt_run_asks_for_json_and_returns_its_text() {
        let (provider, requests) = serve(|_path, _body| streamed(r#"{"text":"Polished."}"#));
        let local = LocalAssistant::new(
            Some(&provider),
            Some("qwen".to_owned()),
            ScriptedHistory::empty(),
        );
        let run = PromptRun {
            model_id: None,
            kind: PromptKind::Rewrite,
            instructions: "Polish it.",
            transcript: "polish this",
        };

        let text = local
            .run_prompt(&run, &CancelToken::never(), Duration::from_secs(5))
            .unwrap();

        assert_eq!(text, "Polished.");
        let request = requests.recv().unwrap();
        assert_eq!(request["response_format"]["type"], "json_object");
        assert_eq!(request["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn readiness_says_what_is_missing() {
        let (provider, _requests) = serve(|path, _body| {
            assert_eq!(path, "/v1/models");
            listing(&["qwen:latest"])
        });
        let status = |provider: Option<&LocalProvider>, model: Option<&str>| {
            LocalAssistant::new(provider, model.map(str::to_owned), ScriptedHistory::empty())
                .check_ready(&CancelToken::never())
        };

        assert!(matches!(
            status(None, Some("qwen")),
            AssistantStatus::NotConfigured { fix: NO_ENDPOINT }
        ));
        assert!(matches!(
            status(Some(&provider), None),
            AssistantStatus::ModelUnset { fix: NO_MODEL }
        ));
        assert!(matches!(
            status(Some(&provider), Some("qwen")),
            AssistantStatus::Ready
        ));
        assert!(matches!(
            status(Some(&provider), Some("llama")),
            AssistantStatus::ModelMissing { model } if model == "llama"
        ));
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = closed.local_addr().unwrap();
        drop(closed);
        let dead = LocalProvider::new(&format!("http://{address}/v1/chat/completions")).unwrap();
        assert!(matches!(
            status(Some(&dead), Some("qwen")),
            AssistantStatus::Unreachable { .. }
        ));
        assert!(matches!(
            status(Some(&dead), None),
            AssistantStatus::Unreachable { .. }
        ));
    }

    #[test]
    fn a_server_that_answers_with_an_error_still_needs_a_model() {
        let (provider, _requests) = serve(|_path, _body| {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
        });
        let local = LocalAssistant::new(Some(&provider), None, ScriptedHistory::empty());

        assert!(matches!(
            local.check_ready(&CancelToken::never()),
            AssistantStatus::ModelUnset { fix: NO_MODEL }
        ));
    }

    #[test]
    fn an_ask_to_a_server_that_is_gone_fails_as_unavailable() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = closed.local_addr().unwrap();
        drop(closed);
        let dead = LocalProvider::new(&format!("http://{address}/v1/chat/completions")).unwrap();
        let local = LocalAssistant::new(
            Some(&dead),
            Some("qwen".to_owned()),
            ScriptedHistory::empty(),
        );

        let error = ask(&mut local.open(spec(None)), "Hello").unwrap_err();

        assert!(matches!(
            error,
            AssistantError::Provider(ProviderError::Transport)
        ));
    }
}
