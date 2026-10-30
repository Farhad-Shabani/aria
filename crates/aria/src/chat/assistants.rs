//! Assistants backed by one non-interactive CLI run per turn. The CLI keeps
//! the conversation on disk; the session keeps only the id that continues it.
//!
//! Both run read-only, and Claude ignores the checkout's own settings,
//! hooks, and MCP servers: a turn may land in a repo nobody has vetted.
//! Skills are the one thing it may take from that checkout, and a skill is
//! only text the assistant reads with the same tools it already has.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use serde::Deserialize;

use super::process::{Output, run_cli};
use super::prompt_run::{self, PromptRun};
use super::{
    Assistant, AssistantError, AssistantModel, AssistantStatus, Session, SessionSpec, Step,
    TurnSink,
};

pub(crate) const CLAUDE_BINARY: &str = "claude";
pub(crate) const CODEX_BINARY: &str = "codex";

// What a CLI needs to find its config, sign in, and reach its API. Anything
// else in the daemon's environment, such as an unrelated token, stays out.
// Claude finds its Keychain login through `USER`.
const SHARED_VARIABLES: &[&str] = &["PATH", "HOME", "USER", "TMPDIR"];
const CLAUDE_PREFIXES: &[&str] = &["ANTHROPIC_", "CLAUDE_", "CLOUD_ML_", "AWS_", "GOOGLE_"];
const CODEX_PREFIXES: &[&str] = &["OPENAI_", "CODEX_"];
const NESTING_VARIABLES: &[&str] = &["CLAUDECODE", "CLAUDE_CODE"];

// Both status commands answer in well under a second.
const READY_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

// A deny-list, so a tool a Codex upgrade adds stays on until it is listed
// here. Checked against codex-cli 0.163.0's `codex features list`.
const CODEX_PROMPT_SETTINGS: &[&str] = &[
    "features.shell_tool=false",
    "features.unified_exec=false",
    "features.view_image=false",
    "features.image_generation=false",
    "features.multi_agent=false",
    "features.goals=false",
    "features.hooks=false",
    "features.skill_search=false",
    "features.sleep_tool=false",
    "features.tool_suggest=false",
    "features.code_mode_host=false",
    "features.apps=false",
    "features.plugins=false",
    "features.browser_use=false",
    "features.computer_use=false",
    "features.in_app_browser=false",
    "project_doc_max_bytes=0",
    "web_search=\"disabled\"",
];

/// `claude` with only the environment it needs.
pub(crate) fn claude_command() -> Command {
    Cli::Claude.command(Path::new(CLAUDE_BINARY))
}

/// `claude -p --output-format stream-json`, prompt on stdin. One JSON line
/// per message as the turn runs; the last is the envelope with the reply
/// and the session id to `--resume` from. `--restricted` drops the
/// command-running tools and ignores user, project and local settings;
/// `--tools` leaves reading the repo and loading a skill.
pub(crate) struct ClaudeCodeAssistant {
    binary: PathBuf,
}

impl ClaudeCodeAssistant {
    pub(crate) fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
}

impl Assistant for ClaudeCodeAssistant {
    type Session = CliSession;

    fn models(&self, cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
        let mut command = Cli::Claude.command(&self.binary);
        command
            .args(["-p", "--restricted", "--strict-mcp-config", "--tools", ""])
            .args(["--no-session-persistence", "--input-format", "stream-json"])
            .args(["--output-format", "stream-json", "--verbose"])
            .env_remove("CLAUDE_CODE_ENTRYPOINT");
        super::models::discover(
            command,
            "{\"type\":\"control_request\",\"request_id\":\"aria-model-list\",\"request\":{\"subtype\":\"initialize\",\"hooks\":{}}}\n",
            super::models::parse_claude,
            cancel,
        )
    }

    fn check_ready(&self, cancel: &CancelToken) -> AssistantStatus {
        Cli::Claude.check_ready(&self.binary, cancel)
    }

    fn open(&self, spec: SessionSpec<'_>) -> Self::Session {
        CliSession::new(Cli::Claude, self.binary.clone(), spec)
    }

    fn run_prompt(
        &self,
        request: &PromptRun<'_>,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<String, AssistantError> {
        let run = run_directory()?;
        let mut command = claude_prompt_command(&self.binary, run.path(), request.model_id);
        let output = run_cli(&mut command, &request.prompt(), |_| {}, cancel, timeout)?;
        output.reply(parse_claude_prompt(&output.stdout))
    }
}

/// `codex exec --json`, prompt on stdin. The reply is the last
/// `agent_message` item; the thread id comes first and is what
/// `codex exec resume` continues. The sandbox is pinned on every turn: a
/// resumed thread takes whatever the user's config says, not what it
/// started with.
pub(crate) struct CodexAssistant {
    binary: PathBuf,
    /// The user's own Codex home, whose login a prompt run borrows.
    user_home: PathBuf,
}

impl CodexAssistant {
    pub(crate) fn new(binary: PathBuf, user_home: PathBuf) -> Self {
        Self { binary, user_home }
    }
}

impl Assistant for CodexAssistant {
    type Session = CliSession;

    fn models(&self, cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
        let mut command = Cli::Codex.command(&self.binary);
        command.args(["debug", "models"]);
        super::models::discover(command, "", super::models::parse_codex, cancel)
    }

    fn check_ready(&self, cancel: &CancelToken) -> AssistantStatus {
        Cli::Codex.check_ready(&self.binary, cancel)
    }

    fn open(&self, spec: SessionSpec<'_>) -> Self::Session {
        CliSession::new(Cli::Codex, self.binary.clone(), spec)
    }

    /// Codex reads its output schema from a file, so the run directory
    /// holds one, beside a Codex home of the run's own.
    fn run_prompt(
        &self,
        request: &PromptRun<'_>,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<String, AssistantError> {
        let run = run_directory()?;
        let schema = run.path().join("schema.json");
        std::fs::write(&schema, prompt_run::SCHEMA).map_err(AssistantError::RunDirectory)?;
        let home = codex_home_for(run.path(), &self.user_home)?;
        let mut command =
            codex_prompt_command(&self.binary, run.path(), &home, &schema, request.model_id);
        let output = run_cli(&mut command, &request.prompt(), |_| {}, cancel, timeout)?;
        output.reply(parse_codex_prompt(&output.stdout))
    }
}

/// An empty directory of Aria's own for one prompt run, so no checkout's
/// instructions or settings reach the assistant. Removed when dropped.
fn run_directory() -> Result<tempfile::TempDir, AssistantError> {
    tempfile::Builder::new()
        .prefix("aria-prompt-")
        .tempdir()
        .map_err(AssistantError::RunDirectory)
}

/// Codex reads the user's instructions (AGENTS.md), hooks and skills from
/// its home. A home holding only a link to the login keeps them out.
fn codex_home_for(run: &Path, user_home: &Path) -> Result<PathBuf, AssistantError> {
    let home = run.join("codex-home");
    std::fs::create_dir(&home).map_err(AssistantError::RunDirectory)?;
    let login = user_home.join("auth.json");
    // No login file means an API key in the environment, or no login at all,
    // which the CLI reports itself.
    if login.is_file() {
        std::os::unix::fs::symlink(login, home.join("auth.json"))
            .map_err(AssistantError::RunDirectory)?;
    }
    Ok(home)
}

/// `--tools ""` leaves no tool at all, and `--restricted` ignores user,
/// project and local settings. `structured_output` carries the answer.
fn claude_prompt_command(binary: &Path, workdir: &Path, model_id: Option<&str>) -> Command {
    let mut command = Cli::Claude.command(binary);
    command
        .args(["-p", "--output-format", "json"])
        .args(["--json-schema", prompt_run::SCHEMA])
        .arg("--no-session-persistence")
        .args(["--restricted", "--tools", ""])
        .arg("--strict-mcp-config")
        .current_dir(workdir);
    if let Some(model_id) = model_id {
        command.args(["--model", model_id]);
    }
    command
}

/// Codex has no single switch for "no tools". Its own home drops the
/// user's config, MCP servers, instructions and hooks; the tool features
/// left on by default are turned off one by one.
fn codex_prompt_command(
    binary: &Path,
    workdir: &Path,
    home: &Path,
    schema: &Path,
    model_id: Option<&str>,
) -> Command {
    let mut command = Cli::Codex.command(binary);
    command
        .env("CODEX_HOME", home)
        .args(["exec", "--ephemeral"])
        .arg("--cd")
        .arg(workdir)
        .args(["--skip-git-repo-check", "--sandbox", "read-only"]);
    for setting in CODEX_PROMPT_SETTINGS {
        command.args(["--config", setting]);
    }
    if let Some(model_id) = model_id {
        command.args(["--model", model_id]);
    }
    command
        .arg("--output-schema")
        .arg(schema)
        .args(["--json", "-"])
        .current_dir(workdir);
    command
}

/// Both status commands exit 1 when signed out; a signal or any other code
/// says nothing about the login.
const SIGNED_OUT_EXIT_CODE: i32 = 1;

fn status_from(checked: Result<Output, AssistantError>, fix: &'static str) -> AssistantStatus {
    match checked {
        Ok(output) if output.status.success() => AssistantStatus::Ready,
        Ok(output) if output.status.code() == Some(SIGNED_OUT_EXIT_CODE) => {
            AssistantStatus::SignedOut { fix }
        }
        Ok(output) => AssistantStatus::Unchecked(AssistantError::Failed(format!(
            "its status check ended with {}",
            output.status
        ))),
        Err(AssistantError::Missing { binary }) => AssistantStatus::NotInstalled { binary },
        Err(error) => AssistantStatus::Unchecked(error),
    }
}

#[derive(Clone, Copy, Debug)]
enum Cli {
    Claude,
    Codex,
}

impl Cli {
    fn command(self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .envs(std::env::vars_os().filter(|(name, _)| self.needs(name)));
        command
    }

    fn check_ready(self, binary: &Path, cancel: &CancelToken) -> AssistantStatus {
        let mut command = self.command(binary);
        command.args(self.status_args());
        let checked = run_cli(&mut command, "", |_| {}, cancel, READY_CHECK_TIMEOUT);
        status_from(checked, self.sign_in_fix())
    }

    fn status_args(self) -> [&'static str; 2] {
        match self {
            Self::Claude => ["auth", "status"],
            Self::Codex => ["login", "status"],
        }
    }

    fn sign_in_fix(self) -> &'static str {
        match self {
            Self::Claude => "run `claude` in Terminal, then type `/login`",
            Self::Codex => "run `codex login` in Terminal",
        }
    }

    fn needs(self, variable: &OsStr) -> bool {
        let Some(variable) = variable.to_str() else {
            return false;
        };
        let prefixes = match self {
            // Inherited from a Claude Code session, these make the CLI
            // refuse to nest.
            Self::Claude if NESTING_VARIABLES.contains(&variable) => return false,
            Self::Claude => CLAUDE_PREFIXES,
            Self::Codex => CODEX_PREFIXES,
        };
        SHARED_VARIABLES.contains(&variable)
            || prefixes.iter().any(|prefix| variable.starts_with(prefix))
    }
}

/// A conversation carried by one CLI run per turn; `resume` is all that
/// connects them.
pub(crate) struct CliSession {
    cli: Cli,
    binary: PathBuf,
    workdir: PathBuf,
    model_id: Option<String>,
    resume: Option<String>,
}

impl CliSession {
    fn new(cli: Cli, binary: PathBuf, spec: SessionSpec<'_>) -> Self {
        Self {
            cli,
            binary,
            workdir: spec.workdir.to_path_buf(),
            model_id: spec.model_id.map(str::to_owned),
            resume: spec.resume.map(str::to_owned),
        }
    }

    fn command(&self) -> Command {
        let mut command = self.cli.command(&self.binary);
        match self.cli {
            Cli::Claude => {
                command
                    .args(["-p", "--output-format", "stream-json", "--verbose"])
                    .args(["--restricted", "--tools", "Read,Glob,Grep,Skill"])
                    .arg("--strict-mcp-config");
                if let Some(model_id) = &self.model_id {
                    command.args(["--model", model_id]);
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
                if let Some(model_id) = &self.model_id {
                    command.args(["--model", model_id]);
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
    ) -> Result<(), AssistantError> {
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
                    tracing::warn!(%error, assistant = ?cli, "an assistant line was not understood");
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
pub(crate) fn parse_claude(stdout: &str) -> Result<Reply, AssistantError> {
    let last = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let envelope: ClaudeEnvelope =
        serde_json::from_str(last).map_err(|error| AssistantError::Malformed(error.to_string()))?;
    let ClaudeEnvelope::Result {
        is_error,
        result,
        session_id,
    } = envelope
    else {
        return Err(AssistantError::Malformed(
            "the stream has no result".to_owned(),
        ));
    };
    if is_error {
        // On an error the result field is the reason, not a reply.
        return Err(AssistantError::Failed(result));
    }
    Ok(Reply {
        text: result,
        session_id,
    })
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClaudePromptEnvelope {
    #[serde(rename = "result")]
    Result {
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        result: String,
        structured_output: Option<serde_json::Value>,
    },
    #[serde(other)]
    Other,
}

/// `--output-format json` writes one envelope. The answer is its
/// `structured_output`; `result` holds the same object as a string.
fn parse_claude_prompt(stdout: &str) -> Result<String, AssistantError> {
    let envelope: ClaudePromptEnvelope = serde_json::from_str(stdout.trim())
        .map_err(|error| AssistantError::Malformed(error.to_string()))?;
    let ClaudePromptEnvelope::Result {
        is_error,
        result,
        structured_output,
    } = envelope
    else {
        return Err(AssistantError::Malformed(
            "the output has no result".to_owned(),
        ));
    };
    if is_error {
        return Err(AssistantError::Failed(result));
    }
    let answer = structured_output.ok_or_else(|| {
        AssistantError::Malformed("the result has no structured output".to_owned())
    })?;
    prompt_run::answer_text(answer)
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
    #[serde(rename = "turn.completed")]
    TurnCompleted,
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

fn parse_codex(stdout: &str, resumed: Option<&str>) -> Result<Reply, AssistantError> {
    let mut thread_id = resumed.map(str::to_owned);
    let mut text = None;
    for event in codex_events(stdout) {
        match event? {
            CodexEvent::ThreadStarted { thread_id: id } => thread_id = Some(id),
            CodexEvent::ItemCompleted {
                item: CodexItem::AgentMessage { text: message },
            } => text = Some(message),
            CodexEvent::TurnFailed { error } => return Err(AssistantError::Failed(error.message)),
            CodexEvent::Error { message } => return Err(AssistantError::Failed(message)),
            CodexEvent::ItemStarted { .. }
            | CodexEvent::ItemCompleted { .. }
            | CodexEvent::TurnCompleted
            | CodexEvent::Other => {}
        }
    }
    Ok(Reply {
        text: text.ok_or_else(|| AssistantError::Malformed("no assistant reply".to_owned()))?,
        session_id: thread_id
            .ok_or_else(|| AssistantError::Malformed("no thread id".to_owned()))?,
    })
}

/// The answer is the last assistant message of a completed turn; earlier ones
/// may be the assistant narrating what it is about to do.
fn parse_codex_prompt(stdout: &str) -> Result<String, AssistantError> {
    let mut last_message = None;
    let mut completed = false;
    for event in codex_events(stdout) {
        match event? {
            CodexEvent::ItemCompleted {
                item: CodexItem::AgentMessage { text },
            } => last_message = Some(text),
            CodexEvent::TurnCompleted => completed = true,
            CodexEvent::TurnFailed { error } => return Err(AssistantError::Failed(error.message)),
            CodexEvent::Error { message } => return Err(AssistantError::Failed(message)),
            CodexEvent::ThreadStarted { .. }
            | CodexEvent::ItemStarted { .. }
            | CodexEvent::ItemCompleted { .. }
            | CodexEvent::Other => {}
        }
    }
    if !completed {
        return Err(AssistantError::Malformed(
            "the turn did not complete".to_owned(),
        ));
    }
    let message =
        last_message.ok_or_else(|| AssistantError::Malformed("no assistant reply".to_owned()))?;
    let answer = serde_json::from_str(&message).map_err(|error| {
        AssistantError::Malformed(format!("the assistant reply is not JSON: {error}"))
    })?;
    prompt_run::answer_text(answer)
}

fn codex_events(stdout: &str) -> impl Iterator<Item = Result<CodexEvent, AssistantError>> + '_ {
    stdout
        .lines()
        .enumerate()
        .filter(|(_index, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            // The line itself stays out of the message: it may be the reply.
            serde_json::from_str(line).map_err(|error| {
                AssistantError::Malformed(format!(
                    "line {} is not a codex event: {error}",
                    index + 1
                ))
            })
        })
}

/// What one line of the stream says the assistant moved on to, if anything.
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

    #[test]
    fn model_discovery_uses_each_assistants_configured_binary() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("missing-assistant");
        let cancel = CancelToken::never();
        let results = [
            ClaudeCodeAssistant::new(binary.clone()).models(&cancel),
            CodexAssistant::new(binary.clone(), directory.path().into()).models(&cancel),
        ];
        for result in results {
            assert!(
                matches!(result, Err(AssistantError::Missing { binary: path }) if path == binary)
            );
        }
    }

    #[test]
    fn the_status_command_decides_readiness() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = directory.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let claude = script("claude", r#"[ "$*" = "auth status" ]"#);
        let codex = script("codex", r#"[ "$*" = "login status" ]"#);
        let signed_out = script("signed-out", "exit 1");
        let broken = script("broken", "exit 2");
        let killed = script("killed", "kill -9 $$");
        let missing = directory.path().join("missing");
        let never = CancelToken::never();
        let cancelled = CancelToken::new();
        cancelled.cancel();

        assert!(matches!(
            Cli::Claude.check_ready(&claude, &never),
            AssistantStatus::Ready
        ));
        assert!(matches!(
            Cli::Codex.check_ready(&codex, &never),
            AssistantStatus::Ready
        ));
        assert!(matches!(
            Cli::Claude.check_ready(&signed_out, &never),
            AssistantStatus::SignedOut { fix } if fix.contains("/login")
        ));
        assert!(matches!(
            Cli::Codex.check_ready(&signed_out, &never),
            AssistantStatus::SignedOut { fix } if fix.contains("codex login")
        ));
        for unknown in [&broken, &killed] {
            assert!(matches!(
                Cli::Claude.check_ready(unknown, &never),
                AssistantStatus::Unchecked(AssistantError::Failed(_))
            ));
        }
        assert!(matches!(
            Cli::Claude.check_ready(&missing, &never),
            AssistantStatus::NotInstalled { binary } if binary == missing
        ));
        assert!(matches!(
            Cli::Claude.check_ready(&claude, &cancelled),
            AssistantStatus::Unchecked(AssistantError::Cancelled)
        ));
    }

    fn argv(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn session(cli: Cli, resume: Option<&str>, model_id: Option<&str>) -> CliSession {
        CliSession::new(
            cli,
            PathBuf::from("assistant"),
            SessionSpec {
                workdir: Path::new("/work"),
                model_id,
                resume,
            },
        )
    }

    #[test]
    fn each_cli_gets_only_what_it_needs_from_the_environment() {
        for name in ["PATH", "HOME", "ANTHROPIC_API_KEY", "CLAUDE_CONFIG_DIR"] {
            assert!(Cli::Claude.needs(OsStr::new(name)), "{name}");
        }
        for name in ["PATH", "HOME", "OPENAI_API_KEY", "CODEX_HOME"] {
            assert!(Cli::Codex.needs(OsStr::new(name)), "{name}");
        }
        for name in [
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "DYLD_INSERT_LIBRARIES",
            "OPENAI_API_KEY",
            "CLAUDECODE",
            "CLAUDE_CODE",
        ] {
            assert!(!Cli::Claude.needs(OsStr::new(name)), "{name}");
        }
        for name in ["GITHUB_TOKEN", "ANTHROPIC_API_KEY", "AWS_SECRET_ACCESS_KEY"] {
            assert!(!Cli::Codex.needs(OsStr::new(name)), "{name}");
        }
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
    fn a_claude_error_envelope_is_a_failure_with_its_reason() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in","session_id":"abc-123"}"#;

        match parse_claude(stdout).expect_err("the envelope should fail") {
            AssistantError::Failed(reason) => assert_eq!(reason, "Not logged in"),
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
            Err(AssistantError::Malformed(reason)) if reason.contains("no result")
        ));
    }

    #[test]
    fn claude_output_that_is_not_an_envelope_is_malformed() {
        assert!(matches!(
            parse_claude("Welcome to Claude"),
            Err(AssistantError::Malformed(_))
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
            AssistantError::Failed(reason) => assert_eq!(reason, "quota"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn malformed_codex_output_is_not_quoted_back() {
        let stdout = "the user's secret reply\n";

        match parse_codex(stdout, None).expect_err("prose is not an event") {
            AssistantError::Malformed(message) => {
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
            Err(AssistantError::Malformed(_))
        ));
    }

    #[test]
    fn prompt_commands_forward_only_explicit_models() {
        for model_id in [None, Some("provider/model:revision")] {
            let commands = [
                claude_prompt_command(Path::new("claude"), Path::new("/run"), model_id),
                codex_prompt_command(
                    Path::new("codex"),
                    Path::new("/run"),
                    Path::new("/home"),
                    Path::new("/schema"),
                    model_id,
                ),
            ];
            for command in commands {
                let args = argv(&command);
                let models: Vec<_> = args
                    .windows(2)
                    .filter(|pair| pair[0] == "--model")
                    .map(|pair| pair[1].as_str())
                    .collect();
                assert_eq!(models, model_id.into_iter().collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn a_claude_prompt_run_has_no_tools_session_or_settings() {
        let args = argv(&claude_prompt_command(
            Path::new("claude"),
            Path::new("/run"),
            None,
        ));

        for expected in [
            ["--output-format", "json"],
            ["--json-schema", prompt_run::SCHEMA],
            ["--tools", ""],
        ] {
            assert!(args.windows(2).any(|pair| pair == expected), "{args:?}");
        }
        for flag in [
            "--no-session-persistence",
            "--restricted",
            "--strict-mcp-config",
        ] {
            assert!(args.contains(&flag.to_owned()), "{args:?}");
        }
        assert!(!args.contains(&"--resume".to_owned()), "{args:?}");
    }

    #[test]
    fn a_codex_prompt_run_ignores_user_config_and_turns_tools_off() {
        let command = codex_prompt_command(
            Path::new("codex"),
            Path::new("/run"),
            Path::new("/run/codex-home"),
            Path::new("/run/schema.json"),
            None,
        );
        let args = argv(&command);

        assert!(
            args.starts_with(&["exec".into(), "--ephemeral".into()]),
            "{args:?}"
        );
        for expected in [
            ["--cd", "/run"],
            ["--sandbox", "read-only"],
            ["--output-schema", "/run/schema.json"],
        ] {
            assert!(args.windows(2).any(|pair| pair == expected), "{args:?}");
        }
        for setting in CODEX_PROMPT_SETTINGS {
            assert!(
                args.windows(2).any(|pair| pair == ["--config", *setting]),
                "{args:?}"
            );
        }
        assert!(
            command.get_envs().any(|(name, value)| name == "CODEX_HOME"
                && value == Some(OsStr::new("/run/codex-home"))),
            "the run's own Codex home"
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/run")));
    }

    #[test]
    fn a_claude_prompt_run_reads_the_structured_output() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"text\":\"ignored\"}","structured_output":{"text":"Send it by Friday."},"session_id":"s"}"#;

        assert_eq!(
            parse_claude_prompt(stdout).expect("text"),
            "Send it by Friday."
        );
    }

    #[test]
    fn a_claude_prompt_run_without_an_answer_fails() {
        let failed = r#"{"type":"result","is_error":true,"result":"not logged in"}"#;
        match parse_claude_prompt(failed).expect_err("is_error") {
            AssistantError::Failed(reason) => assert_eq!(reason, "not logged in"),
            other => panic!("unexpected {other:?}"),
        }

        for stdout in [
            r#"{"type":"result","is_error":false,"result":"Sure! Here it is."}"#,
            r#"{"type":"result","is_error":false,"structured_output":{"text":"ok","extra":1}}"#,
            r#"{"type":"system"}"#,
            "",
        ] {
            assert!(
                matches!(
                    parse_claude_prompt(stdout),
                    Err(AssistantError::Malformed(_))
                ),
                "{stdout}"
            );
        }
    }

    #[test]
    fn a_codex_prompt_run_takes_the_last_message_of_a_completed_turn() {
        let stdout = [
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            r#"{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"I'll clean this up."}}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"{\"text\":\"Send it by Friday.\"}"}}"#,
            r#"{"type":"turn.completed","usage":{}}"#,
        ]
        .join("\n");

        assert_eq!(
            parse_codex_prompt(&stdout).expect("text"),
            "Send it by Friday."
        );
    }

    #[test]
    fn a_codex_prompt_run_that_did_not_finish_cleanly_fails() {
        let message = r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"{\"text\":\"ok\"}"}}"#;
        let unfinished = message.to_owned();
        let failed = format!(
            "{message}\n{}",
            r#"{"type":"turn.failed","error":{"message":"quota"}}"#
        );
        let prose = format!(
            "{}\n{}",
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"Sure!"}}"#,
            r#"{"type":"turn.completed"}"#
        );

        assert!(matches!(
            parse_codex_prompt(&unfinished),
            Err(AssistantError::Malformed(_))
        ));
        assert!(matches!(
            parse_codex_prompt(&failed),
            Err(AssistantError::Failed(reason)) if reason == "quota"
        ));
        assert!(matches!(
            parse_codex_prompt(&prose),
            Err(AssistantError::Malformed(_))
        ));
    }
}
