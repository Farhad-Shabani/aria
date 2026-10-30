//! Domain types for talking to coding assistants.
//!
//! The daemon owns conversations keyed by assistant and session name. Each ask
//! returns a [`Ticket`]; its reply arrives through [`ChatStage`] events.
//! A [`Step`] describes the assistant's activity while the ask is running.

mod assistants;
mod models;
pub use models::AssistantModel;
pub(crate) use models::is_valid_model_id;
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
use aria_kit::primitives::process::ChildProcessError;
use serde::{Deserialize, Serialize};

use crate::config::{AssistantId, ChatConfig, ConfigError, Environment};
use crate::delivery::{DeliveryError, OutputMode};
use crate::event::EventEmitter;
use crate::primitives::error_chain;
use crate::rpc::RpcErrorCode;

pub(crate) use assistants::{
    CLAUDE_BINARY, CODEX_BINARY, ClaudeCodeAssistant, CodexAssistant, claude_command, parse_claude,
};
pub(crate) use process::run_cli;
pub use projects::{Ambiguous, DiscoverError, Focus, Mention, Project, Projects, ProjectsError};
pub use prompt_run::PromptKind;
pub(crate) use prompt_run::{InstructionsError, PromptRun, read_instructions};
pub(crate) use sessions::{SessionStore, SessionStoreError};
pub(crate) use subscribers::ChatSubscribers;

pub(crate) const EVENT_THREAD: &str = "aria-chat-events";

/// The most `chat.timeout` may ask for.
pub(crate) const MAX_TIMEOUT: Duration = Duration::from_secs(570);

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
    /// A model ID the assistant's CLI accepts; `None` uses the shared preference.
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
    pub fn configured(text: String, assistant_id: AssistantId) -> Result<Self, ChatRequestError> {
        let config = ChatConfig::load(&Environment::capture()?, assistant_id)?;
        Self::from_config(text, config)
    }

    pub(crate) fn from_config(text: String, config: ChatConfig) -> Result<Self, ChatRequestError> {
        let workdir = match config.workdir {
            Some(workdir) => workdir,
            None => std::env::current_dir().map_err(ChatRequestError::CurrentDir)?,
        };
        Ok(Self {
            text,
            workdir,
            assistant_id: config.assistant_id,
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

    pub(crate) fn key(&self) -> SessionKey {
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
pub(crate) struct SessionKey {
    pub(crate) assistant_id: String,
    pub(crate) name: String,
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.assistant_id, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptCall {
    pub(crate) text: String,
    pub(crate) prompt: crate::preferences::PromptId,
    /// The app in front when the chord was pressed, the only one the result
    /// may be typed into. None when the caller could not tell.
    pub(crate) application: Option<crate::preferences::ApplicationId>,
    /// A tap of the Aria key cancels this run while it is in flight.
    pub(crate) tap_cancels: bool,
}

/// A prompt call resolved against the stored prompt and the configured
/// assistant. One fresh assistant run; no conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptRequest {
    pub(crate) text: String,
    pub(crate) kind: PromptKind,
    /// The Markdown instructions, read when the run starts.
    pub(crate) file: PathBuf,
    pub(crate) assistant_id: AssistantId,
    pub(crate) model_id: Option<String>,
    pub(crate) application: Option<crate::preferences::ApplicationId>,
    pub(crate) tap_cancels: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ChatRequestError {
    #[error("failed to read the chat configuration")]
    Config(#[from] ConfigError),

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
    pub(crate) fn skill(name: &str) -> Self {
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

pub(crate) type ChatEventEmitter = EventEmitter<ChatStage>;

pub(crate) struct SessionSpec<'a> {
    pub(crate) workdir: &'a Path,
    pub(crate) model_id: Option<&'a str>,
    pub(crate) resume: Option<&'a str>,
}

pub(crate) trait Assistant: Send + Sync {
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
pub(crate) enum AssistantStatus {
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
    pub(crate) fn fix(&self) -> Option<String> {
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
pub(crate) trait Session: Send {
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
pub(crate) trait TurnSink: Send {
    fn text(&mut self, chunk: &str);
    /// What the assistant has moved on to. Repeats are the sink's to drop.
    fn step(&mut self, step: Step);
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AssistantError {
    #[error(
        "assistant executable {} was not found; install its command-line app",
        binary.display()
    )]
    Missing { binary: PathBuf },

    #[error("the assistant's working directory {} does not exist", .0.display())]
    NoWorkdir(PathBuf),

    #[error("failed to start the assistant")]
    Spawn(#[source] std::io::Error),

    #[error("failed to write the prompt to the assistant")]
    Input(#[source] std::io::Error),

    #[error("failed to prepare the assistant's run directory")]
    RunDirectory(#[source] std::io::Error),

    #[error("failed to read the assistant's output")]
    Io(#[source] std::io::Error),

    #[error("failed to wait for the assistant")]
    Wait(#[source] ChildProcessError),

    #[error("the assistant failed: {0}")]
    Failed(String),

    #[error("the assistant is not signed in; {fix}")]
    SignedOut { fix: &'static str },

    #[error("the assistant's output could not be understood: {0}")]
    Malformed(String),

    #[error("the assistant wrote more than {0} bytes")]
    OutputTooLarge(usize),

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

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResolveError {
    #[error("failed to read preferences")]
    Preferences(#[source] crate::preferences::PreferenceStoreError),

    #[error("failed to load chat configuration")]
    Config(#[source] ConfigError),

    #[error("failed to read the known projects")]
    Projects(#[source] ProjectsError),

    #[error("failed to resolve the assistant's working directory")]
    Directory(#[source] std::io::Error),

    #[error(
        "no project is in focus and chat.workdir is unset; ask from a project once, \
         or set chat.workdir in the config"
    )]
    NoTarget,
}

impl From<&ResolveError> for RpcErrorCode {
    fn from(error: &ResolveError) -> Self {
        match error {
            ResolveError::Config(_) | ResolveError::Projects(_) | ResolveError::Preferences(_) => {
                Self::Internal
            }
            ResolveError::NoTarget | ResolveError::Directory(_) => Self::InvalidRequest,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChatError {
    #[error("failed to load chat configuration")]
    Config(#[source] ConfigError),

    #[error(transparent)]
    Assistant(#[from] AssistantError),

    #[error("failed to deliver the reply")]
    Delivery(#[source] DeliveryError),

    #[error(transparent)]
    Session(#[from] SessionStoreError),

    #[error("failed to read shared preferences")]
    Preferences(#[source] crate::preferences::PreferenceStoreError),

    #[error("no stored prompt has that ID")]
    UnknownPrompt,

    #[error("that prompt is turned off")]
    PromptOff,

    #[error(transparent)]
    Instructions(#[from] InstructionsError),
}

impl From<&ChatError> for RpcErrorCode {
    fn from(error: &ChatError) -> Self {
        match error {
            ChatError::Config(_) | ChatError::Session(_) | ChatError::Preferences(_) => {
                Self::Internal
            }
            ChatError::Assistant(AssistantError::Missing { .. }) => Self::AssistantUnavailable,
            ChatError::Assistant(AssistantError::NoWorkdir(_))
            | ChatError::UnknownPrompt
            | ChatError::PromptOff => Self::InvalidRequest,
            ChatError::Instructions(_) => Self::PromptUnreadable,
            ChatError::Assistant(AssistantError::Timeout(_)) => Self::AssistantTimeout,
            ChatError::Assistant(AssistantError::SignedOut { .. }) => Self::AssistantSignedOut,
            ChatError::Assistant(AssistantError::Cancelled)
            | ChatError::Delivery(DeliveryError::Cancelled) => Self::Cancelled,
            ChatError::Assistant(_) => Self::AssistantFailed,
            ChatError::Delivery(DeliveryError::Timeout(_)) => Self::DeliveryTimeout,
            ChatError::Delivery(DeliveryError::TargetChanged) => Self::DeliveryTargetChanged,
            ChatError::Delivery(DeliveryError::ApplicationExcluded) => Self::ApplicationExcluded,
            ChatError::Delivery(_) => Self::DeliveryFailed,
        }
    }
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
