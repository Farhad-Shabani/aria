//! Agents backed by one non-interactive CLI run per turn. The CLI keeps
//! the conversation on disk; the session keeps only the id that continues it.
//!
//! Both run read-only, and Claude ignores the checkout's own settings,
//! hooks, and MCP servers: a turn may land in a repo nobody has vetted.
//! Skills are the one thing it may take from that checkout, and a skill is
//! only text the agent reads with the same tools it already has.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use serde::Deserialize;

use super::process::run_cli;
use super::{Agent, AgentError, Session, SessionSpec, Step, TurnSink};

pub(crate) const CLAUDE_BINARY: &str = "claude";
pub(crate) const CODEX_BINARY: &str = "codex";

/// `claude -p --output-format stream-json`, prompt on stdin. One JSON line
/// per message as the turn runs; the last is the envelope with the reply
/// and the session id to `--resume` from. `--restricted` drops the
/// command-running tools and ignores user, project and local settings;
/// `--tools` leaves reading the repo and loading a skill.
pub(crate) struct ClaudeCodeAgent {
    binary: PathBuf,
}

impl ClaudeCodeAgent {
    pub(crate) fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
}

impl Agent for ClaudeCodeAgent {
    fn open(&self, spec: SessionSpec<'_>) -> Box<dyn Session> {
        Box::new(CliSession::new(Cli::Claude, self.binary.clone(), spec))
    }
}

/// `codex exec --json`, prompt on stdin. The reply is the last
/// `agent_message` item; the thread id comes first and is what
/// `codex exec resume` continues. The sandbox is pinned on every turn: a
/// resumed thread takes whatever the user's config says, not what it
/// started with.
pub(crate) struct CodexAgent {
    binary: PathBuf,
}

impl CodexAgent {
    pub(crate) fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
}

impl Agent for CodexAgent {
    fn open(&self, spec: SessionSpec<'_>) -> Box<dyn Session> {
        Box::new(CliSession::new(Cli::Codex, self.binary.clone(), spec))
    }
}

#[derive(Clone, Copy, Debug)]
enum Cli {
    Claude,
    Codex,
}

/// A conversation carried by one CLI run per turn; `resume` is all that
/// connects them.
struct CliSession {
    cli: Cli,
    binary: PathBuf,
    workdir: PathBuf,
    model: Option<String>,
    resume: Option<String>,
}

impl CliSession {
    fn new(cli: Cli, binary: PathBuf, spec: SessionSpec<'_>) -> Self {
        Self {
            cli,
            binary,
            workdir: spec.workdir.to_path_buf(),
            model: spec.model.map(str::to_owned),
            resume: spec.resume.map(str::to_owned),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        match self.cli {
            Cli::Claude => {
                command
                    .args(["-p", "--output-format", "stream-json", "--verbose"])
                    .args(["--restricted", "--tools", "Read,Glob,Grep,Skill"])
                    .arg("--strict-mcp-config")
                    // Inherited from a Claude Code session, these make the
                    // CLI refuse to nest.
                    .env_remove("CLAUDECODE")
                    .env_remove("CLAUDE_CODE");
                if let Some(model) = &self.model {
                    command.args(["--model", model]);
                }
                if let Some(resume) = &self.resume {
                    command.args(["--resume", resume]);
                }
            }
            Cli::Codex => {
                command.arg("exec");
                match &self.resume {
                    // A resumed thread keeps the directory it started in;
                    // `resume` takes no `--cd`.
                    Some(resume) => command.args(["resume", resume]),
                    None => command.arg("--cd").arg(&self.workdir),
                };
                if let Some(model) = &self.model {
                    command.args(["--model", model]);
                }
                command
                    .args(["--config", "sandbox_mode=\"read-only\""])
                    .args(["--skip-git-repo-check", "--json", "-"]);
            }
        }
        command.current_dir(&self.workdir);
        command
    }
}

impl Session for CliSession {
    fn prompt(
        &mut self,
        text: &str,
        sink: &mut dyn TurnSink,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<(), AgentError> {
        let cli = self.cli;
        let mut warned = false;
        let output = run_cli(
            &mut self.command(),
            text,
            |line: &str| match step(cli, line) {
                Ok(Some(step)) => sink.step(step),
                // One format drift is worth one line in the log, not one
                // per line of output, and never a failed ask.
                Err(error) if !warned => {
                    warned = true;
                    tracing::warn!(%error, agent = ?cli, "an agent line was not understood");
                }
                Ok(None) | Err(_) => {}
            },
            cancel,
            timeout,
        )?;
        let parsed = match self.cli {
            Cli::Claude => parse_claude(&output.stdout),
            Cli::Codex => parse_codex(&output.stdout, self.resume.as_deref()),
        };
        let reply = output.reply(parsed)?;
        self.resume = Some(reply.session_id);
        sink.text(&reply.text);
        Ok(())
    }

    fn resume_id(&self) -> Option<&str> {
        self.resume.as_deref()
    }
}

#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) text: String,
    session_id: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClaudeEnvelope {
    #[serde(rename = "result")]
    Result {
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        result: String,
        session_id: String,
    },
    #[serde(other)]
    Other,
}

/// The stream ends with the envelope; everything before it was a step. A
/// stream with no envelope died early, whatever its last line says.
pub(crate) fn parse_claude(stdout: &str) -> Result<Reply, AgentError> {
    let last = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let envelope: ClaudeEnvelope =
        serde_json::from_str(last).map_err(|error| AgentError::Malformed(error.to_string()))?;
    let ClaudeEnvelope::Result {
        is_error,
        result,
        session_id,
    } = envelope
    else {
        return Err(AgentError::Malformed("the stream has no result".to_owned()));
    };
    if is_error {
        // On an error the result field is the reason, not a reply.
        return Err(AgentError::Failed(result));
    }
    Ok(Reply {
        text: result,
        session_id,
    })
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },
    #[serde(rename = "item.started")]
    ItemStarted { item: CodexItem },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexItem },
    #[serde(rename = "turn.failed")]
    TurnFailed {
        #[serde(default)]
        error: CodexError,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        message: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum CodexItem {
    #[serde(rename = "agent_message")]
    AgentMessage { text: String },
    #[serde(rename = "reasoning")]
    Reasoning,
    #[serde(rename = "command_execution")]
    CommandExecution,
    #[serde(rename = "mcp_tool_call")]
    McpToolCall,
    #[serde(rename = "web_search")]
    WebSearch,
    #[serde(other)]
    Other,
}

#[derive(Default, Deserialize)]
struct CodexError {
    #[serde(default)]
    message: String,
}

fn parse_codex(stdout: &str, resumed: Option<&str>) -> Result<Reply, AgentError> {
    let mut thread_id = resumed.map(str::to_owned);
    let mut text = None;
    for (index, line) in stdout
        .lines()
        .enumerate()
        .filter(|(_index, line)| !line.trim().is_empty())
    {
        // The line itself stays out of the message: it may be the reply.
        let event: CodexEvent = serde_json::from_str(line).map_err(|error| {
            AgentError::Malformed(format!("line {} is not a codex event: {error}", index + 1))
        })?;
        match event {
            CodexEvent::ThreadStarted { thread_id: id } => thread_id = Some(id),
            CodexEvent::ItemCompleted {
                item: CodexItem::AgentMessage { text: message },
            } => text = Some(message),
            CodexEvent::TurnFailed { error } => return Err(AgentError::Failed(error.message)),
            CodexEvent::Error { message } => return Err(AgentError::Failed(message)),
            CodexEvent::ItemStarted { .. }
            | CodexEvent::ItemCompleted { .. }
            | CodexEvent::Other => {}
        }
    }
    Ok(Reply {
        text: text.ok_or_else(|| AgentError::Malformed("no agent message".to_owned()))?,
        session_id: thread_id.ok_or_else(|| AgentError::Malformed("no thread id".to_owned()))?,
    })
}

/// What one line of the stream says the agent moved on to, if anything.
/// A line that is not an event is an error for the caller to log.
fn step(cli: Cli, line: &str) -> Result<Option<Step>, serde_json::Error> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    match cli {
        Cli::Claude => claude_step(line),
        Cli::Codex => codex_step(line),
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClaudeLine {
    #[serde(rename = "assistant")]
    Assistant { message: ClaudeMessage },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    content: Vec<ClaudeBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClaudeBlock {
    #[serde(rename = "thinking")]
    Thinking,
    #[serde(rename = "tool_use")]
    ToolUse {
        name: String,
        #[serde(default)]
        input: ClaudeToolInput,
    },
    #[serde(other)]
    Other,
}

#[derive(Default, Deserialize)]
struct ClaudeToolInput {
    #[serde(default)]
    skill: String,
}

fn claude_step(line: &str) -> Result<Option<Step>, serde_json::Error> {
    let ClaudeLine::Assistant { message } = serde_json::from_str(line)? else {
        return Ok(None);
    };
    Ok(message.content.into_iter().find_map(|block| match block {
        ClaudeBlock::Thinking => Some(Step::Thinking),
        ClaudeBlock::ToolUse { name, input } => match name.as_str() {
            "Read" | "Glob" => Some(Step::Reading),
            "Grep" => Some(Step::Searching),
            "Skill" => Some(Step::skill(&input.skill)),
            // `--tools` bounds what can appear; a new name is not news.
            _ => None,
        },
        ClaudeBlock::Other => None,
    }))
}

fn codex_step(line: &str) -> Result<Option<Step>, serde_json::Error> {
    let CodexEvent::ItemStarted { item } = serde_json::from_str(line)? else {
        return Ok(None);
    };
    Ok(match item {
        CodexItem::Reasoning => Some(Step::Thinking),
        CodexItem::CommandExecution | CodexItem::McpToolCall => Some(Step::Running),
        CodexItem::WebSearch => Some(Step::Searching),
        CodexItem::AgentMessage { .. } | CodexItem::Other => None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn argv(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn session(cli: Cli, resume: Option<&str>, model: Option<&str>) -> CliSession {
        CliSession::new(
            cli,
            PathBuf::from("agent"),
            SessionSpec {
                workdir: Path::new("/work"),
                model,
                resume,
            },
        )
    }

    #[test]
    fn claude_gets_a_model_and_a_resume_only_when_asked() {
        let fresh = argv(&session(Cli::Claude, None, None).command());
        assert!(!fresh.contains(&"--model".to_owned()), "{fresh:?}");
        assert!(!fresh.contains(&"--resume".to_owned()), "{fresh:?}");
        assert!(fresh.contains(&"--restricted".to_owned()), "{fresh:?}");

        let resumed = argv(&session(Cli::Claude, Some("s-1"), Some("opus")).command());
        assert!(resumed.ends_with(&[
            "--model".into(),
            "opus".into(),
            "--resume".into(),
            "s-1".into()
        ]));
    }

    #[test]
    fn codex_pins_its_sandbox_on_both_fresh_and_resumed_turns() {
        let fresh = argv(&session(Cli::Codex, None, None).command());
        assert!(fresh.starts_with(&["exec".into(), "--cd".into(), "/work".into()]));
        assert!(fresh.contains(&"sandbox_mode=\"read-only\"".to_owned()));
        assert!(!fresh.contains(&"--model".to_owned()));

        let resumed = argv(&session(Cli::Codex, Some("t-1"), Some("mini")).command());
        assert!(resumed.starts_with(&[
            "exec".into(),
            "resume".into(),
            "t-1".into(),
            "--model".into(),
            "mini".into()
        ]));
        assert!(resumed.contains(&"sandbox_mode=\"read-only\"".to_owned()));
        assert!(resumed.ends_with(&["--json".into(), "-".into()]));
    }

    #[test]
    fn a_claude_envelope_yields_the_reply_and_its_session() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"result":"hi there","session_id":"abc-123","total_cost_usd":0.0}"#;

        let reply = parse_claude(stdout).expect("the envelope should parse");

        assert_eq!(reply.text, "hi there");
        assert_eq!(reply.session_id, "abc-123");
    }

    #[test]
    fn a_claude_error_envelope_is_a_failure_with_its_reason() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in","session_id":"abc-123"}"#;

        match parse_claude(stdout).expect_err("the envelope should fail") {
            AgentError::Failed(reason) => assert_eq!(reason, "Not logged in"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_claude_stream_that_ends_before_its_envelope_is_malformed() {
        let stdout = concat!(
            r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[]},"session_id":"abc-123"}"#,
            "\n",
        );

        assert!(matches!(
            parse_claude(stdout),
            Err(AgentError::Malformed(reason)) if reason.contains("no result")
        ));
    }

    #[test]
    fn claude_output_that_is_not_an_envelope_is_malformed() {
        assert!(matches!(
            parse_claude("Welcome to Claude"),
            Err(AgentError::Malformed(_))
        ));
    }

    #[test]
    fn a_claude_stream_ends_with_its_envelope() {
        let stdout = concat!(
            r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"hi","session_id":"abc-123"}"#,
            "\n",
        );

        let reply = parse_claude(stdout).expect("the stream should parse");

        assert_eq!(reply.text, "hi");
        assert_eq!(reply.session_id, "abc-123");
    }

    #[test]
    fn claude_lines_map_to_steps_by_tool() {
        let assistant =
            |block: &str| format!(r#"{{"type":"assistant","message":{{"content":[{block}]}}}}"#);
        let cases = [
            (
                assistant(r#"{"type":"thinking","thinking":"hm"}"#),
                Some(Step::Thinking),
            ),
            (
                assistant(
                    r#"{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.rs"}}"#,
                ),
                Some(Step::Reading),
            ),
            (
                assistant(r#"{"type":"tool_use","id":"t2","name":"Glob","input":{}}"#),
                Some(Step::Reading),
            ),
            (
                assistant(r#"{"type":"tool_use","id":"t3","name":"Grep","input":{}}"#),
                Some(Step::Searching),
            ),
            (
                assistant(
                    r#"{"type":"tool_use","id":"t4","name":"Skill","input":{"skill":"pair"}}"#,
                ),
                Some(Step::Skill("pair".to_owned())),
            ),
            (
                assistant(r#"{"type":"tool_use","id":"t5","name":"WebFetch","input":{}}"#),
                None,
            ),
            (assistant(r#"{"type":"text","text":"the reply"}"#), None),
            (r#"{"type":"system","subtype":"init"}"#.to_owned(), None),
            (
                r#"{"type":"result","is_error":false,"result":"x","session_id":"s"}"#.to_owned(),
                None,
            ),
            (String::new(), None),
        ];
        for (line, expected) in cases {
            assert_eq!(step(Cli::Claude, &line).expect("json"), expected, "{line}");
        }
        assert!(step(Cli::Claude, "not json").is_err());
    }

    #[test]
    fn codex_lines_map_to_steps_by_item() {
        let cases = [
            (
                r#"{"type":"item.started","item":{"id":"i1","type":"reasoning"}}"#,
                Some(Step::Thinking),
            ),
            (
                r#"{"type":"item.started","item":{"id":"i2","type":"command_execution","command":"ls"}}"#,
                Some(Step::Running),
            ),
            (
                r#"{"type":"item.started","item":{"id":"i3","type":"web_search"}}"#,
                Some(Step::Searching),
            ),
            (
                r#"{"type":"item.started","item":{"id":"i4","type":"agent_message","text":"x"}}"#,
                None,
            ),
            (
                r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution"}}"#,
                None,
            ),
            (r#"{"type":"turn.started"}"#, None),
        ];
        for (line, expected) in cases {
            assert_eq!(step(Cli::Codex, line).expect("json"), expected, "{line}");
        }
        assert!(step(Cli::Codex, "{").is_err());
    }

    #[test]
    fn codex_events_yield_the_last_message_and_the_thread() {
        let stdout = concat!(
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"i1","type":"reasoning","text":"thinking"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"hi"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#,
            "\n",
        );

        let reply = parse_codex(stdout, None).expect("the events should parse");

        assert_eq!(reply.text, "hi");
        assert_eq!(reply.session_id, "t-1");
    }

    #[test]
    fn a_resumed_codex_thread_keeps_its_id_when_no_event_names_one() {
        let stdout = r#"{"type":"item.completed","item":{"type":"agent_message","text":"again"}}"#;

        let reply = parse_codex(stdout, Some("t-9")).expect("the events should parse");

        assert_eq!(reply.session_id, "t-9");
    }

    #[test]
    fn a_failed_codex_turn_is_a_failure() {
        let stdout = concat!(
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"quota"}}"#,
        );

        match parse_codex(stdout, None).expect_err("the turn should fail") {
            AgentError::Failed(reason) => assert_eq!(reason, "quota"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn malformed_codex_output_is_not_quoted_back() {
        let stdout = "the user's secret reply\n";

        match parse_codex(stdout, None).expect_err("prose is not an event") {
            AgentError::Malformed(message) => {
                assert!(message.starts_with("line 1"), "{message}");
                assert!(!message.contains("secret"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn codex_output_without_a_message_is_malformed() {
        let stdout = r#"{"type":"thread.started","thread_id":"t-1"}"#;

        assert!(matches!(
            parse_codex(stdout, None),
            Err(AgentError::Malformed(_))
        ));
    }
}
