//! Daemon runtime, RPC server, and session ownership.
//!
//! One import point for both roles: hosts run a [`Daemon`], clients open a
//! [`DaemonSession`] or watch an [`EventStream`].

mod assembly;
mod capture;
mod chat;
mod context;
mod dictation;
pub mod guided;
mod host;
mod hotkey;
mod listen;
mod log;
mod memory;
mod operation;
mod preferences;
mod relay;
mod server;
mod service;
mod session;
mod shutdown;
mod speaker;
mod speech;
mod subscriptions;
mod tasks;
mod transcripts;
mod vocabulary;
mod wake;

pub use assembly::DaemonError;
pub use dictation::DictationInitializationError;
pub use host::{
    Daemon, DaemonBuilder, DaemonFailure, DaemonLifetime, DaemonStopper, RunningDaemon,
};
pub use log::{DaemonLogError, open_log_writer, read_log_tail};
pub use session::{
    AskError, Clarify, DaemonSession, ListenCommandError, ListenFollow, ListenSnapshot,
    ListenStarted, ListenStopped, PendingAsk, RouteError, Routing, StartupError, WakeWordStatus,
    WakeWordStatusError,
};
pub use shutdown::ShutdownError;

/// What `aria doctor` needs to report about the relay without owning one.
pub(crate) use relay::{SERVICE_TYPE as RELAY_SERVICE_TYPE, ffmpeg_path};
pub use speech::SpeechInitializationError;

pub use crate::chat::{
    Ambiguous, ChatReply, ChatRequest, ChatRequestError, ChatStage, ChatStageKind, DiscoverError,
    Focus, Project, ProjectsError, Step, Ticket,
};
pub use crate::config::ChatAgent;
pub use crate::dictation::{
    DeliveryMode, DictationOutcome, DictationRequest, DictationResult, DictationStage, OutputMode,
    RecordingId, StopMode,
};
pub use crate::enrollment::{
    EnrolledSpeaker, EnrollmentRequest, EnrollmentResult, EnrollmentStage,
};
pub use crate::ipc::{ClientError, ConnectError, DaemonEvent, EventStream};
pub use crate::listen::ListenStage;
pub use crate::speaker::{Role, SpeakerName, SpeakerNameError};
pub use crate::speech::{SpeechOutcome, SpeechRequest, SpeechStage};
