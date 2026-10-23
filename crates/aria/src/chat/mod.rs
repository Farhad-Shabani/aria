//! Domain types for talking to coding agents.
//!
//! The daemon owns conversations keyed by agent and session name. Each ask
//! returns a [`Ticket`]; its reply arrives through [`ChatStage`] events.
//! A [`Step`] describes the agent's activity while the ask is running.

mod agents;
mod process;
mod projects;
mod sessions;
mod subscribers;

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::process::ChildProcessError;
use serde::{Deserialize, Serialize};

use crate::config::{ChatAgent, ChatConfig, ConfigError, Environment};
use crate::delivery::{DeliveryError, OutputMode};
use crate::event::EventEmitter;
use crate::rpc::RpcErrorCode;

pub(crate) use agents::{
    CLAUDE_BINARY, CODEX_BINARY, ClaudeCodeAgent, CodexAgent, claude_command, parse_claude,
};
pub(crate) use process::run_cli;
pub use projects::{Ambiguous, DiscoverError, Focus, Mention, Project, Projects, ProjectsError};
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
    pub agent: ChatAgent,
    /// A model name the agent's CLI accepts; `None` is the agent's default.
    pub model: Option<String>,
    /// Names the conversation to continue; `None` means the directory's.
    pub session: Option<String>,
    /// Forget the running conversation and start another.
    pub new: bool,
    /// Deliver the reply like a transcript instead of only announcing it.
    pub output_mode: Option<OutputMode>,
    /// The daemon's own-voice gate judged the recording this came from as
    /// the owner's. Typed text, and a recording nothing judged, are not.
    pub own_voice: bool,
}

impl ChatRequest {
    /// A request for the configured agent, working where `chat.workdir`
    /// says or else in the current directory.
    pub fn configured(text: String) -> Result<Self, ChatRequestError> {
        let config = ChatConfig::load(&Environment::capture()?)?;
        let workdir = match config.workdir {
            Some(workdir) => workdir,
            None => std::env::current_dir().map_err(ChatRequestError::CurrentDir)?,
        };
        Ok(Self {
            text,
            workdir,
            agent: config.agent,
            model: None,
            session: None,
            new: false,
            output_mode: None,
            own_voice: false,
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
            agent: self.agent.name().to_owned(),
            name: self
                .session
                .clone()
                .unwrap_or_else(|| self.workdir.to_string_lossy().into_owned()),
        }
    }
}

/// How a session is addressed: its agent and its name, which is the
/// directory unless the request names one. Turns on one key run one after
/// another; different keys run side by side.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct SessionKey {
    pub(crate) agent: String,
    pub(crate) name: String,
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.agent, self.name)
    }
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
    pub agent: ChatAgent,
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
    /// A skill named by the agent, and so by whatever it read. Control
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
}

pub(crate) type ChatEventEmitter = EventEmitter<ChatStage>;

pub(crate) struct SessionSpec<'a> {
    pub(crate) workdir: &'a Path,
    pub(crate) model: Option<&'a str>,
    pub(crate) resume: Option<&'a str>,
}

/// A kind of agent and how to reach it.
pub(crate) trait Agent: Send + Sync {
    fn open(&self, spec: SessionSpec<'_>) -> Box<dyn Session>;
}

/// One conversation with an agent, taking one turn at a time.
pub(crate) trait Session: Send {
    /// Sends one prompt and feeds its output to `sink` until the turn ends.
    fn prompt(
        &mut self,
        text: &str,
        sink: &mut dyn TurnSink,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<(), AgentError>;

    /// What continues this conversation next time; `None` before the
    /// first reply.
    fn resume_id(&self) -> Option<&str>;
}

/// Where a turn's output goes.
pub(crate) trait TurnSink: Send {
    fn text(&mut self, chunk: &str);
    /// What the agent has moved on to. Repeats are the sink's to drop.
    fn step(&mut self, step: Step);
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentError {
    #[error(
        "agent binary {} is not installed or not on PATH; install it, or pick another \
         agent with --agent or chat.agent",
        binary.display()
    )]
    Missing { binary: PathBuf },

    #[error("the agent's working directory {} does not exist", .0.display())]
    NoWorkdir(PathBuf),

    #[error("failed to start the agent")]
    Spawn(#[source] std::io::Error),

    #[error("failed to write the prompt to the agent")]
    Input(#[source] std::io::Error),

    #[error("failed to read the agent's output")]
    Io(#[source] std::io::Error),

    #[error("failed to wait for the agent")]
    Wait(#[source] ChildProcessError),

    #[error("the agent failed: {0}")]
    Failed(String),

    #[error("the agent's output could not be understood: {0}")]
    Malformed(String),

    #[error("the agent wrote more than {0} bytes")]
    OutputTooLarge(usize),

    #[error("the agent exceeded its deadline of {0:?}")]
    Timeout(Duration),

    #[error("the ask was cancelled")]
    Cancelled,
}

/// Where an ask goes when its caller has no working directory of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskTarget {
    pub workdir: PathBuf,
    pub agent: ChatAgent,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResolveError {
    #[error("failed to load chat configuration")]
    Config(#[source] ConfigError),

    #[error("failed to read the known projects")]
    Projects(#[source] ProjectsError),

    #[error("failed to resolve the agent's working directory")]
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
            ResolveError::Config(_) | ResolveError::Projects(_) => Self::Internal,
            ResolveError::NoTarget | ResolveError::Directory(_) => Self::InvalidRequest,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChatError {
    #[error("failed to load chat configuration")]
    Config(#[source] ConfigError),

    #[error(transparent)]
    Agent(#[from] AgentError),

    #[error("failed to deliver the reply")]
    Delivery(#[source] DeliveryError),

    #[error(transparent)]
    Session(#[from] SessionStoreError),

    #[error("failed to read shared preferences")]
    Preferences(#[source] crate::preferences::PreferenceStoreError),
}

impl From<&ChatError> for RpcErrorCode {
    fn from(error: &ChatError) -> Self {
        match error {
            ChatError::Config(_) | ChatError::Session(_) | ChatError::Preferences(_) => {
                Self::Internal
            }
            ChatError::Agent(AgentError::Missing { .. }) => Self::AgentUnavailable,
            ChatError::Agent(AgentError::NoWorkdir(_)) => Self::InvalidRequest,
            ChatError::Agent(AgentError::Timeout(_)) => Self::AgentTimeout,
            ChatError::Agent(AgentError::Cancelled)
            | ChatError::Delivery(DeliveryError::Cancelled) => Self::Cancelled,
            ChatError::Agent(_) => Self::AgentFailed,
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
