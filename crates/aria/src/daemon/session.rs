//! Acquisition and ownership of a usable daemon session.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crate::chat::{
    Ambiguous, AskTarget, AssistantModel, ChatReply, ChatRequest, ChatRequestError, ChatStage,
    ChatStageKind, Focus, Mention, Project, Projects, ProjectsError, Ticket,
};
use crate::delivery::{PlatformTextDelivery, TextDelivery};
use crate::dictation::{
    DeliveryMode, DictationOutcome, DictationRequest, DictationResult, DictationStage, RecordingId,
    StopMode,
};
use crate::enrollment::{EnrolledSpeaker, EnrollmentRequest, EnrollmentResult};
use crate::ipc::{
    ClientError, ConnectError, DaemonClient, DaemonEvent, EventStream, Liveness, ProbeError,
    WAKE_WORD_HEALTH_SERVICE, probe_daemon,
};
use crate::listen::ListenStage;
use crate::rpc::RpcErrorCode;
use crate::runtime::{PreparedRuntimePaths, RuntimeError};
use crate::settings::{SettingsPatch, SettingsSnapshot, UpdateResult};
use crate::speaker::SpeakerName;
use crate::speech::{SpeechOutcome, SpeechRequest};
use aria_kit::synthesis::{SpeechText, SpeechTextError};
use tonic_health::pb::health_check_response::ServingStatus;

use crate::config::{AssistantId, ChatConfig, ConfigError, WakeWordOverride};

use super::assembly::DaemonError;
use super::host::{DaemonFailure, DaemonLifetime, SpawnedRuntime, spawn_runtime};
use super::shutdown::ShutdownSignal;

pub(super) const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(50);

pub struct DaemonSession {
    // Field order: the connection closes before an owned temporary daemon
    // drops, which stops it.
    client: DaemonClient,
    _daemon: Option<SpawnedRuntime>,
}

impl DaemonSession {
    /// Opens against the current user's daemon, starting a temporary one
    /// when none is running and the platform allows it.
    pub fn open() -> Result<Self, StartupError> {
        let paths = PreparedRuntimePaths::for_current_user().map_err(StartupError::Runtime)?;
        Self::open_at(&paths)
    }

    pub fn open_at(paths: &PreparedRuntimePaths) -> Result<Self, StartupError> {
        if !PlatformTextDelivery::OUTLIVES_PROCESS {
            return connect_to_required(paths).map(Self::existing);
        }

        // Spawn and connect alternate under one deadline: an incumbent that
        // stops accepting but still holds the socket during its teardown
        // would otherwise strand the connect-only retry loop.
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            // The process embedding this daemon owns its signals: `aria
            // dictate` stops the press on Ctrl-C, which a draining daemon
            // would cancel.
            let shutdown = ShutdownSignal::new()
                .map_err(|error| StartupError::Build(DaemonError::Shutdown(error)))?;
            match spawn_runtime(
                paths,
                DaemonLifetime::OnDemand,
                WakeWordOverride::Inherit,
                shutdown,
            ) {
                Ok(daemon) => return start_temporary(daemon, paths, deadline),
                Err(error) if error.is_socket_owned_elsewhere() => {}
                Err(error) => return Err(StartupError::Build(error)),
            }

            let pending = match probe_daemon(paths.socket_path()) {
                Ok(Liveness::Ready(client)) => return Ok(Self::existing(client)),
                Ok(Liveness::NotRunning(error)) => ProbeError::Connect(error),
                Ok(Liveness::Starting(error)) => ProbeError::Readiness(error),
                Err(error) => return Err(probe_error(paths, error)),
            };

            if Instant::now() >= deadline {
                return Err(gave_up(paths, pending));
            }
            thread::sleep(CONNECT_RETRY_INTERVAL);
        }
    }

    pub fn wake_word_status() -> Result<WakeWordStatus, WakeWordStatusError> {
        let paths =
            PreparedRuntimePaths::for_current_user().map_err(WakeWordStatusError::Runtime)?;
        Self::wake_word_status_at(&paths)
    }

    pub(crate) fn wake_word_status_at(
        paths: &PreparedRuntimePaths,
    ) -> Result<WakeWordStatus, WakeWordStatusError> {
        let mut client = match DaemonClient::connect(paths.socket_path()) {
            Ok(client) => client,
            Err(error) if error.is_daemon_unavailable() => return Ok(WakeWordStatus::Stopped),
            Err(error) => return Err(WakeWordStatusError::Connect(error)),
        };

        client
            .service_status(WAKE_WORD_HEALTH_SERVICE)
            .map(classify_wake_word_status)
            .map_err(WakeWordStatusError::Query)
    }

    /// Runs one press to its end. Each stage passes through `observe` on
    /// the way, so a caller can act on the recording's start and end.
    pub fn request_dictation(
        &self,
        request: DictationRequest,
        observe: impl FnMut(&DictationStage),
    ) -> Result<DictationOutcome, ClientError> {
        self.client.request_dictation(request, observe)
    }

    /// Hands the ask to the daemon. The reply arrives on the event stream,
    /// subscribed before the ask goes out so it cannot slip past.
    pub fn ask(&self, request: ChatRequest) -> Result<PendingAsk, ClientError> {
        let events = self.client.subscribe()?;
        let ticket = self.client.ask(request)?;
        Ok(PendingAsk { ticket, events })
    }

    /// Ends the named recording so its transcript comes back. False when it
    /// is not the one running.
    pub fn stop_dictation(&self, recording: RecordingId) -> Result<bool, ClientError> {
        self.client.stop_dictation(recording)
    }

    pub fn list_assistant_models(
        &self,
        assistant_id: AssistantId,
    ) -> Result<Vec<AssistantModel>, ClientError> {
        self.client.list_assistant_models(assistant_id)
    }

    /// Where an ask goes when the caller has no working directory.
    pub fn resolve_ask(&self) -> Result<AskTarget, ClientError> {
        self.client.resolve_ask()
    }

    /// Ends an ask, running or waiting; `false` when it was neither.
    pub fn cancel(&self, ticket: Ticket) -> Result<bool, ClientError> {
        self.client.cancel_ask(ticket)
    }

    pub fn speak(&self, request: SpeechRequest) -> Result<SpeechOutcome, ClientError> {
        self.client.request_speech(request)
    }

    /// Records one utterance and hands back what was heard. `None` when this
    /// call stopped a recording already running instead of starting one.
    pub fn transcribe(
        &self,
        stop_mode: StopMode,
        observe: impl FnMut(&DictationStage),
    ) -> Result<Option<DictationResult>, ClientError> {
        self.transcribe_in(stop_mode, crate::vocabulary::Context::Default, observe)
    }

    pub fn transcribe_in(
        &self,
        stop_mode: StopMode,
        project: crate::vocabulary::Context,
        observe: impl FnMut(&DictationStage),
    ) -> Result<Option<DictationResult>, ClientError> {
        let outcome = self.request_dictation(
            DictationRequest {
                output_mode: None,
                delivery_mode: Some(DeliveryMode::Complete),
                stop_mode,
                transcript_only: true,
                project,
            },
            observe,
        )?;
        Ok(match outcome {
            DictationOutcome::Completed(result) => Some(result),
            DictationOutcome::StopAccepted => None,
        })
    }

    pub fn route(
        &self,
        text: String,
        clarify: Clarify,
        assistant_id: AssistantId,
    ) -> Result<Routing, RouteError> {
        let home = crate::config::home_directory().map_err(ProjectsError::Config)?;
        let settings = crate::settings::load(&home)
            .map_err(ConfigError::from)
            .map_err(ProjectsError::Config)?;
        let config = ChatConfig::load(&settings).map_err(ProjectsError::Config)?;
        let projects = Projects::load_with(&home, &config)?;
        let request = ChatRequest::from_config(text, assistant_id, config)?;
        let project = match projects.mentioned(&request.text) {
            Mention::None => {
                let request = match projects.focused() {
                    Some(project) => request.in_project(project),
                    None => request,
                };
                return Ok(Routing::Ready {
                    request,
                    focus: None,
                });
            }
            Mention::One(project) => project,
            Mention::Several(candidates) => match self.ask_which(candidates, clarify)? {
                Some(project) => project,
                None => return Ok(Routing::Stopped),
            },
        };
        Ok(Routing::Ready {
            request: request.in_project(&project),
            focus: Some(projects.focus(project)),
        })
    }

    /// Speaks the choice and records one answer. `None` when either was
    /// stopped.
    fn ask_which(
        &self,
        candidates: Ambiguous,
        clarify: Clarify,
    ) -> Result<Option<Project>, RouteError> {
        let Clarify::Aloud { stop_mode } = clarify else {
            return Err(RouteError::Ambiguous(candidates));
        };
        let question = SpeechText::new(format!("{}?", candidates.names().join(", or ")))?;
        let spoken = self.speak(SpeechRequest {
            text: question,
            voice: None,
            rate: None,
        })?;
        if spoken == SpeechOutcome::Stopped {
            return Ok(None);
        }
        let Some(answer) = self.transcribe(stop_mode, |_stage| {})? else {
            return Ok(None);
        };
        let answer = answer.text;
        match candidates.pick(&answer) {
            Some(project) => Ok(Some(project)),
            None => Err(RouteError::Unresolved {
                heard: answer,
                candidates,
            }),
        }
    }

    /// Stops the current utterance, if any; answers whether one was running.
    pub fn stop_speaking(&self) -> Result<bool, ClientError> {
        self.client.stop_speech()
    }

    pub fn list_history(
        &self,
        query: crate::history::HistoryQuery,
    ) -> Result<crate::history::HistoryPage, ClientError> {
        self.client.list_history(query)
    }

    pub fn retry_history(
        &self,
        id: u64,
        options: crate::history::RetryOptions,
    ) -> Result<crate::history::HistoryEntry, ClientError> {
        self.client.retry_history(id, options)
    }

    pub fn clear_history(&self) -> Result<crate::history::ClearHistoryResult, ClientError> {
        self.client.clear_history()
    }

    pub fn vocabulary_status(
        &self,
        directory: Option<&crate::vocabulary::Directory>,
    ) -> Result<crate::vocabulary::Status, ClientError> {
        self.client.vocabulary_status(directory)
    }

    pub fn vocabulary_refresh(
        &self,
        directory: Option<&crate::vocabulary::Directory>,
    ) -> Result<crate::vocabulary::Status, ClientError> {
        self.client.vocabulary_refresh(directory)
    }

    pub fn vocabulary_forget(
        &self,
        directory: Option<&crate::vocabulary::Directory>,
    ) -> Result<crate::vocabulary::Status, ClientError> {
        self.client.vocabulary_forget(directory)
    }

    pub fn get_settings(&self) -> Result<SettingsSnapshot, ClientError> {
        self.client.get_settings()
    }

    /// Whether settings.toml is valid, and what waits for a restart.
    pub fn settings_status(&self) -> Result<crate::settings::SettingsStatus, ClientError> {
        self.client.settings_status()
    }

    /// Subscribes to the daemon's events on this session's connection.
    pub fn events(&self) -> Result<EventStream, ClientError> {
        self.client.subscribe()
    }

    pub fn update_settings(
        &self,
        expected_revision: u64,
        patch: SettingsPatch,
    ) -> Result<UpdateResult, ClientError> {
        self.client.update_settings(expected_revision, patch)
    }

    /// Connects to a running daemon and never starts one: a ticketed ask
    /// outlives its request, which a temporary daemon would not.
    pub fn connect_at(paths: &PreparedRuntimePaths) -> Result<Self, StartupError> {
        connect_existing(paths).map(Self::existing)
    }

    fn existing(client: DaemonClient) -> Self {
        Self {
            client,
            _daemon: None,
        }
    }

    fn temporary(client: DaemonClient, daemon: SpawnedRuntime) -> Self {
        Self {
            client,
            _daemon: Some(daemon),
        }
    }
}

impl DaemonSession {
    /// `output` names the exact transcript file. The daemon resolves
    /// nothing, so a relative path is made absolute here.
    pub fn listen_start(
        &self,
        output: Option<PathBuf>,
    ) -> Result<ListenStarted, ListenCommandError> {
        let output = output
            .map(|path| {
                std::path::absolute(&path)
                    .map_err(|_error| ListenCommandError::UnresolvableOutput(path))
            })
            .transpose()?;
        let response = self
            .client
            .listen_start(output.map(|path| path.display().to_string()))
            .map_err(ListenCommandError::Call)?;
        Ok(ListenStarted {
            already_listening: response.already_listening,
            transcript_path: response.transcript_path,
        })
    }

    /// Records the samples and stores the voice they agree on. Progress
    /// arrives on the event stream, so a caller that wants to prompt the
    /// person subscribes before calling.
    pub fn enroll(&self, request: EnrollmentRequest) -> Result<EnrollmentResult, ClientError> {
        self.client.enroll(request)
    }

    pub fn list_speakers(&self) -> Result<Vec<EnrolledSpeaker>, ClientError> {
        self.client.list_speakers()
    }

    pub fn remove_speaker(&self, name: &SpeakerName) -> Result<(), ClientError> {
        self.client.remove_speaker(name.to_string())
    }

    pub fn set_owner(&self, name: &SpeakerName) -> Result<(), ClientError> {
        self.client.set_owner(name.to_string())
    }

    pub fn listen_stop(&self) -> Result<ListenStopped, ListenCommandError> {
        let response = self
            .client
            .listen_stop()
            .map_err(ListenCommandError::Call)?;
        Ok(ListenStopped {
            was_listening: response.was_listening,
            transcript_path: response.transcript_path,
        })
    }

    pub fn listen_status(&self) -> Result<ListenSnapshot, ListenCommandError> {
        let response = self
            .client
            .listen_status()
            .map_err(ListenCommandError::Call)?;
        Ok(ListenSnapshot {
            listening: response.listening,
            transcript_path: response.transcript_path,
            error: response.error_message,
        })
    }

    /// Follows the running listen session. Subscribed before the status
    /// check, so a session ending in between is not missed.
    pub fn follow_listen(&self) -> Result<ListenFollow, ListenCommandError> {
        let events = self.client.subscribe().map_err(ListenCommandError::Call)?;
        let status = self.listen_status()?;
        if !status.listening {
            return Err(ListenCommandError::NotListening);
        }
        Ok(ListenFollow {
            events,
            transcript_path: status.transcript_path,
            ended: false,
        })
    }
}

/// The stages of a running listen session, ending with `Stopped` or
/// `Failed`. Ends early when the daemon closes the stream.
pub struct ListenFollow {
    events: EventStream,
    pub transcript_path: String,
    ended: bool,
}

impl Iterator for ListenFollow {
    type Item = Result<ListenStage, ClientError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        loop {
            match self.events.next()? {
                Ok(DaemonEvent::Listen(stage)) => {
                    self.ended = matches!(stage, ListenStage::Stopped | ListenStage::Failed(_));
                    return Some(Ok(stage));
                }
                Ok(_) => {}
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

/// An ask the daemon has taken, with the stream its reply arrives on.
pub struct PendingAsk {
    ticket: Ticket,
    events: EventStream,
}

impl PendingAsk {
    pub fn ticket(&self) -> Ticket {
        self.ticket
    }

    /// Waits for the reply, skipping every other event on the way. Each
    /// stage of this ask passes through `observe` first, so a caller can
    /// show progress.
    pub fn reply(self, observe: impl FnMut(&ChatStage)) -> Result<ChatReply, AskError> {
        reply_from(self.ticket, self.events, observe)
    }
}

fn reply_from(
    ticket: Ticket,
    events: impl IntoIterator<Item = Result<DaemonEvent, ClientError>>,
    mut observe: impl FnMut(&ChatStage),
) -> Result<ChatReply, AskError> {
    for event in events {
        let DaemonEvent::Chat(stage) = event? else {
            continue;
        };
        if stage.ticket != ticket {
            continue;
        }
        observe(&stage);
        match stage.kind {
            ChatStageKind::Working(..) => {}
            ChatStageKind::Replied(reply) => return Ok(reply),
            ChatStageKind::Failed { code, message } => {
                return Err(AskError::Failed { code, message });
            }
        }
    }
    Err(AskError::DaemonStopped(ticket))
}

/// How to settle an ask that names two projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clarify {
    /// Ask aloud and record one answer, ending it the same way as the ask.
    Aloud { stop_mode: StopMode },
    /// Refuse: nobody is listening for a question.
    Refuse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routing {
    Ready {
        request: ChatRequest,
        /// Set when the text named a project: the one to keep for later asks.
        focus: Option<Focus>,
    },
    /// The question or the answer was stopped before it ended.
    Stopped,
}

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error(transparent)]
    Projects(#[from] ProjectsError),

    #[error(transparent)]
    Request(#[from] ChatRequestError),

    #[error("the ask names {}", .0.names().join(" and "))]
    Ambiguous(Ambiguous),

    #[error("heard {heard:?}, not {}", candidates.names().join(" or "))]
    Unresolved {
        heard: String,
        candidates: Ambiguous,
    },

    #[error("the question cannot be spoken")]
    Question(#[from] SpeechTextError),

    #[error(transparent)]
    Call(#[from] ClientError),
}

#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error("{}", code.reason(message))]
    Failed { code: RpcErrorCode, message: String },

    #[error("the daemon stopped before answering ticket {0}")]
    DaemonStopped(Ticket),

    #[error(transparent)]
    Stream(#[from] ClientError),
}

pub struct ListenStarted {
    pub already_listening: bool,
    pub transcript_path: String,
}

pub struct ListenStopped {
    pub was_listening: bool,
    pub transcript_path: String,
}

pub struct ListenSnapshot {
    pub listening: bool,
    pub transcript_path: String,
    pub error: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ListenCommandError {
    #[error("cannot resolve the output path {}", .0.display())]
    UnresolvableOutput(PathBuf),

    #[error("nothing is listening")]
    NotListening,

    #[error(transparent)]
    Call(ClientError),
}

fn classify_wake_word_status(status: ServingStatus) -> WakeWordStatus {
    match status {
        ServingStatus::Serving => WakeWordStatus::Listening,
        ServingStatus::NotServing => WakeWordStatus::Degraded,
        ServingStatus::Unknown | ServingStatus::ServiceUnknown => WakeWordStatus::Disabled,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeWordStatus {
    Listening,
    Degraded,
    Disabled,
    Stopped,
}

#[derive(Debug, thiserror::Error)]
pub enum WakeWordStatusError {
    #[error("failed to prepare the daemon runtime paths")]
    Runtime(#[source] RuntimeError),

    #[error(transparent)]
    Connect(#[from] ConnectError),

    #[error("failed to query wake-word status")]
    Query(#[source] ClientError),
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error(transparent)]
    Connect(#[from] ConnectError),

    #[error("failed to prepare the daemon runtime paths")]
    Runtime(#[source] RuntimeError),

    #[error(
        "no Aria daemon is running, and on {} a transcript survives only while the process that copied it does; start `aria daemon` first",
        std::env::consts::OS
    )]
    DaemonRequired,

    #[error("no Aria daemon is running; start `aria daemon` first")]
    NotRunning,

    #[error("failed to construct the temporary Aria daemon")]
    Build(#[source] DaemonError),

    #[error("the temporary Aria daemon did not survive startup")]
    Daemon(#[source] DaemonFailure),

    #[error("Aria daemon at {} accepted a connection but did not report readiness", socket.display())]
    Readiness {
        socket: PathBuf,
        #[source]
        source: ClientError,
    },

    #[error("Aria daemon did not become responsive at {}", socket.display())]
    StartupTimeout {
        socket: PathBuf,
        #[source]
        last_error: ConnectError,
    },
}

fn start_temporary(
    daemon: SpawnedRuntime,
    paths: &PreparedRuntimePaths,
    deadline: Instant,
) -> Result<DaemonSession, StartupError> {
    match await_spawned(paths, deadline) {
        Ok(client) => Ok(DaemonSession::temporary(client, daemon)),
        Err(error) => match daemon.stop() {
            Ok(()) => Err(error),
            Err(failure) => Err(StartupError::Daemon(failure)),
        },
    }
}

/// Waits for the daemon this call just spawned. It owns the socket already, so
/// both a refused connection and an unready answer mean startup in progress —
/// unlike `connect_to_required`, where an absent daemon never arrives.
fn await_spawned(
    paths: &PreparedRuntimePaths,
    deadline: Instant,
) -> Result<DaemonClient, StartupError> {
    loop {
        let pending = match probe_daemon(paths.socket_path()) {
            Ok(Liveness::Ready(client)) => return Ok(client),
            Ok(Liveness::NotRunning(error)) => ProbeError::Connect(error),
            Ok(Liveness::Starting(error)) => ProbeError::Readiness(error),
            Err(error) => return Err(probe_error(paths, error)),
        };

        if Instant::now() >= deadline {
            return Err(gave_up(paths, pending));
        }
        thread::sleep(CONNECT_RETRY_INTERVAL);
    }
}

/// Connects where a press may not start the daemon it needs, so an absent
/// daemon is an instruction to the user rather than something to wait for.
fn connect_to_required(paths: &PreparedRuntimePaths) -> Result<DaemonClient, StartupError> {
    connect_existing(paths).map_err(|error| match error {
        StartupError::NotRunning => StartupError::DaemonRequired,
        other => other,
    })
}

fn connect_existing(paths: &PreparedRuntimePaths) -> Result<DaemonClient, StartupError> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;

    loop {
        let pending = match probe_daemon(paths.socket_path()) {
            Ok(Liveness::Ready(client)) => return Ok(client),
            Ok(Liveness::NotRunning(_)) => return Err(StartupError::NotRunning),
            Ok(Liveness::Starting(error)) => ProbeError::Readiness(error),
            Err(error) => return Err(probe_error(paths, error)),
        };

        if Instant::now() >= deadline {
            return Err(gave_up(paths, pending));
        }
        thread::sleep(CONNECT_RETRY_INTERVAL);
    }
}

fn gave_up(paths: &PreparedRuntimePaths, error: ProbeError) -> StartupError {
    match error {
        ProbeError::Connect(last_error) => StartupError::StartupTimeout {
            socket: paths.socket_path().to_path_buf(),
            last_error,
        },
        ProbeError::Readiness(_) => probe_error(paths, error),
    }
}

fn probe_error(paths: &PreparedRuntimePaths, error: ProbeError) -> StartupError {
    match error {
        ProbeError::Connect(error) => error.into(),
        ProbeError::Readiness(source) => StartupError::Readiness {
            socket: paths.socket_path().to_path_buf(),
            source,
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::chat::Step;
    use crate::config::AssistantId;
    use crate::daemon::Daemon;
    use crate::runtime::RuntimePaths;
    use crate::speech::SpeechStage;

    use super::*;

    #[test]
    fn a_daemon_without_a_wake_word_reports_it_disabled() {
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        // On demand, so it serves without a model and never monitors a wake word.
        let daemon = Daemon::builder()
            .runtime_dir(directory.path())
            .lifetime(DaemonLifetime::OnDemand)
            .open()
            .expect("the daemon should configure")
            .spawn()
            .expect("the daemon should spawn");
        let paths = RuntimePaths::in_directory(directory.path().to_path_buf())
            .prepare()
            .expect("the runtime directory should be usable");

        let status = DaemonSession::wake_word_status_at(&paths);

        assert!(
            matches!(status, Ok(WakeWordStatus::Disabled)),
            "a daemon with no wake word should say so, got {status:?}"
        );
        let readiness = DaemonClient::connect(paths.socket_path())
            .expect("the daemon accepts")
            .check_service_ready(WAKE_WORD_HEALTH_SERVICE);
        assert!(
            matches!(readiness, Err(ClientError::NotProvided)),
            "waiting will not bring the wake word up, got {readiness:?}"
        );
        daemon.stopper().stop();
        daemon
            .join()
            .expect("the stopped daemon should exit cleanly");
    }

    fn chat(ticket: u64, kind: ChatStageKind) -> Result<DaemonEvent, ClientError> {
        Ok(DaemonEvent::Chat(ChatStage {
            ticket: Ticket(ticket),
            assistant_id: AssistantId::Codex,
            kind,
        }))
    }

    #[test]
    fn a_pending_ask_shows_its_own_stages_and_returns_its_reply() {
        let mine = ChatReply {
            text: "one".to_owned(),
            delivery_error: None,
            copied: false,
        };
        let events = vec![
            chat(2, ChatStageKind::Working(Step::Thinking)),
            chat(1, ChatStageKind::Working(Step::Thinking)),
            chat(1, ChatStageKind::Working(Step::Reading)),
            Ok(DaemonEvent::Speech(SpeechStage::Spoken)),
            chat(1, ChatStageKind::Replied(mine.clone())),
            chat(2, ChatStageKind::Replied(mine.clone())),
        ];
        let mut seen = Vec::new();

        let reply = reply_from(Ticket(1), events, |stage| seen.push(stage.kind.clone()));

        assert_eq!(reply.expect("the reply arrives"), mine);
        assert_eq!(
            seen,
            [
                ChatStageKind::Working(Step::Thinking),
                ChatStageKind::Working(Step::Reading),
                ChatStageKind::Replied(mine)
            ]
        );
    }

    #[test]
    fn a_stream_that_ends_first_is_the_daemon_stopping() {
        let reply = reply_from(
            Ticket(1),
            [chat(1, ChatStageKind::Working(Step::Thinking))],
            |_| {},
        );

        assert!(matches!(reply, Err(AskError::DaemonStopped(Ticket(1)))));
    }

    #[test]
    fn wake_health_has_one_domain_status_per_wire_status() {
        assert_eq!(
            classify_wake_word_status(ServingStatus::Serving),
            WakeWordStatus::Listening
        );
        assert_eq!(
            classify_wake_word_status(ServingStatus::NotServing),
            WakeWordStatus::Degraded
        );
        assert_eq!(
            classify_wake_word_status(ServingStatus::Unknown),
            WakeWordStatus::Disabled
        );
        assert_eq!(
            classify_wake_word_status(ServingStatus::ServiceUnknown),
            WakeWordStatus::Disabled
        );
    }
}
