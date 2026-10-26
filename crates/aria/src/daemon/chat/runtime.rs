//! One ask, start to finish: the assistant named by the request, the
//! conversation it continues, and the reply's optional delivery. Ordering
//! between asks is the actor's job; here only the record file is shared.

use std::sync::Arc;

use std::time::Duration;
use tokio::sync::mpsc;

use aria_kit::cancel::CancelToken;

use crate::chat::{
    AskTarget, Assistant, AssistantError, AssistantModel, CLAUDE_BINARY, CODEX_BINARY, ChatError,
    ChatEventEmitter, ChatReply, ChatRequest, ChatStage, ChatStageKind, ChatSubscribers,
    ClaudeCodeAssistant, CodexAssistant, Projects, PromptCall, PromptRequest, PromptRun,
    ResolveError, Session, SessionSpec, SessionStore, Step, Ticket, TurnSink, read_instructions,
};
use crate::config::{AssistantId, ChatConfig, Environment, MemoryConfig, SpeakerConfig};
use crate::delivery::{
    DeliveryError, DeliveryOutcome, OutputMode, PlatformTextDelivery, TargetPolicy, TextDelivery,
};
use crate::memory::{DB_FILE, Store, load_profile_block};
use crate::preferences::{ApplicationId, PreferenceStore};
use crate::primitives::error_chain;
use crate::rpc::RpcErrorCode;
use crate::speaker::{ProfileStore, Role};

use super::super::context::Context;
use super::super::subscriptions::Subscriptions;

/// Resume ids outlive the daemon and a reboot, like the assistants' own history.
const SESSION_FILE: &str = "chat-session.json";

pub(in crate::daemon) enum ChatJob {
    Ask(ChatRequest),
    Prompt(PromptRequest),
}

impl ChatJob {
    pub(in crate::daemon) fn assistant_id(&self) -> AssistantId {
        match self {
            Self::Ask(request) => request.assistant_id,
            Self::Prompt(request) => request.assistant_id,
        }
    }
}

pub(in crate::daemon) struct ChatRuntime {
    emitter: ChatEventEmitter,
    config_environment: Environment,
    sessions: SessionStore,
    #[cfg(test)]
    assistant_overrides: Option<tests::AssistantOverrides>,
    /// Read per ask, so a reply never types into an app with Aria off.
    preferences: Arc<PreferenceStore>,
    /// Nudges the memory backlog after a turn worth remembering. `None`
    /// in a daemon that runs no backlog.
    memory_wake: Option<mpsc::Sender<()>>,
}

impl ChatRuntime {
    /// Reads the config file per ask, using the daemon's captured environment.
    /// A broken chat section fails asks without preventing daemon startup.
    pub(in crate::daemon) fn new(
        context: &Context<'_>,
        subscriptions: Arc<Subscriptions>,
        preferences: Arc<PreferenceStore>,
        memory_wake: Option<mpsc::Sender<()>>,
    ) -> Self {
        Self {
            emitter: event_emitter(context, subscriptions),
            config_environment: context.config_environment.clone(),
            sessions: SessionStore::open(context.preferences_path.with_file_name(SESSION_FILE)),
            preferences,
            memory_wake,
            #[cfg(test)]
            assistant_overrides: None,
        }
    }

    pub(in crate::daemon) fn models(
        &self,
        assistant_id: AssistantId,
        cancel: &CancelToken,
    ) -> Result<Vec<AssistantModel>, AssistantError> {
        #[cfg(test)]
        if let Some(overrides) = &self.assistant_overrides {
            return (overrides.models)(assistant_id, cancel);
        }
        match assistant_id {
            AssistantId::ClaudeCode => {
                ClaudeCodeAssistant::new(CLAUDE_BINARY.into()).models(cancel)
            }
            AssistantId::Codex => {
                CodexAssistant::new(CODEX_BINARY.into(), self.config_environment.codex_home())
                    .models(cancel)
            }
        }
    }

    pub(in crate::daemon) fn prepare_ask(
        &self,
        mut request: ChatRequest,
    ) -> Result<ChatRequest, ChatError> {
        if request.model_id.is_none() {
            request.model_id = self
                .preferences
                .snapshot()
                .map_err(ChatError::Preferences)?
                .preferences
                .model_id(request.assistant_id)
                .map(str::to_owned);
        }
        Ok(request)
    }

    /// Runs one job to its end. Everything a caller learns arrives as an
    /// event under `ticket`.
    pub(in crate::daemon) fn execute(
        &self,
        ticket: Ticket,
        job: ChatJob,
        cancellation: CancelToken,
    ) {
        #[cfg(test)]
        if let Some(overrides) = &self.assistant_overrides {
            (overrides.execute)(self, ticket, job, cancellation);
            return;
        }
        match job.assistant_id() {
            AssistantId::ClaudeCode => self.execute_with(
                &ClaudeCodeAssistant::new(CLAUDE_BINARY.into()),
                ticket,
                job,
                cancellation,
            ),
            AssistantId::Codex => self.execute_with(
                &CodexAssistant::new(CODEX_BINARY.into(), self.config_environment.codex_home()),
                ticket,
                job,
                cancellation,
            ),
        }
    }

    fn execute_with<A: Assistant>(
        &self,
        assistant: &A,
        ticket: Ticket,
        job: ChatJob,
        cancellation: CancelToken,
    ) {
        let assistant_id = job.assistant_id();
        if cancellation.is_cancelled() {
            self.cancel_unstarted(ticket, assistant_id);
            return;
        }
        let mut recorder = TurnRecorder::new(&self.emitter, ticket, assistant_id);
        recorder.step(Step::Thinking);
        let outcome = match job {
            ChatJob::Ask(request) => self.ask(assistant, &mut recorder, request, &cancellation),
            ChatJob::Prompt(request) => self.run_prompt(assistant, request, &cancellation),
        };
        let kind = match outcome {
            Ok(kind) => kind,
            Err(error) => failed(&error),
        };
        self.emitter.emit(ChatStage {
            ticket,
            assistant_id,
            kind,
        });
    }

    /// The prompt and assistant as stored now; a later edit applies to the
    /// next run.
    pub(in crate::daemon) fn prompt_request(
        &self,
        call: PromptCall,
    ) -> Result<PromptRequest, ChatError> {
        let snapshot = self
            .preferences
            .snapshot()
            .map_err(ChatError::Preferences)?;
        let prompt = snapshot
            .preferences
            .prompts
            .find(call.prompt.as_str())
            .ok_or(ChatError::UnknownPrompt)?;
        if !prompt.enabled() {
            return Err(ChatError::PromptOff);
        }
        let assistant_id = snapshot
            .preferences
            .resolve_assistant_id(&self.config_environment)
            .map_err(ChatError::Config)?;
        Ok(PromptRequest {
            text: call.text,
            kind: prompt.kind(),
            file: prompt.file().to_path_buf(),
            assistant_id,
            model_id: snapshot
                .preferences
                .model_id(assistant_id)
                .map(str::to_owned),
            application: call.application,
        })
    }

    /// Ends an ask that never ran, the way a cancelled run ends.
    pub(in crate::daemon) fn cancel_unstarted(&self, ticket: Ticket, assistant_id: AssistantId) {
        self.emitter.emit(ChatStage {
            ticket,
            assistant_id,
            kind: failed(&ChatError::Assistant(AssistantError::Cancelled)),
        });
    }

    /// Where an ask goes for a caller with no working directory: the
    /// project in focus, else the one configuration pins. Reading the focus
    /// file and scanning the roots touches the disk, so callers run it off
    /// the reactor.
    pub(in crate::daemon) fn resolve_target(&self) -> Result<AskTarget, ResolveError> {
        let snapshot = self
            .preferences
            .snapshot()
            .map_err(ResolveError::Preferences)?;
        let assistant_id = snapshot
            .preferences
            .resolve_assistant_id(&self.config_environment)
            .map_err(ResolveError::Config)?;
        let config = ChatConfig::load(&self.config_environment, assistant_id)
            .map_err(ResolveError::Config)?;
        let projects = Projects::load_with(&self.config_environment, &config)
            .map_err(ResolveError::Projects)?;
        let workdir = match projects.focused() {
            Some(project) => project.path.clone(),
            None => config.workdir.ok_or(ResolveError::NoTarget)?,
        };
        let workdir = workdir.canonicalize().map_err(ResolveError::Directory)?;
        if !workdir
            .metadata()
            .map_err(ResolveError::Directory)?
            .is_dir()
        {
            return Err(ResolveError::Directory(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "the assistant's working directory is not a directory",
            )));
        }
        Ok(AskTarget {
            workdir,
            assistant_id: config.assistant_id,
        })
    }

    fn ask<A: Assistant>(
        &self,
        assistant: &A,
        recorder: &mut TurnRecorder<'_>,
        request: ChatRequest,
        cancel: &CancelToken,
    ) -> Result<ChatStageKind, ChatError> {
        let config = ChatConfig::load(&self.config_environment, request.assistant_id)
            .map_err(ChatError::Config)?;
        if !request.workdir.is_dir() {
            return Err(ChatError::Assistant(AssistantError::NoWorkdir(
                request.workdir,
            )));
        }
        // The target is bound before the assistant runs, so a refusal costs
        // nothing and a window switch during the run is caught at delivery.
        let (delivery, refused) = match request.output_mode {
            None => (None, None),
            Some(mode) => {
                match Delivery::prepare(
                    mode,
                    self.target_policy(None)?,
                    config.delivery_timeout,
                    cancel,
                ) {
                    Ok(delivery) => (Some(delivery), None),
                    // The reply still reaches the caller, just not typed there.
                    Err(ChatError::Delivery(DeliveryError::ApplicationExcluded)) => {
                        (None, Some(RpcErrorCode::ApplicationExcluded))
                    }
                    Err(error) => return Err(error),
                }
            }
        };

        let memory = self.memory();
        let prompt = match memory
            .as_ref()
            .and_then(|memory| load_profile_block(&memory.dir))
        {
            Some(block) => format!("{block}\n\n{}", request.text),
            None => request.text.clone(),
        };
        let text = self.turn(
            assistant,
            recorder,
            &request,
            &prompt,
            config.timeout,
            cancel,
        )?;
        if let Some(memory) = &memory
            && request.own_voice
        {
            self.remember(memory, &request);
        }

        let delivered = match (refused, delivery) {
            (Some(code), _) => Some(Err(code)),
            (None, Some(delivery)) => Some(delivery.deliver(&text, cancel)),
            (None, None) => None,
        };
        Ok(replied(text, delivered))
    }

    /// No memory, conversation, or project: the transcript and the user's
    /// instructions are all the assistant sees. The target is bound before the
    /// assistant runs, so speech meant for an app with Aria off, or for an app
    /// the user has since left, is never sent.
    fn run_prompt<A: Assistant>(
        &self,
        assistant: &A,
        request: PromptRequest,
        cancel: &CancelToken,
    ) -> Result<ChatStageKind, ChatError> {
        let config = ChatConfig::load(&self.config_environment, request.assistant_id)
            .map_err(ChatError::Config)?;
        let delivery = Delivery::prepare(
            OutputMode::InsertOrCopy,
            self.target_policy(request.application.as_ref())?,
            config.delivery_timeout,
            cancel,
        )?;
        let text = self.prompt_answer(assistant, &request, config.timeout, cancel)?;
        if cancel.is_cancelled() {
            return Err(ChatError::Assistant(AssistantError::Cancelled));
        }
        let delivered = delivery.deliver(&text, cancel);
        Ok(replied(text, Some(delivered)))
    }

    fn prompt_answer<A: Assistant>(
        &self,
        assistant: &A,
        request: &PromptRequest,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<String, ChatError> {
        let instructions = read_instructions(&request.file)?;
        let run = PromptRun {
            model_id: request.model_id.as_deref(),
            kind: request.kind,
            instructions: &instructions,
            transcript: &request.text,
        };
        Ok(assistant.run_prompt(&run, cancel, timeout)?)
    }

    /// Refuses an app with Aria off and, when given, any app but `application`.
    fn target_policy(
        &self,
        application: Option<&ApplicationId>,
    ) -> Result<TargetPolicy, ChatError> {
        let overrides = self
            .preferences
            .snapshot()
            .map_err(ChatError::Preferences)?
            .preferences
            .application_overrides;
        Ok(TargetPolicy {
            overridden_for: application.map(|id| id.as_str().to_owned()),
            excluded: overrides.excluded_ids(),
        })
    }

    /// One turn on the request's conversation: continues or restarts it,
    /// prompts the assistant, and records what continues it next time.
    fn turn<A: Assistant>(
        &self,
        assistant: &A,
        recorder: &mut TurnRecorder<'_>,
        request: &ChatRequest,
        prompt: &str,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<String, ChatError> {
        let key = request.key();
        // Forgetting happens before the ask, so a failed one cannot
        // resurrect the session the user asked to drop.
        let resume = if request.new {
            self.sessions.forget(&key)?;
            None
        } else {
            self.sessions.recall(&key)?
        };

        let mut session = assistant.open(SessionSpec {
            workdir: &request.workdir,
            model_id: request.model_id.as_deref(),
            resume: resume.as_deref(),
        });
        let prompted = session.prompt(prompt, recorder, cancel, timeout);
        // The assistant may have deleted the session it was asked to resume.
        // Kept, its id would fail every later ask on this key.
        if resume.is_some() && matches!(prompted, Err(AssistantError::Failed(_))) {
            self.sessions.forget(&key)?;
        }
        prompted?;
        if let Some(resume) = session.resume_id() {
            self.sessions.record(key, resume)?;
        }
        Ok(std::mem::take(&mut recorder.text))
    }

    /// The memory section when it is on. One that does not resolve costs
    /// memory, not the ask.
    fn memory(&self) -> Option<MemoryConfig> {
        match MemoryConfig::load(&self.config_environment) {
            Ok(config) if config.enabled => Some(config),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    error = %error_chain(&error),
                    "memory configuration unreadable; the ask runs without memory"
                );
                None
            }
        }
    }

    /// Keeps what the owner said, for extraction later. Only a recording
    /// the own-voice gate judged reaches here, and only an enrolled owner
    /// can be the one it judged for.
    fn remember(&self, memory: &MemoryConfig, request: &ChatRequest) {
        let Some(owner) = self.owner_name() else {
            tracing::warn!("no owner is enrolled; the ask is not remembered");
            return;
        };
        let session = request.key().to_string();
        let recorded = Store::open(&memory.dir.join(DB_FILE))
            .and_then(|store| store.add_turn(&session, &request.text, &owner));
        if let Err(error) = recorded {
            tracing::warn!(error = %error_chain(&error), "chat turn not remembered");
            return;
        }
        // A full slot means a nudge is already waiting; one is enough.
        if let Some(wake) = &self.memory_wake {
            let _ = wake.try_send(());
        }
    }

    fn owner_name(&self) -> Option<String> {
        let config = match SpeakerConfig::load(&self.config_environment) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(error = %error_chain(&error), "speaker configuration unreadable");
                return None;
            }
        };
        crate::speaker::usable_profiles(ProfileStore::open(&config.speakers_dir).read_all())
            .into_iter()
            .find(|profile| profile.role == Role::Owner)
            .map(|profile| profile.name.as_str().to_owned())
    }
}

/// Collects the reply and relays each change of step as a `Working` stage.
struct TurnRecorder<'a> {
    text: String,
    last: Option<Step>,
    emitter: &'a ChatEventEmitter,
    ticket: Ticket,
    assistant_id: AssistantId,
}

impl<'a> TurnRecorder<'a> {
    fn new(emitter: &'a ChatEventEmitter, ticket: Ticket, assistant_id: AssistantId) -> Self {
        Self {
            text: String::new(),
            last: None,
            emitter,
            ticket,
            assistant_id,
        }
    }
}

impl TurnSink for TurnRecorder<'_> {
    fn text(&mut self, chunk: &str) {
        self.text.push_str(chunk);
    }

    fn step(&mut self, step: Step) {
        if self.last.as_ref() == Some(&step) {
            return;
        }
        self.emitter.emit(ChatStage {
            ticket: self.ticket,
            assistant_id: self.assistant_id,
            kind: ChatStageKind::Working(step.clone()),
        });
        self.last = Some(step);
    }
}

fn failed(error: &ChatError) -> ChatStageKind {
    ChatStageKind::Failed {
        code: error.into(),
        message: error_chain(error),
    }
}

/// A reply's delivery, bound to its target before the assistant runs.
struct Delivery {
    policy: TargetPolicy,
    platform: PlatformTextDelivery,
    target: <PlatformTextDelivery as TextDelivery>::Prepared,
}

impl Delivery {
    fn prepare(
        mode: OutputMode,
        policy: TargetPolicy,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<Self, ChatError> {
        policy.confirm_front().map_err(ChatError::Delivery)?;
        let platform = PlatformTextDelivery::new(timeout).map_err(ChatError::Delivery)?;
        Ok(Self {
            target: platform
                .prepare(mode, || cancel.is_cancelled())
                .map_err(ChatError::Delivery)?,
            policy,
            platform,
        })
    }

    /// Types the reply at the target; a failure is reported, not fatal,
    /// because the reply itself still reaches the caller.
    fn deliver(self, text: &str, cancel: &CancelToken) -> Result<DeliveryOutcome, RpcErrorCode> {
        let text = self.platform.caret(&self.target).insert(text);
        match self
            .policy
            .confirm_front()
            .and_then(|()| {
                self.platform
                    .deliver(&self.target, &text, || cancel.is_cancelled())
            })
            .and_then(|outcome| {
                if outcome == DeliveryOutcome::Inserted {
                    self.platform
                        .finish(&self.target, || cancel.is_cancelled())?;
                }
                Ok(outcome)
            }) {
            Ok(outcome) => Ok(outcome),
            Err(failed) => {
                tracing::warn!(error = %error_chain(&failed), "the reply was not delivered");
                Err(RpcErrorCode::from(&ChatError::Delivery(failed)))
            }
        }
    }
}

/// A reply, and what became of its delivery when one was asked for.
fn replied(
    text: String,
    delivered: Option<Result<DeliveryOutcome, RpcErrorCode>>,
) -> ChatStageKind {
    ChatStageKind::Replied(ChatReply {
        text,
        delivery_error: delivered.and_then(Result::err),
        copied: matches!(delivered, Some(Ok(DeliveryOutcome::Copied))),
    })
}

/// Nobody hears an unattended run, so it subscribes no sound. A connected
/// client asked to be told, and still is.
fn event_emitter(context: &Context<'_>, subscriptions: Arc<Subscriptions>) -> ChatEventEmitter {
    let local = context.attended.then_some(ChatSubscribers);
    ChatEventEmitter::subscribed_by(crate::chat::EVENT_THREAD, move |stage| {
        if let Some(local) = &local {
            local.observe(&stage);
        }
        subscriptions.broadcast(stage);
    })
}

#[cfg(test)]
pub(super) mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::mpsc::{Sender, channel};

    use crate::chat::PromptKind;

    use super::*;

    type ExecuteAssistant = dyn Fn(&ChatRuntime, Ticket, ChatJob, CancelToken) + Send + Sync;
    type DiscoverModels = dyn Fn(AssistantId, &CancelToken) -> Result<Vec<AssistantModel>, AssistantError>
        + Send
        + Sync;

    pub(super) struct AssistantOverrides {
        pub(super) execute: Box<ExecuteAssistant>,
        pub(super) models: Box<DiscoverModels>,
    }

    /// Records what it was asked and answers with a fresh id each turn.
    #[derive(Default)]
    pub(in crate::daemon::chat) struct Log {
        pub(in crate::daemon::chat) sessions: Vec<Option<String>>,
        pub(in crate::daemon::chat) prompts: Vec<String>,
    }

    pub(in crate::daemon::chat) struct FakeAssistant(pub(in crate::daemon::chat) Arc<Mutex<Log>>);

    impl Assistant for FakeAssistant {
        type Session = FakeSession;

        fn models(&self, _cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
            Ok(Vec::new())
        }

        fn open(&self, spec: SessionSpec<'_>) -> Self::Session {
            FakeSession {
                log: Arc::clone(&self.0),
                resume: spec.resume.map(str::to_owned),
            }
        }

        fn run_prompt(
            &self,
            request: &PromptRun<'_>,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<String, AssistantError> {
            let mut log = self.0.lock().expect("the log is not poisoned");
            log.prompts.push(request.prompt());
            Ok("Polished.".to_owned())
        }
    }

    #[tokio::test]
    async fn request_drop_and_shutdown_cancel_model_discovery() {
        use crate::daemon::host::DaemonLifetime;
        use crate::daemon::service::ChatRpc;
        use crate::ipc::wire::{self, chat_service_server::ChatService};

        for disconnect in [true, false] {
            let directory = tempfile::tempdir().expect("test directory");
            let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
            let log = Arc::new(Mutex::new(Log::default()));
            let (mut runtime, _events) = runtime_with(directory.path(), logging_assistants(&log));
            runtime.assistant_overrides.as_mut().unwrap().models = Box::new(move |_, cancel| {
                started.send(cancel.clone()).expect("discovery started");
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(AssistantError::Cancelled)
            });
            let runtime = Arc::new(runtime);
            let (handle, _actor) = crate::daemon::chat::channel(Arc::clone(&runtime));
            let shutdown = tokio_util::sync::CancellationToken::new();
            let rpc = ChatRpc::new(
                handle,
                runtime,
                DaemonLifetime::Persistent,
                shutdown.clone(),
            );
            let request = tokio::spawn(async move {
                rpc.list_models(tonic::Request::new(wire::ListAssistantModelsRequest {
                    assistant_id: wire::AssistantId::Codex.into(),
                }))
                .await
            });
            let cancel = tokio::time::timeout(Duration::from_secs(1), observed.recv())
                .await
                .expect("discovery starts")
                .expect("discovery token");
            if disconnect {
                request.abort();
                assert!(request.await.expect_err("request aborted").is_cancelled());
            } else {
                shutdown.cancel();
                assert!(cancel.is_cancelled());
                assert!(request.await.expect("request finishes").is_err());
            }
            assert!(cancel.is_cancelled());
        }
    }

    pub(in crate::daemon::chat) struct FakeSession {
        log: Arc<Mutex<Log>>,
        resume: Option<String>,
    }

    impl Session for FakeSession {
        fn prompt(
            &mut self,
            text: &str,
            sink: &mut dyn TurnSink,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<(), AssistantError> {
            let mut log = self.log.lock().expect("the log is not poisoned");
            log.sessions.push(self.resume.clone());
            log.prompts.push(text.to_owned());
            let turn = log.sessions.len();
            self.resume = Some(format!("session-{turn}"));
            sink.step(Step::Thinking);
            sink.step(Step::Reading);
            sink.step(Step::Reading);
            sink.step(Step::Searching);
            sink.text(&format!("reply {turn}"));
            Ok(())
        }

        fn resume_id(&self) -> Option<&str> {
            self.resume.as_deref()
        }
    }

    pub(in crate::daemon::chat) fn runtime_with<A: Assistant + 'static>(
        directory: &Path,
        assistant_factory: impl Fn(AssistantId) -> A + Send + Sync + 'static,
    ) -> (ChatRuntime, std::sync::mpsc::Receiver<ChatStage>) {
        let (runtime, received, _wake) = runtime_waking(directory, assistant_factory);
        (runtime, received)
    }

    fn runtime_waking<A: Assistant + 'static>(
        directory: &Path,
        assistant_factory: impl Fn(AssistantId) -> A + Send + Sync + 'static,
    ) -> (
        ChatRuntime,
        std::sync::mpsc::Receiver<ChatStage>,
        mpsc::Receiver<()>,
    ) {
        let (events, received) = channel();
        let (wake, woken) = mpsc::channel(1);
        let assistant_factory = Arc::new(assistant_factory);
        let for_execution = Arc::clone(&assistant_factory);
        let runtime = ChatRuntime {
            config_environment: Environment::in_home(directory.to_path_buf()),
            sessions: SessionStore::open(directory.join(SESSION_FILE)),
            emitter: ChatEventEmitter::subscribed_by("aria-test-chat-events", move |stage| {
                let _ = events.send(stage);
            }),
            assistant_overrides: Some(AssistantOverrides {
                execute: Box::new(move |runtime, ticket, job, cancel| {
                    runtime.execute_with(&for_execution(job.assistant_id()), ticket, job, cancel);
                }),
                models: Box::new(move |assistant_id, cancel| {
                    assistant_factory(assistant_id).models(cancel)
                }),
            }),
            preferences: Arc::new(
                PreferenceStore::open(directory.join("preferences.json"))
                    .expect("test preferences"),
            ),
            memory_wake: Some(wake),
        };
        (runtime, received, woken)
    }

    /// A home whose memory section is on, with a profile block written.
    fn home_with_memory() -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("a temp home");
        std::fs::create_dir_all(home.path().join(".aria/memory")).expect("the memory directory");
        std::fs::write(
            home.path().join(".aria/config.yaml"),
            "memory:\n  enabled: true\n",
        )
        .expect("the config file");
        std::fs::write(
            home.path().join(".aria/memory/profile.md"),
            "## About the user\n- Is Sam\n",
        )
        .expect("the profile");
        home
    }

    fn turns(home: &Path, request: &ChatRequest) -> Vec<(String, String)> {
        let store = Store::open(&home.join(".aria/memory").join(DB_FILE)).expect("opens");
        store
            .turns(&request.key().to_string(), 0, u32::MAX)
            .expect("reads")
            .into_iter()
            .map(|turn| (turn.text, turn.speaker))
            .collect()
    }

    fn enroll_owner(home: &Path) {
        crate::speaker::ProfileStore::open(home.join(".aria/speakers"))
            .save(
                &aria_kit::speaker::enrolled_voice("sam", Role::Owner, vec![1.0]),
                crate::speaker::Replace::Never,
            )
            .expect("the owner enrolls");
    }

    fn prompt_request(file: PathBuf) -> PromptRequest {
        PromptRequest {
            text: "um send it friday".to_owned(),
            kind: PromptKind::Rewrite,
            file,
            assistant_id: AssistantId::ClaudeCode,
            model_id: None,
            application: None,
        }
    }

    #[test]
    fn ask_models_are_bound_at_admission_and_request_overrides_win() {
        use crate::preferences::PreferencePatch;
        let home = tempfile::tempdir().unwrap();
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&Arc::default()));
        runtime
            .preferences
            .update(
                0,
                PreferencePatch {
                    assistant_models: [
                        (AssistantId::ClaudeCode, "claude-first".to_owned()),
                        (AssistantId::Codex, "codex-first".to_owned()),
                    ]
                    .into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let admitted = runtime
            .prepare_ask(request("hello", home.path(), false))
            .unwrap();
        runtime
            .preferences
            .update(
                1,
                PreferencePatch {
                    assistant_models: [(AssistantId::ClaudeCode, "claude-later".to_owned())].into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(admitted.model_id.as_deref(), Some("claude-first"));
        let explicit = runtime
            .prepare_ask(ChatRequest {
                assistant_id: AssistantId::Codex,
                model_id: Some("request-model".to_owned()),
                ..request("hello", home.path(), false)
            })
            .unwrap();
        assert_eq!(explicit.model_id.as_deref(), Some("request-model"));
        let codex = runtime
            .prepare_ask(ChatRequest {
                assistant_id: AssistantId::Codex,
                ..request("hello", home.path(), false)
            })
            .unwrap();
        assert_eq!(codex.model_id.as_deref(), Some("codex-first"));
        assert_eq!(runtime.preferences.snapshot().unwrap().revision, 2);
    }

    #[test]
    fn prompts_and_ask_targets_share_the_saved_assistant() {
        use crate::preferences::{PreferencePatch, Prompt, PromptId, Prompts};

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".aria")).unwrap();
        std::fs::write(
            home.path().join(".aria/config.yaml"),
            "chat:\n  assistant: invalid\n  workdir: '~'\n",
        )
        .unwrap();
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&Arc::default()));
        let prompt = Prompt::new(
            PromptId::new("polish").unwrap(),
            "Polish".to_owned(),
            PromptKind::Rewrite,
            home.path().join("polish.md"),
            true,
            None,
        )
        .unwrap();
        runtime
            .preferences
            .update(
                0,
                PreferencePatch {
                    assistant_id: Some(AssistantId::Codex),
                    assistant_models: [
                        (AssistantId::ClaudeCode, "claude-choice".to_owned()),
                        (AssistantId::Codex, "codex-choice".to_owned()),
                    ]
                    .into(),
                    prompts: Some(Prompts::new(vec![prompt]).unwrap()),
                    ..Default::default()
                },
            )
            .unwrap();
        let call = || PromptCall {
            text: "hi".to_owned(),
            prompt: PromptId::new("polish").unwrap(),
            application: None,
        };
        assert_eq!(
            runtime.prompt_request(call()).unwrap().assistant_id,
            AssistantId::Codex
        );
        assert_eq!(
            runtime.resolve_target().unwrap().assistant_id,
            AssistantId::Codex
        );
        let admitted = runtime.prompt_request(call()).unwrap();
        assert_eq!(admitted.model_id.as_deref(), Some("codex-choice"));
        runtime
            .preferences
            .update(
                1,
                PreferencePatch {
                    assistant_id: Some(AssistantId::ClaudeCode),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            runtime.prompt_request(call()).unwrap().assistant_id,
            AssistantId::ClaudeCode
        );
        assert_eq!(
            runtime.resolve_target().unwrap().assistant_id,
            AssistantId::ClaudeCode
        );
        assert_eq!(admitted.assistant_id, AssistantId::Codex);
        assert_eq!(admitted.model_id.as_deref(), Some("codex-choice"));
        assert_eq!(
            runtime.prompt_request(call()).unwrap().model_id.as_deref(),
            Some("claude-choice")
        );
    }

    #[test]
    fn a_prompt_request_names_only_a_stored_prompt_that_is_on() {
        use crate::preferences::{PreferencePatch, Prompt, PromptId, Prompts};

        let home = tempfile::tempdir().expect("a temp home");
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&Arc::default()));
        let prompt = |id: &str, enabled| {
            Prompt::new(
                PromptId::new(id).expect("id"),
                id.to_owned(),
                PromptKind::Rewrite,
                home.path().join(format!("{id}.md")),
                enabled,
                None,
            )
            .expect("prompt")
        };
        let prompts =
            Prompts::new(vec![prompt("on", true), prompt("off", false)]).expect("prompts");
        runtime
            .preferences
            .update(
                0,
                PreferencePatch {
                    prompts: Some(prompts),
                    ..PreferencePatch::default()
                },
            )
            .expect("stored");

        let call = |id: &str| PromptCall {
            text: "hi".to_owned(),
            prompt: PromptId::new(id).expect("id"),
            application: None,
        };
        let request = runtime
            .prompt_request(call("on"))
            .expect("an enabled prompt");
        assert_eq!(request.file, home.path().join("on.md"));
        assert!(matches!(
            runtime.prompt_request(call("off")),
            Err(ChatError::PromptOff)
        ));
        assert!(matches!(
            runtime.prompt_request(call("gone")),
            Err(ChatError::UnknownPrompt)
        ));
    }

    #[test]
    fn a_prompt_run_for_an_app_no_longer_in_front_never_reaches_the_assistant() {
        let home = tempfile::tempdir().expect("a temp home");
        let file = home.path().join("polish.md");
        std::fs::write(&file, "Fix grammar.").expect("the prompt file");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&log));
        let request = PromptRequest {
            application: Some(ApplicationId::new("com.example.left-behind").expect("id")),
            ..prompt_request(file)
        };

        let error = runtime
            .run_prompt(
                &FakeAssistant(Arc::clone(&log)),
                request,
                &CancelToken::never(),
            )
            .expect_err("the app the chord was pressed in is not in front");

        assert!(matches!(
            error,
            ChatError::Delivery(DeliveryError::TargetChanged)
        ));
        assert!(log.lock().unwrap().prompts.is_empty(), "nothing was sent");
    }

    #[test]
    fn a_prompt_run_hands_one_fresh_run_the_instructions_then_the_transcript() {
        let home = home_with_memory();
        let instructions = home.path().join("polish.md");
        std::fs::write(&instructions, "Fix grammar.").expect("the prompt file");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&log));

        let text = runtime
            .prompt_answer(
                &FakeAssistant(Arc::clone(&log)),
                &prompt_request(instructions),
                Duration::from_secs(5),
                &CancelToken::never(),
            )
            .expect("the run succeeds");

        assert_eq!(text, "Polished.");
        let log = log.lock().unwrap();
        assert!(log.sessions.is_empty(), "no conversation was opened");
        let [prompt] = log.prompts.as_slice() else {
            panic!("one run: {:?}", log.prompts);
        };
        assert!(prompt.starts_with("Fix grammar."), "{prompt}");
        assert!(prompt.contains("um send it friday"), "{prompt}");
        assert!(!prompt.contains("Is Sam"), "memory stays out: {prompt}");
    }

    #[test]
    fn a_prompt_run_without_its_file_never_reaches_the_assistant() {
        let home = tempfile::tempdir().expect("a temp home");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&log));

        let error = runtime
            .prompt_answer(
                &FakeAssistant(Arc::clone(&log)),
                &prompt_request(home.path().join("moved.md")),
                Duration::from_secs(5),
                &CancelToken::never(),
            )
            .expect_err("the file is missing");

        assert!(matches!(error, ChatError::Instructions(_)));
        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::InvalidRequest);
        assert!(log.lock().unwrap().prompts.is_empty());
    }

    #[test]
    fn memory_off_records_nothing_and_changes_no_prompt() {
        let home = tempfile::tempdir().expect("a temp home");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(home.path(), logging_assistants(&log));

        run(&runtime, &events, 1, request("hello", home.path(), false));

        assert_eq!(log.lock().unwrap().prompts, ["hello"]);
        assert!(!home.path().join(".aria/memory").exists());
    }

    #[test]
    fn memory_on_shows_the_profile_and_keeps_a_typed_ask_out() {
        let home = home_with_memory();
        enroll_owner(home.path());
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events, mut woken) = runtime_waking(home.path(), logging_assistants(&log));
        let typed = request("hello", home.path(), false);

        run(&runtime, &events, 1, typed.clone());

        assert_eq!(
            log.lock().unwrap().prompts,
            ["## About the user\n- Is Sam\n\nhello"]
        );
        assert!(turns(home.path(), &typed).is_empty());
        assert!(woken.try_recv().is_err());
    }

    #[test]
    fn an_ask_in_the_owners_voice_is_kept_and_wakes_the_backlog() {
        let home = home_with_memory();
        enroll_owner(home.path());
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events, mut woken) = runtime_waking(home.path(), logging_assistants(&log));
        let heard = ChatRequest {
            own_voice: true,
            ..request("hello", home.path(), false)
        };

        run(&runtime, &events, 1, heard.clone());

        assert_eq!(
            turns(home.path(), &heard),
            [("hello".to_owned(), "sam".to_owned())]
        );
        assert!(woken.try_recv().is_ok());
    }

    #[test]
    fn without_an_enrolled_owner_nothing_is_kept() {
        let home = home_with_memory();
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events, mut woken) = runtime_waking(home.path(), logging_assistants(&log));
        let heard = ChatRequest {
            own_voice: true,
            ..request("hello", home.path(), false)
        };

        run(&runtime, &events, 1, heard.clone());

        assert!(turns(home.path(), &heard).is_empty());
        assert!(woken.try_recv().is_err());
    }

    #[test]
    fn a_failed_turn_is_not_kept() {
        let home = home_with_memory();
        enroll_owner(home.path());
        let (runtime, events) = runtime_with(home.path(), |_assistant_id| FailingAssistant);
        let heard = ChatRequest {
            own_voice: true,
            ..request("hello", home.path(), false)
        };

        let kind = run(&runtime, &events, 1, heard.clone());

        assert!(matches!(kind, ChatStageKind::Failed { .. }));
        assert!(turns(home.path(), &heard).is_empty());
    }

    struct FailingAssistant;

    impl Assistant for FailingAssistant {
        type Session = FailingSession;

        fn models(&self, _cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
            Err(AssistantError::Failed("down".to_owned()))
        }

        fn open(&self, _spec: SessionSpec<'_>) -> Self::Session {
            FailingSession
        }

        fn run_prompt(
            &self,
            _request: &PromptRun<'_>,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<String, AssistantError> {
            Err(AssistantError::Failed("down".to_owned()))
        }
    }

    struct FailingSession;

    impl Session for FailingSession {
        fn prompt(
            &mut self,
            _text: &str,
            _sink: &mut dyn TurnSink,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<(), AssistantError> {
            Err(AssistantError::Failed("down".to_owned()))
        }

        fn resume_id(&self) -> Option<&str> {
            None
        }
    }

    pub(in crate::daemon::chat) fn logging_assistants(
        log: &Arc<Mutex<Log>>,
    ) -> impl Fn(AssistantId) -> FakeAssistant + Send + Sync + 'static {
        let log = Arc::clone(log);
        move |_assistant_id| FakeAssistant(Arc::clone(&log))
    }

    pub(in crate::daemon::chat) fn request(text: &str, workdir: &Path, new: bool) -> ChatRequest {
        ChatRequest {
            text: text.to_owned(),
            workdir: workdir.to_path_buf(),
            assistant_id: AssistantId::ClaudeCode,
            model_id: None,
            session: None,
            new,
            output_mode: None,
            own_voice: false,
        }
    }

    fn run(
        runtime: &ChatRuntime,
        events: &std::sync::mpsc::Receiver<ChatStage>,
        ticket: u64,
        request: ChatRequest,
    ) -> ChatStageKind {
        let assistant_id = request.assistant_id;
        runtime.execute(Ticket(ticket), ChatJob::Ask(request), CancelToken::never());
        let mut steps = Vec::new();
        loop {
            let stage = events.recv().expect("a stage arrives");
            assert_eq!(stage.ticket, Ticket(ticket));
            assert_eq!(stage.assistant_id, assistant_id);
            match stage.kind {
                ChatStageKind::Working(step) => steps.push(step),
                ended => {
                    assert!(steps.starts_with(&[Step::Thinking]), "{steps:?}");
                    if steps.len() > 1 {
                        // The fake steps thinking, reading twice, searching:
                        // repeats collapse, the opening thinking is not resent.
                        assert_eq!(steps, [Step::Thinking, Step::Reading, Step::Searching]);
                    }
                    return ended;
                }
            }
        }
    }

    fn replied(kind: ChatStageKind) -> String {
        match kind {
            ChatStageKind::Replied(reply) => reply.text,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn an_explicit_ask_assistant_does_not_change_the_shared_choice() {
        let directory = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Log::default()));
        let assistant_factory = move |assistant_id| {
            assert_eq!(assistant_id, AssistantId::ClaudeCode);
            FakeAssistant(Arc::clone(&log))
        };
        let (runtime, events) = runtime_with(directory.path(), assistant_factory);
        runtime
            .preferences
            .update(
                0,
                crate::preferences::PreferencePatch {
                    assistant_id: Some(AssistantId::Codex),
                    ..Default::default()
                },
            )
            .unwrap();

        let reply = run(
            &runtime,
            &events,
            1,
            request("hello", directory.path(), false),
        );

        assert_eq!(replied(reply), "reply 1");
        let snapshot = runtime.preferences.snapshot().unwrap();
        assert_eq!(snapshot.preferences.assistant_id, Some(AssistantId::Codex));
        assert_eq!(snapshot.revision, 1);
    }

    #[test]
    fn an_unstarted_ask_ends_as_cancelled_under_its_ticket() {
        let directory = tempfile::tempdir().expect("test directory");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(directory.path(), logging_assistants(&log));

        runtime.cancel_unstarted(Ticket(9), AssistantId::Codex);

        let stage = events.recv().expect("the refusal arrives");
        assert_eq!(stage.ticket, Ticket(9));
        assert_eq!(stage.assistant_id, AssistantId::Codex);
        assert!(matches!(
            stage.kind,
            ChatStageKind::Failed {
                code: RpcErrorCode::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn turns_chain_until_a_new_conversation_is_asked_for() {
        let directory = tempfile::tempdir().expect("test directory");
        let work = directory.path();
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(work, logging_assistants(&log));

        let first = run(&runtime, &events, 1, request("one", work, false));
        run(&runtime, &events, 2, request("two", work, false));
        run(&runtime, &events, 3, request("three", work, true));

        assert_eq!(replied(first), "reply 1");
        assert_eq!(
            log.lock().expect("not poisoned").sessions,
            [None, Some("session-1".to_owned()), None],
            "the second turn continues the first; `new` forgets it"
        );
    }

    #[test]
    fn conversations_are_kept_per_assistant_and_directory() {
        let directory = tempfile::tempdir().expect("test directory");
        let work = directory.path();
        let elsewhere = tempfile::tempdir().expect("another directory");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(work, logging_assistants(&log));

        run(&runtime, &events, 1, request("one", work, false));
        run(
            &runtime,
            &events,
            2,
            request("two", elsewhere.path(), false),
        );
        let mut codex = request("three", work, false);
        codex.assistant_id = AssistantId::Codex;
        run(&runtime, &events, 3, codex);
        run(&runtime, &events, 4, request("four", work, false));
        run(
            &runtime,
            &events,
            5,
            request("five", elsewhere.path(), false),
        );

        assert_eq!(
            log.lock().expect("not poisoned").sessions,
            [
                None,
                None,
                None,
                Some("session-1".to_owned()),
                Some("session-2".to_owned())
            ],
            "each key continues its own conversation"
        );
    }

    #[test]
    fn forgetting_drops_only_the_asked_conversation_even_when_the_ask_fails() {
        let directory = tempfile::tempdir().expect("test directory");
        let work = directory.path();
        let elsewhere = tempfile::tempdir().expect("another directory");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(work, logging_assistants(&log));
        run(&runtime, &events, 1, request("one", work, false));
        run(
            &runtime,
            &events,
            2,
            request("two", elsewhere.path(), false),
        );
        let (failing, failing_events) = runtime_with(work, |_assistant_id| {
            ClaudeCodeAssistant::new("/nonexistent/claude".into())
        });

        let outcome = run(&failing, &failing_events, 3, request("three", work, true));

        assert!(
            matches!(outcome, ChatStageKind::Failed { .. }),
            "{outcome:?}"
        );
        let record = std::fs::read_to_string(work.join(SESSION_FILE)).expect("a record exists");
        assert!(!record.contains("session-1"), "{record}");
        assert!(record.contains("session-2"), "{record}");
    }

    #[test]
    fn a_cancelled_ask_never_starts() {
        let directory = tempfile::tempdir().expect("test directory");
        let work = directory.path();
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(work, logging_assistants(&log));
        let cancel = CancelToken::new();
        cancel.cancel();

        runtime.execute(Ticket(1), ChatJob::Ask(request("hi", work, false)), cancel);

        let ended = events.recv().expect("the end arrives");
        assert!(
            matches!(
                ended.kind,
                ChatStageKind::Failed {
                    code: RpcErrorCode::Cancelled,
                    ..
                }
            ),
            "{ended:?}"
        );
        assert!(log.lock().expect("not poisoned").prompts.is_empty());
    }

    use std::os::unix::fs::PermissionsExt;

    /// A home with a config file, a project root holding one git repo, and
    /// optionally that repo in focus.
    fn home_with_project(focused: bool) -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().expect("a temp home");
        std::fs::create_dir(home.path().join(".aria")).expect("the aria directory");
        let roots = home.path().join("code");
        let repo = roots.join("Writ");
        std::fs::create_dir_all(repo.join(".git")).expect("a repo");
        // Discovery canonicalizes, and a temp dir on macOS is a symlink, so
        // a focus written raw would name a directory discovery never saw.
        let repo = repo.canonicalize().expect("the repo resolves");
        std::fs::write(
            home.path().join(".aria/config.yaml"),
            format!("chat:\n  project_roots: [{}]\n", roots.display()),
        )
        .expect("the config file");
        if focused {
            let focus = home.path().join(".aria/project");
            std::fs::write(&focus, repo.to_string_lossy().as_bytes()).expect("the focus file");
            // The reader refuses a focus file others could write.
            std::fs::set_permissions(&focus, std::fs::Permissions::from_mode(0o600))
                .expect("a private focus file");
        }
        (home, repo)
    }

    #[test]
    fn resolving_takes_the_project_in_focus() {
        let (home, repo) = home_with_project(true);
        let (runtime, _events) = runtime_with(
            home.path(),
            logging_assistants(&Arc::new(Mutex::new(Log::default()))),
        );

        let target = runtime.resolve_target().expect("the focus resolves");

        assert_eq!(target.workdir, repo);
    }

    #[test]
    fn resolving_falls_back_to_the_configured_directory() {
        let home = tempfile::tempdir().expect("a temp home");
        std::fs::create_dir(home.path().join(".aria")).expect("the aria directory");
        let pinned = home.path().join("pinned");
        std::fs::create_dir(&pinned).expect("the pinned directory");
        std::fs::write(
            home.path().join(".aria/config.yaml"),
            format!("chat:\n  workdir: {}\n", pinned.display()),
        )
        .expect("the config file");
        let (runtime, _events) = runtime_with(
            home.path(),
            logging_assistants(&Arc::new(Mutex::new(Log::default()))),
        );

        let target = runtime
            .resolve_target()
            .expect("the configured directory resolves");

        assert_eq!(target.workdir, pinned.canonicalize().unwrap());
    }

    #[test]
    fn resolving_a_relative_directory_produces_an_absolute_recording_context() {
        let (home, _) = home_with_project(false);
        std::fs::write(
            home.path().join(".aria/config.yaml"),
            "chat:\n  workdir: .\n",
        )
        .unwrap();
        let (runtime, _events) = runtime_with(
            home.path(),
            logging_assistants(&Arc::new(Mutex::new(Log::default()))),
        );

        let target = runtime.resolve_target().unwrap();

        assert_eq!(
            target.workdir,
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        assert!(crate::vocabulary::Directory::new(target.workdir).is_ok());
    }

    #[test]
    fn resolving_a_file_as_a_working_directory_is_an_invalid_request() {
        let (home, _) = home_with_project(false);
        std::fs::write(
            home.path().join(".aria/config.yaml"),
            "chat:\n  workdir: ~/.aria/config.yaml\n",
        )
        .unwrap();
        let (runtime, _events) = runtime_with(
            home.path(),
            logging_assistants(&Arc::new(Mutex::new(Log::default()))),
        );

        let error = runtime.resolve_target().unwrap_err();

        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::InvalidRequest);
        assert!(matches!(error, ResolveError::Directory(_)));
    }

    #[test]
    fn resolving_with_nothing_to_go_on_says_so() {
        let home = tempfile::tempdir().expect("a temp home");
        std::fs::create_dir(home.path().join(".aria")).expect("the aria directory");
        let (runtime, _events) = runtime_with(
            home.path(),
            logging_assistants(&Arc::new(Mutex::new(Log::default()))),
        );

        let error = runtime
            .resolve_target()
            .expect_err("nothing names a target");

        assert!(matches!(error, ResolveError::NoTarget), "{error:?}");
    }

    #[test]
    fn a_missing_directory_is_not_a_missing_binary() {
        let directory = tempfile::tempdir().expect("test directory");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(directory.path(), logging_assistants(&log));

        let outcome = run(
            &runtime,
            &events,
            1,
            request("hi", Path::new("/nonexistent/repo"), false),
        );

        assert!(
            matches!(
                outcome,
                ChatStageKind::Failed {
                    code: RpcErrorCode::InvalidRequest,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_missing_binary_is_reported_as_unavailable_with_its_reason() {
        let directory = tempfile::tempdir().expect("test directory");
        let work = directory.path();
        let (runtime, events) = runtime_with(work, |_assistant_id| {
            ClaudeCodeAssistant::new("/nonexistent/claude".into())
        });

        let outcome = run(&runtime, &events, 1, request("hi", work, false));

        match outcome {
            ChatStageKind::Failed { code, message } => {
                assert_eq!(code, RpcErrorCode::AssistantUnavailable);
                assert!(message.contains("/nonexistent/claude"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// An assistant that answers only when told to, so a test can hold an ask
    /// open and watch what the actor does with the next one.
    pub(in crate::daemon::chat) struct ParkedAssistant {
        pub(in crate::daemon::chat) started: Sender<u64>,
        pub(in crate::daemon::chat) release: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
    }

    impl Assistant for ParkedAssistant {
        type Session = ParkedSession;

        fn models(&self, _cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
            Err(AssistantError::Failed(
                "a parked assistant only takes turns".to_owned(),
            ))
        }

        fn open(&self, _spec: SessionSpec<'_>) -> Self::Session {
            ParkedSession {
                started: self.started.clone(),
                release: Arc::clone(&self.release),
                resume: None,
            }
        }

        fn run_prompt(
            &self,
            _request: &PromptRun<'_>,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<String, AssistantError> {
            Err(AssistantError::Failed(
                "a parked assistant only takes turns".to_owned(),
            ))
        }
    }

    pub(in crate::daemon::chat) struct ParkedSession {
        started: Sender<u64>,
        release: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
        resume: Option<String>,
    }

    impl Session for ParkedSession {
        fn prompt(
            &mut self,
            text: &str,
            sink: &mut dyn TurnSink,
            cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<(), AssistantError> {
            let turn: u64 = text.parse().expect("prompts are turn numbers");
            let _ = self.started.send(turn);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if cancel.is_cancelled() {
                    return Err(AssistantError::Cancelled);
                }
                let released = self
                    .release
                    .lock()
                    .expect("not poisoned")
                    .recv_timeout(Duration::from_millis(20));
                if released.is_ok() || std::time::Instant::now() >= deadline {
                    break;
                }
            }
            self.resume = Some(format!("session-{turn}"));
            sink.text(&format!("reply {turn}"));
            Ok(())
        }

        fn resume_id(&self) -> Option<&str> {
            self.resume.as_deref()
        }
    }
}
