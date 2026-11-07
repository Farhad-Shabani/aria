//! Domain types for talking to coding assistants.
//!
//! The daemon owns conversations keyed by assistant and session name. Each ask
//! returns a [`Ticket`]; its reply arrives through [`ChatStage`] events.
//! A [`Step`] describes the assistant's activity while the ask is running.

mod config;
pub use config::ChatConfig;
mod assistants;
mod models;
pub use models::AssistantModel;
mod process;
mod projects;
mod prompt_run;
mod sessions;
mod subscribers;

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::process::RunError;
use serde::{Deserialize, Serialize};

use aria_platform::delivery::OutputMode;
use aria_primitives::error_chain;
use aria_settings::PromptKind;
use aria_settings::config::AssistantId;
use aria_settings::event::EventEmitter;
use aria_settings::rpc::RpcErrorCode;

pub use assistants::{CLAUDE_BINARY, CODEX_BINARY, ClaudeCodeAssistant, CodexAssistant};
pub(crate) use assistants::{claude_command, parse_claude};
pub(crate) use process::run_cli;
pub use projects::{Ambiguous, DiscoverError, Focus, Mention, Project, Projects, ProjectsError};
pub use prompt_run::{
    Instructions, InstructionsError, InstructionsTextError, PromptRun, read_instructions,
};
pub use sessions::{SessionStore, SessionStoreError};
pub use subscribers::ChatSubscribers;

pub const EVENT_THREAD: &str = "aria-chat-events";

/// Names one ask for as long as the daemon that took it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket(pub u64);

impl fmt::Display for Ticket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "#{}", self.0)
    }
}

/// Reads a ticket the way it prints, with or without the `#`.
impl FromStr for Ticket {
    type Err = std::num::ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.strip_prefix('#').unwrap_or(text).parse().map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub text: String,
    pub workdir: PathBuf,
    pub assistant_id: AssistantId,
    /// A model ID the assistant's CLI accepts; `None` uses the shared setting.
    pub model_id: Option<String>,
    /// Names the conversation to continue; `None` means the directory's.
    pub session: Option<String>,
    /// Forget the running conversation and start another.
    pub new: bool,
    /// Deliver the reply like a transcript instead of only announcing it.
    pub output_mode: Option<OutputMode>,
    /// The daemon's own-voice gate judged the recording this came from as
    /// the owner's. Typed text, and a recording nothing judged, are not.
    pub own_voice: bool,
    /// A tap of the Aria key cancels this run while it is in flight.
    pub tap_cancels: bool,
}

impl ChatRequest {
    pub fn from_config(
        text: String,
        assistant_id: AssistantId,
        config: ChatConfig,
    ) -> Result<Self, ChatRequestError> {
        let workdir = match config.workdir {
            Some(workdir) => workdir,
            None => std::env::current_dir().map_err(ChatRequestError::CurrentDir)?,
        };
        Ok(Self {
            text,
            workdir,
            assistant_id,
            model_id: None,
            session: None,
            new: false,
            output_mode: None,
            own_voice: false,
            tap_cancels: false,
        })
    }

    pub fn in_project(self, project: &Project) -> Self {
        Self {
            workdir: project.path.clone(),
            ..self
        }
    }

    pub fn key(&self) -> SessionKey {
        SessionKey {
            assistant_id: self.assistant_id.name().to_owned(),
            name: self
                .session
                .clone()
                .unwrap_or_else(|| self.workdir.to_string_lossy().into_owned()),
        }
    }
}

/// How a session is addressed: its assistant and its name, which is the
/// directory unless the request names one. Turns on one key run one after
/// another; different keys run side by side.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub(crate) assistant_id: String,
    pub(crate) name: String,
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.assistant_id, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptInput {
    Text(String),
    /// A history entry by id. The run's reply is saved under that entry.
    Entry(u64),
}

/// Which instructions a prompt call runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptChoice {
    Stored(aria_settings::PromptId),
    /// Instructions written for this run alone, run as compose.
    OneOff(Instructions),
}

/// Where a resolved prompt's instructions come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptInstructions {
    /// A stored prompt's Markdown, read when the run starts.
    File(PathBuf),
    OneOff(Instructions),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCall {
    pub input: PromptInput,
    pub prompt: PromptChoice,
    /// The app in front when the chord was pressed, the only one the result
    /// may be typed into. None when the caller could not tell.
    pub application: Option<aria_settings::ApplicationId>,
    /// A tap of the Aria key cancels this run while it is in flight.
    pub tap_cancels: bool,
    /// How to type the result. `None` types nothing: the result arrives only
    /// in the reply.
    pub output_mode: Option<OutputMode>,
}

/// Text to type that the caller already has, such as a result it showed
/// before choosing to type it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverCall {
    pub text: String,
    /// The only app the text may be typed into. None types into the app in
    /// front.
    pub application: Option<aria_settings::ApplicationId>,
    pub output_mode: OutputMode,
}

/// A prompt call resolved against the stored prompt and the configured
/// assistant. One fresh assistant run; no conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    pub text: String,
    /// The prompt's name, stored with the saved result.
    pub name: String,
    /// The history entry the text came from, which keeps the reply.
    pub entry: Option<u64>,
    pub kind: PromptKind,
    pub instructions: PromptInstructions,
    pub assistant_id: AssistantId,
    pub model_id: Option<String>,
    pub application: Option<aria_settings::ApplicationId>,
    pub tap_cancels: bool,
    pub output_mode: Option<OutputMode>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChatRequestError {
    #[error("failed to read the working directory")]
    CurrentDir(#[source] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatStage {
    pub ticket: Ticket,
    pub assistant_id: AssistantId,
    pub kind: ChatStageKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatStageKind {
    Working(Step),
    Replied(ChatReply),
    Failed { code: RpcErrorCode, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Thinking,
    Reading,
    Searching,
    Running,
    Skill(String),
    /// A step this build does not know; a newer daemon sent it.
    Unknown(i32),
}

/// Longer than any skill name; the rest of a runaway one is dropped.
const MAX_SKILL_NAME_CHARS: usize = 64;

impl Step {
    /// A skill named by the assistant, and so by whatever it read. Control
    /// characters would reach a terminal as-is, so they do not survive.
    pub fn skill(name: &str) -> Self {
        Self::Skill(
            name.chars()
                .filter(|character| !character.is_control())
                .take(MAX_SKILL_NAME_CHARS)
                .collect(),
        )
    }

    /// The one spelling of a step's kind; `Display` adds what it carries.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::Reading => "reading",
            Self::Searching => "searching",
            Self::Running => "running",
            Self::Skill(..) => "skill",
            Self::Unknown(..) => "unknown",
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skill(name) => write!(formatter, "skill: {name}"),
            Self::Unknown(code) => write!(formatter, "unknown ({code})"),
            step => formatter.write_str(step.label()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    pub text: String,
    /// Set when delivery was asked for and failed. The reply still
    /// came back, so a listener can show it instead.
    pub delivery_error: Option<RpcErrorCode>,
    /// The reply went to the clipboard instead of being typed.
    pub copied: bool,
}

pub type ChatEventEmitter = EventEmitter<ChatStage>;

pub struct SessionSpec<'a> {
    pub workdir: &'a Path,
    pub model_id: Option<&'a str>,
    pub resume: Option<&'a str>,
}

pub trait Assistant: Send + Sync {
    type Session: Session;

    fn models(&self, cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError>;

    /// Whether its command is installed and signed in. Spends no tokens.
    fn check_ready(&self, cancel: &CancelToken) -> AssistantStatus;

    fn open(&self, spec: SessionSpec<'_>) -> Self::Session;

    /// One run outside any conversation or project, with no tools: the
    /// text to type, or why there is none.
    fn run_prompt(
        &self,
        request: &PromptRun<'_>,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<String, AssistantError>;
}

#[derive(Debug)]
pub enum AssistantStatus {
    Ready,
    /// Its command is not on the daemon's PATH.
    NotInstalled {
        binary: PathBuf,
    },
    /// Its command runs but has no login.
    SignedOut {
        fix: &'static str,
    },
    /// The check itself failed, so the status is not known.
    Unchecked(AssistantError),
}

impl AssistantStatus {
    /// What makes it ready, or why the check failed; `None` when ready.
    pub fn fix(&self) -> Option<String> {
        match self {
            Self::Ready => None,
            Self::NotInstalled { binary } => {
                Some(format!("install the `{}` command", binary.display()))
            }
            Self::SignedOut { fix } => Some((*fix).to_owned()),
            Self::Unchecked(error) => Some(error_chain(error)),
        }
    }
}

/// One conversation with an assistant, taking one turn at a time.
pub trait Session: Send {
    /// Sends one prompt and feeds its output to `sink` until the turn ends.
    fn prompt(
        &mut self,
        text: &str,
        sink: &mut dyn TurnSink,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<(), AssistantError>;

    /// What continues this conversation next time; `None` before the
    /// first reply.
    fn resume_id(&self) -> Option<&str>;
}

/// Where a turn's output goes.
pub trait TurnSink: Send {
    fn text(&mut self, chunk: &str);
    /// What the assistant has moved on to. Repeats are the sink's to drop.
    fn step(&mut self, step: Step);
}

#[derive(Debug, thiserror::Error)]
pub enum AssistantError {
    #[error(
        "assistant executable {} was not found; install its command-line app",
        binary.display()
    )]
    Missing { binary: PathBuf },

    #[error("the assistant's working directory {} does not exist", .0.display())]
    NoWorkdir(PathBuf),

    #[error("the assistant could not run")]
    Run(#[source] RunError),

    #[error("failed to prepare the assistant's run directory")]
    RunDirectory(#[source] std::io::Error),

    #[error("the assistant failed: {0}")]
    Failed(String),

    #[error("{} isn't signed in; {fix}", assistant.display_name())]
    SignedOut {
        assistant: AssistantId,
        fix: &'static str,
    },

    #[error("the assistant's output could not be understood: {0}")]
    Malformed(String),

    #[error("the assistant exceeded its deadline of {0:?}")]
    Timeout(Duration),

    #[error("the ask was cancelled")]
    Cancelled,
}

/// Where an ask goes when its caller has no working directory of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskTarget {
    pub workdir: PathBuf,
    pub assistant_id: AssistantId,
}

#[cfg(test)]
mod tests {
    use super::Ticket;

    #[test]
    fn a_ticket_reads_as_it_prints() {
        assert_eq!("7".parse::<Ticket>(), Ok(Ticket(7)));
        assert_eq!("#7".parse::<Ticket>(), Ok(Ticket(7)));
        assert!("seven".parse::<Ticket>().is_err());
        assert!("##7".parse::<Ticket>().is_err());
    }
}
