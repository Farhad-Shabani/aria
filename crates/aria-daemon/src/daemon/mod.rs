//! Daemon runtime, RPC server, and session ownership.
//!
//! One import point for both roles: hosts run a [`Daemon`], clients open a
//! [`DaemonSession`] or watch an [`EventStream`].

pub mod config;

mod assembly;
mod capture;
mod chat;
mod context;
mod dictation;
pub mod guided;
mod host;
mod hotkey;
mod inference;
mod listen;
#[cfg(test)]
mod live_keys;
mod log;
mod memory;
mod operation;
mod relay;
mod server;
mod service;
mod session;
mod settings;
#[cfg(target_os = "macos")]
mod shell;
mod shutdown;
mod speaker;
mod speech;
mod subscriptions;
mod tasks;
#[cfg(test)]
mod testing;
mod transcripts;
mod vocabulary;
mod wake;

pub use assembly::DaemonError;
pub use dictation::DictationInitializationError;
pub use host::{
    Daemon, DaemonBuilder, DaemonFailure, DaemonLifetime, DaemonStopper, RunningDaemon,
};
pub use inference::admin::{remote_invite, remote_revoke};
pub use inference::client::RemoteClientError;
pub use inference::host::{InferenceHostError, serve_inference};
pub use inference::pairing::{Forgotten, remote_pair};
pub use log::{DaemonLogError, open_log_writer, read_log_tail};
pub use session::{
    AskError, Clarify, DaemonSession, ListenCommandError, ListenFollow, ListenSnapshot,
    ListenStarted, ListenStopped, PendingAsk, RouteError, Routing, StartupError, WakeWordStatus,
    WakeWordStatusError, restart_required_keys,
};
#[cfg(target_os = "macos")]
pub use shell::{LoginShellError, login_shell_path, needs_login_shell_path};
pub use shutdown::ShutdownError;

pub use inference::Paths as InferencePaths;
pub use inference::pairing::forget as forget_paired_server;
#[cfg(any(test, feature = "test-support"))]
pub use inference::pin_server_for_tests;
pub use inference::{PairedServer, paired_server};
/// What `aria doctor` needs to report about the relay without owning one.
pub use relay::SERVICE_TYPE as RELAY_SERVICE_TYPE;
pub(crate) use relay::{Invite, RelayStage};

pub use crate::ipc::{ClientError, ConnectError, DaemonEvent, EventStream, TransportFailure};
pub use aria_assistant::chat::{
    Ambiguous, ChatReply, ChatRequest, ChatRequestError, ChatStage, ChatStageKind, DiscoverError,
    Focus, Project, ProjectsError, Step, Ticket,
};
pub use aria_platform::delivery::OutputMode;
pub use aria_settings::DeliveryMode;
pub use aria_settings::config::AssistantId;
pub use aria_voice::dictation::{
    DictationOutcome, DictationRequest, DictationResult, DictationStage, RecordingId, StopMode,
};
pub use aria_voice::enrollment::{
    EnrolledSpeaker, EnrollmentRequest, EnrollmentResult, EnrollmentStage,
};
pub use aria_voice::listen::ListenStage;
pub use aria_voice::speaker::{Role, SpeakerName, SpeakerNameError};
pub use aria_voice::speech::{SpeechOutcome, SpeechRequest, SpeechStage};
