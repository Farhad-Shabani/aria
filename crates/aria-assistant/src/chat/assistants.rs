//! Assistants backed by one non-interactive CLI run per turn. The CLI keeps
//! the chat on disk. The session keeps only the id that continues it.
//!
//! Every run is limited to its [`Tools`]. Neither CLI runs hooks or starts
//! MCP servers, the user's or the checkout's: a turn may land in a repo
//! nobody has vetted, and an MCP server runs outside the sandbox. Codex
//! keeps the rest of the user's config, so its provider, endpoints and
//! credential store still apply.
//!
//! Instructions still reach a chat turn: skills, Claude's CLAUDE.md files,
//! and Codex's AGENTS.md from the user's Codex home and the checkout. A
//! Codex prompt run keeps the AGENTS.md files out.

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
    StepSink,
};

pub const CLAUDE_BINARY: &str = "claude";
pub const CODEX_BINARY: &str = "codex";

// What a CLI needs to find its config, sign in, and reach its API. Anything
// else in the daemon's environment, such as an unrelated token, stays out.
// Claude finds its Keychain login through `USER`.
const SHARED_VARIABLES: &[&str] = &["PATH", "HOME", "USER", "TMPDIR"];
const CLAUDE_PREFIXES: &[&str] = &["ANTHROPIC_", "CLAUDE_", "CLOUD_ML_", "AWS_", "GOOGLE_"];
const CODEX_PREFIXES: &[&str] = &["OPENAI_", "CODEX_"];
const NESTING_VARIABLES: &[&str] = &["CLAUDECODE", "CLAUDE_CODE"];

const MCP_LIST_TIMEOUT: Duration = Duration::from_secs(10);

// Both status commands answer in well under a second.
const READY_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

// Codex has no single switch for "no tools", so these are deny-lists: a
// tool a Codex upgrade adds stays on until it is listed here.
//
// Every run turns these off. Each acts outside the sandbox or does more
// than read.
const CODEX_TOOLS_OFF: &[&str] = &[
    "features.image_generation=false",
    "features.multi_agent=false",
    // The model catalog turns the subagent tools on per model. No
    // `features` key turns them off.
    "agents.enabled=false",
    "features.goals=false",
    "features.hooks=false",
    "features.sleep_tool=false",
    "features.tool_suggest=false",
    "features.apps=false",
    "features.plugins=false",
    "features.skill_mcp_dependency_install=false",
    "features.browser_use=false",
    "features.computer_use=false",
    "features.in_app_browser=false",
    "web_search=\"disabled\"",
];

// A chat turn reads files and its AGENTS.md files with these, so only
// `Tools::Off` sets them. The code-mode host runs the shell tool, so
// without it nothing can read.
const CODEX_READING_OFF: &[&str] = &[
    "features.shell_tool=false",
    "features.unified_exec=false",
    "features.code_mode_host=false",
    "features.view_image=false",
    "features.skill_search=false",
    "project_doc_max_bytes=0",
];

/// What a CLI run may use besides the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tools {
    Off,
    /// Reading files and loading a skill.
    Reading,
}

/// `claude` with only the environment it needs.
pub(crate) fn claude_without_tools() -> Command {
    let mut command = Cli::Claude.command(Path::new(CLAUDE_BINARY));
    confine_claude(&mut command, Tools::Off);
    command
}

/// `--restricted` ignores user, project and local settings, and
/// `--strict-mcp-config` every MCP server.
fn confine_claude(command: &mut Command, tools: Tools) {
    let allowed = match tools {
        Tools::Off => "",
        Tools::Reading => "Read,Glob,Grep,Skill",
    };
    command.args(["--restricted", "--strict-mcp-config", "--tools", allowed]);
}

/// A chat turn is `claude -p --output-format stream-json`, prompt on
/// stdin. One JSON line per message as the turn runs; the last is the
/// envelope with the reply and the session id to `--resume` from.
pub struct ClaudeCodeAssistant {
    binary: PathBuf,
}

impl ClaudeCodeAssistant {
    pub fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
}

impl Assistant for ClaudeCodeAssistant {
    type Session = CliSession;

    fn models(&self, cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
        let mut command = Cli::Claude.command(&self.binary);
        command.arg("-p");
        confine_claude(&mut command, Tools::Off);
        command
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
/// `codex exec resume` continues. The thread lives in the user's own Codex
/// home, so a later turn can resume it.
pub struct CodexAssistant {
    binary: PathBuf,
    /// The user's own Codex home, whose login and config a prompt run
    /// borrows.
    user_home: PathBuf,
}

impl CodexAssistant {
    pub fn new(binary: PathBuf, user_home: PathBuf) -> Self {
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
        let exec = codex_exec(&self.binary, run.path(), Some(&home), Tools::Off, cancel)?;
        let mut command = codex_prompt_command(exec, run.path(), &schema, request.model_id);
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

/// Codex reads the user's instructions (AGENTS.md) from its home. A home
/// holding only links to the login and the config keeps them out, while
/// the config's provider, endpoints and credential store still apply.
fn codex_home_for(run: &Path, user_home: &Path) -> Result<PathBuf, AssistantError> {
    let home = run.join("codex-home");
    std::fs::create_dir(&home).map_err(AssistantError::RunDirectory)?;
    // A missing login means an API key in the environment, or no login at
    // all, which the CLI reports itself. A missing config means defaults.
    for name in ["auth.json", "config.toml"] {
        let file = user_home.join(name);
        if file.is_file() {
            std::os::unix::fs::symlink(file, home.join(name))
                .map_err(AssistantError::RunDirectory)?;
        }
    }
    Ok(home)
}

/// `codex exec`, held to `tools`, in `workdir`. Every setting goes before
/// the subcommand, so the `mcp list` that names the servers to turn off
/// reads the same config as the run. `None` keeps the daemon's
/// `CODEX_HOME`. A list that fails or can't be read stops the run.
fn codex_exec(
    binary: &Path,
    workdir: &Path,
    home: Option<&Path>,
    tools: Tools,
    cancel: &CancelToken,
) -> Result<Command, AssistantError> {
    let codex = || {
        let mut command = Cli::Codex.command(binary);
        command.current_dir(workdir);
        if let Some(home) = home {
            command.env("CODEX_HOME", home);
        }
        let reading = match tools {
            Tools::Off => CODEX_READING_OFF,
            Tools::Reading => &[],
        };
        // A resumed thread doesn't keep the sandbox it started with.
        command.args(["--config", "sandbox_mode=\"read-only\""]);
        for setting in CODEX_TOOLS_OFF.iter().chain(reading) {
            command.args(["--config", setting]);
        }
        command
    };

    let mut listing = codex();
    listing.args(["mcp", "list", "--json"]);
    let output = run_cli(&mut listing, "", |_| {}, cancel, MCP_LIST_TIMEOUT)?;
    if !output.status.success() {
        return output.reply(Err(AssistantError::Malformed(
            "the MCP server list did not finish".to_owned(),
        )));
    }
    #[derive(Deserialize)]
    struct Server {
        name: String,
    }
    let servers: Vec<Server> = serde_json::from_str(&output.stdout).map_err(|_| {
        AssistantError::Malformed("the MCP server list could not be read".to_owned())
    })?;

    let mut exec = codex();
    for server in servers {
        // A name is a TOML key, so one with a dot or a quote stays one key.
        let key = toml_edit::Key::new(server.name);
        exec.arg("--config")
            .arg(format!("mcp_servers.{}.enabled=false", key.display_repr()));
    }
    exec.args(["exec", "--ignore-rules"]);
    Ok(exec)
}

/// `structured_output` carries the answer.
fn claude_prompt_command(binary: &Path, workdir: &Path, model_id: Option<&str>) -> Command {
    let mut command = Cli::Claude.command(binary);
    command
        .args(["-p", "--output-format", "json"])
        .args(["--json-schema", prompt_run::SCHEMA])
        .arg("--no-session-persistence");
    confine_claude(&mut command, Tools::Off);
    command.current_dir(workdir);
    if let Some(model_id) = model_id {
        command.args(["--model", model_id]);
    }
    command
}

fn codex_prompt_command(
    mut exec: Command,
    workdir: &Path,
    schema: &Path,
    model_id: Option<&str>,
) -> Command {
    exec.arg("--ephemeral")
        .arg("--cd")
        .arg(workdir)
        .arg("--skip-git-repo-check");
    if let Some(model_id) = model_id {
        exec.args(["--model", model_id]);
    }
    exec.arg("--output-schema")
        .arg(schema)
        .args(["--json", "-"]);
    exec
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

/// A chat carried by one CLI run per turn. Only `resume` connects the
/// runs.
pub struct CliSession {
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

    /// A Codex turn asks Codex for its MCP servers before it runs.
    fn command(&self, cancel: &CancelToken) -> Result<Command, AssistantError> {
        let mut command = match self.cli {
            Cli::Claude => {
                let mut command = Cli::Claude.command(&self.binary);
                command.args(["-p", "--output-format", "stream-json", "--verbose"]);
                confine_claude(&mut command, Tools::Reading);
                if let Some(model_id) = &self.model_id {
                    command.args(["--model", model_id]);
                }
                if let Some(resume) = &self.resume {
                    command.args(["--resume", resume]);
                }
                command
            }
            Cli::Codex => {
                let mut command =
                    codex_exec(&self.binary, &self.workdir, None, Tools::Reading, cancel)?;
                match &self.resume {
                    // A resumed thread keeps the directory it started in;
                    // `resume` takes no `--cd`.
                    Some(resume) => command.args(["resume", resume]),
                    None => command.arg("--cd").arg(&self.workdir),
                };
                if let Some(model_id) = &self.model_id {
                    command.args(["--model", model_id]);
                }
                command.args(["--skip-git-repo-check", "--json", "-"]);
                command
            }
        };
        command.current_dir(&self.workdir);
        Ok(command)
    }
}

impl Session for CliSession {
    fn prompt(
        &mut self,
        text: &str,
        steps: &mut dyn StepSink,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<String, AssistantError> {
        let cli = self.cli;
        let mut warned = false;
        let output = run_cli(
            &mut self.command(cancel)?,
            text,
            |line: &str| match step(cli, line) {
                Ok(Some(step)) => steps.step(step),
                // One format drift is worth one line in the log, not one
                // per line of output, and never a failed chat.
                Err(Unparsed { column }) if !warned => {
                    warned = true;
                    tracing::warn!(column, assistant = ?cli, "an assistant line was not understood");
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
        Ok(reply.text)
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
    let envelope: ClaudeEnvelope = serde_json::from_str(last)
        .map_err(|_| AssistantError::Malformed("the result is not an envelope".to_owned()))?;
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
        .map_err(|_| AssistantError::Malformed("the output is not an envelope".to_owned()))?;
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
    let answer = serde_json::from_str(&message)
        .map_err(|_| AssistantError::Malformed("the assistant reply is not JSON".to_owned()))?;
    prompt_run::answer_text(answer)
}

fn codex_events(stdout: &str) -> impl Iterator<Item = Result<CodexEvent, AssistantError>> + '_ {
    stdout
        .lines()
        .enumerate()
        .filter(|(_index, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            // Neither the line nor serde's message, which can quote it: it
            // may be the reply.
            serde_json::from_str(line).map_err(|_| {
                AssistantError::Malformed(format!("line {} is not a codex event", index + 1))
            })
        })
}

/// A stream line that did not parse, known only by where it stopped: serde's
/// message can quote the line, and the line may carry the reply.
#[derive(Debug)]
struct Unparsed {
    column: usize,
}

/// What one line of the stream says the assistant moved on to, if anything.
/// A line that is not an event is an error for the caller to log.
fn step(cli: Cli, line: &str) -> Result<Option<Step>, Unparsed> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    match cli {
        Cli::Claude => claude_step(line),
        Cli::Codex => codex_step(line),
    }
    .map_err(|error| Unparsed {
        column: error.column(),
    })
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
        let fresh = argv(
            &session(Cli::Claude, None, None)
                .command(&CancelToken::never())
                .unwrap(),
        );
        assert!(!fresh.contains(&"--model".to_owned()), "{fresh:?}");
        assert!(!fresh.contains(&"--resume".to_owned()), "{fresh:?}");
        assert!(fresh.contains(&"--restricted".to_owned()), "{fresh:?}");

        let resumed = argv(
            &session(Cli::Claude, Some("s-1"), Some("opus"))
                .command(&CancelToken::never())
                .unwrap(),
        );
        assert!(resumed.ends_with(&[
            "--model".into(),
            "opus".into(),
            "--resume".into(),
            "s-1".into()
        ]));
    }

    struct IgnoredSteps;

    impl StepSink for IgnoredSteps {
        fn step(&mut self, _step: Step) {}
    }

    /// A stand-in `codex` that answers `mcp list` with `servers`. An `exec`
    /// logs its `CODEX_HOME`, what that home holds, and its arguments to
    /// `exec.log`, then replies as a completed turn.
    fn fake_codex(directory: &Path, servers: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let log = directory.join("exec.log");
        let path = directory.join("codex");
        std::fs::write(
            &path,
            format!(
                r#"#!/bin/sh
case " $* " in
  *" mcp list "*) printf '%s' '{servers}'; exit ;;
esac
printf 'home=%s\n' "$CODEX_HOME" > '{log}'
printf 'held=%s\n' "$(ls "$CODEX_HOME" 2>/dev/null | tr '\n' ' ')" >> '{log}'
printf '%s\n' "$@" >> '{log}'
cat > /dev/null
echo '{{"type":"thread.started","thread_id":"t-1"}}'
echo '{{"type":"item.completed","item":{{"id":"i0","type":"agent_message","text":"{{\"text\":\"ok\"}}"}}}}'
echo '{{"type":"turn.completed","usage":{{}}}}'
"#,
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The logged arguments, split at `exec`: the settings before it, and
    /// the subcommand with its own arguments.
    fn exec_log(directory: &Path) -> (Vec<String>, Vec<String>) {
        let log = std::fs::read_to_string(directory.join("exec.log")).unwrap_or_default();
        let mut args: Vec<String> = log.lines().skip(2).map(str::to_owned).collect();
        let exec = args
            .iter()
            .position(|arg| arg == "exec")
            .unwrap_or(args.len());
        let subcommand = args.split_off(exec);
        (args, subcommand)
    }

    fn sets(settings: &[String], setting: &str) -> bool {
        settings
            .windows(2)
            .any(|pair| pair == ["--config", setting])
    }

    #[test]
    fn a_codex_chat_turn_turns_off_each_listed_server_and_keeps_reading() {
        let directory = tempfile::tempdir().unwrap();
        let workdir = directory.path().to_str().unwrap();
        let codex = fake_codex(directory.path(), r#"[{"name":"node_repl"},{"name":"a.b"}]"#);

        for (resume, model_id, expected) in [
            (None, None, vec!["exec", "--ignore-rules", "--cd", workdir]),
            (
                Some("t-1"),
                Some("mini"),
                vec!["exec", "--ignore-rules", "resume", "t-1", "--model", "mini"],
            ),
        ] {
            CodexAssistant::new(codex.clone(), directory.path().into())
                .open(SessionSpec {
                    workdir: directory.path(),
                    model_id,
                    resume,
                })
                .prompt(
                    "hi",
                    &mut IgnoredSteps,
                    &CancelToken::never(),
                    Duration::from_secs(10),
                )
                .unwrap();

            let (settings, subcommand) = exec_log(directory.path());
            assert!(
                subcommand.starts_with(
                    &expected
                        .iter()
                        .map(|arg| arg.to_string())
                        .collect::<Vec<_>>()
                ),
                "{subcommand:?}"
            );
            assert!(
                subcommand.ends_with(&["--json".into(), "-".into()]),
                "{subcommand:?}"
            );
            for setting in [
                "sandbox_mode=\"read-only\"",
                "features.apps=false",
                "agents.enabled=false",
                "mcp_servers.node_repl.enabled=false",
                "mcp_servers.\"a.b\".enabled=false",
            ] {
                assert!(sets(&settings, setting), "{setting} in {settings:?}");
            }
            for setting in ["features.shell_tool=false", "features.code_mode_host=false"] {
                assert!(!sets(&settings, setting), "{setting} in {settings:?}");
            }
        }
    }

    #[test]
    fn a_codex_chat_turn_does_not_run_when_its_mcp_servers_cannot_be_listed() {
        let directory = tempfile::tempdir().unwrap();
        let codex = fake_codex(directory.path(), "not json");

        let chatted = CodexAssistant::new(codex, directory.path().into())
            .open(SessionSpec {
                workdir: directory.path(),
                model_id: None,
                resume: None,
            })
            .prompt(
                "hi",
                &mut IgnoredSteps,
                &CancelToken::never(),
                Duration::from_secs(10),
            );

        assert!(
            matches!(chatted, Err(AssistantError::Malformed(_))),
            "{chatted:?}"
        );
        assert!(!directory.path().join("exec.log").exists());
    }

    #[test]
    fn a_codex_prompt_run_borrows_the_login_and_config_and_turns_every_tool_off() {
        let directory = tempfile::tempdir().unwrap();
        let user_home = directory.path().join("user-codex");
        std::fs::create_dir(&user_home).unwrap();
        for name in ["auth.json", "config.toml", "AGENTS.md"] {
            std::fs::write(user_home.join(name), "").unwrap();
        }
        let codex = fake_codex(directory.path(), r#"[{"name":"node_repl"}]"#);

        let reply = CodexAssistant::new(codex, user_home).run_prompt(
            &PromptRun {
                model_id: None,
                kind: aria_settings::PromptKind::Rewrite,
                instructions: "Tidy it.",
                transcript: "hello",
            },
            &CancelToken::never(),
            Duration::from_secs(10),
        );

        assert_eq!(reply.unwrap(), "ok");
        let log = std::fs::read_to_string(directory.path().join("exec.log")).unwrap();
        assert_eq!(log.lines().nth(1), Some("held=auth.json config.toml "));
        let (settings, subcommand) = exec_log(directory.path());
        assert!(
            subcommand.starts_with(&["exec".into(), "--ignore-rules".into(), "--ephemeral".into()]),
            "{subcommand:?}"
        );
        for setting in [
            "sandbox_mode=\"read-only\"",
            "features.shell_tool=false",
            "project_doc_max_bytes=0",
            "mcp_servers.node_repl.enabled=false",
        ] {
            assert!(sets(&settings, setting), "{setting} in {settings:?}");
        }
    }

    /// Runs against the user's own Codex config, which chat turns and
    /// prompt runs both keep.
    #[test]
    #[ignore = "live: runs the installed codex CLI"]
    fn a_confined_codex_run_shows_the_model_no_tool_beyond_its_own() {
        const ALLOWED: &[&str] = &[
            "functions.exec",
            "functions.wait",
            "functions.request_user_input",
            "functions.request_user_input_async",
            "functions.send_message_to_user_async",
        ];
        fn names(tools: &[serde_json::Value], prefix: &str, into: &mut Vec<String>) {
            for tool in tools {
                let name = format!("{prefix}{}", tool["name"].as_str().unwrap_or("?"));
                match tool["tools"].as_array() {
                    Some(nested) => names(nested, &format!("{name}."), into),
                    None => into.push(name),
                }
            }
        }
        let never = CancelToken::never();
        let workdir = tempfile::tempdir().unwrap();
        for tools in [Tools::Off, Tools::Reading] {
            let exec =
                codex_exec(Path::new(CODEX_BINARY), workdir.path(), None, tools, &never).unwrap();
            let args = argv(&exec);
            let settings = &args[..args.iter().position(|arg| arg == "exec").unwrap()];
            let run = |subcommand: &[&str]| {
                let mut command = Cli::Codex.command(Path::new(CODEX_BINARY));
                command
                    .args(settings)
                    .args(subcommand)
                    .current_dir(workdir.path());
                let output =
                    run_cli(&mut command, "", |_| {}, &never, Duration::from_secs(60)).unwrap();
                assert!(output.status.success(), "{}", output.stderr);
                serde_json::from_str::<Vec<serde_json::Value>>(&output.stdout).unwrap()
            };

            let mut seen = Vec::new();
            for item in run(&["debug", "prompt-input", "hi"]) {
                if item["type"] == "additional_tools" {
                    names(item["tools"].as_array().unwrap(), "", &mut seen);
                }
            }
            seen.retain(|name| !ALLOWED.contains(&name.as_str()));
            assert!(seen.is_empty(), "{tools:?} shows {seen:?}");

            // A lazily started MCP server stays out of the prompt input,
            // so Codex's own list is what shows it is off.
            let running: Vec<_> = run(&["mcp", "list", "--json"])
                .into_iter()
                .filter(|server| server["enabled"] != false)
                .map(|server| server["name"].clone())
                .collect();
            assert!(running.is_empty(), "{tools:?} keeps {running:?}");
        }
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
    fn a_reply_in_the_wrong_shape_stays_out_of_every_parse_error() {
        let reply = "the meeting moved to Friday";
        let bare = format!("{reply:?}");
        let errors = [
            parse_claude(&bare).map(|_| ()).unwrap_err().to_string(),
            parse_claude_prompt(&bare)
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            parse_codex(&bare, None)
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            format!(
                "{:?}",
                step(
                    Cli::Claude,
                    &format!(r#"{{"type":"assistant","message":{{"content":{bare}}}}}"#),
                )
                .unwrap_err()
            ),
            format!(
                "{:?}",
                step(
                    Cli::Codex,
                    &format!(r#"{{"type":"item.started","item":{bare}}}"#),
                )
                .unwrap_err()
            ),
        ];
        for error in errors {
            assert!(!error.contains("Friday"), "{error}");
        }
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
            r#"{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"first"}}"#,
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
                    Command::new("codex"),
                    Path::new("/run"),
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
