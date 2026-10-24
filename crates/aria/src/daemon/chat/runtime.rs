//! One ask, start to finish: the agent named by the request, the
//! conversation it continues, and the reply's optional delivery. Ordering
//! between asks is the actor's job; here only the record file is shared.

use std::path::PathBuf;
use std::sync::Arc;

use std::time::Duration;
use tokio::sync::mpsc;

use aria_kit::cancel::CancelToken;

use crate::chat::{
    Agent, AgentError, AskTarget, CLAUDE_BINARY, CODEX_BINARY, ChatError, ChatEventEmitter,
    ChatReply, ChatRequest, ChatStage, ChatStageKind, ChatSubscribers, ClaudeCodeAgent, CodexAgent,
    Projects, PromptCall, PromptRequest, PromptRun, ResolveError, SessionSpec, SessionStore, Step,
    Ticket, TurnSink, read_instructions,
};
use crate::config::{ChatAgent, ChatConfig, Environment, MemoryConfig, SpeakerConfig};
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

/// Resume ids outlive the daemon and a reboot, like the agents' own history.
const SESSION_FILE: &str = "chat-session.json";

type AgentFactory = Box<dyn Fn(ChatAgent) -> Box<dyn Agent> + Send + Sync>;

pub(in crate::daemon) enum ChatJob {
    Ask(ChatRequest),
    Prompt(PromptRequest),
}

impl ChatJob {
    pub(in crate::daemon) fn agent(&self) -> ChatAgent {
        match self {
            Self::Ask(request) => request.agent,
            Self::Prompt(request) => request.agent,
        }
    }
}

pub(in crate::daemon) struct ChatRuntime {
    emitter: ChatEventEmitter,
    config_environment: Environment,
    sessions: SessionStore,
    agents: AgentFactory,
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
        Self::with_agents(
            context.config_environment.clone(),
            context.preferences_path.with_file_name(SESSION_FILE),
            event_emitter(context, subscriptions),
            build_agents(context.config_environment.codex_home()),
            preferences,
            memory_wake,
        )
    }

    fn with_agents(
        config_environment: Environment,
        session_path: PathBuf,
        emitter: ChatEventEmitter,
        agents: AgentFactory,
        preferences: Arc<PreferenceStore>,
        memory_wake: Option<mpsc::Sender<()>>,
    ) -> Self {
        Self {
            emitter,
            config_environment,
            sessions: SessionStore::open(session_path),
            agents,
            preferences,
            memory_wake,
        }
    }

    /// Runs one job to its end. Everything a caller learns arrives as an
    /// event under `ticket`.
    pub(in crate::daemon) fn execute(
        &self,
        ticket: Ticket,
        job: ChatJob,
        cancellation: CancelToken,
    ) {
        let agent = job.agent();
        if cancellation.is_cancelled() {
            self.cancel_unstarted(ticket, agent);
            return;
        }
        let mut recorder = TurnRecorder::new(&self.emitter, ticket, agent);
        recorder.step(Step::Thinking);
        let outcome = match job {
            ChatJob::Ask(request) => self.ask(&mut recorder, request, &cancellation),
            ChatJob::Prompt(request) => self.run_prompt(request, &cancellation),
        };
        let kind = match outcome {
            Ok(kind) => kind,
            Err(error) => failed(&error),
        };
        self.emitter.emit(ChatStage {
            ticket,
            agent,
            kind,
        });
    }

    /// The prompt and agent as stored now; a later edit applies to the
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
        let config = ChatConfig::load(&self.config_environment).map_err(ChatError::Config)?;
        Ok(PromptRequest {
            text: call.text,
            kind: prompt.kind(),
            file: prompt.file().to_path_buf(),
            agent: config.agent,
            application: call.application,
        })
    }

    /// Ends an ask that never ran, the way a cancelled run ends.
    pub(in crate::daemon) fn cancel_unstarted(&self, ticket: Ticket, agent: ChatAgent) {
        self.emitter.emit(ChatStage {
            ticket,
            agent,
            kind: failed(&ChatError::Agent(AgentError::Cancelled)),
        });
    }

    /// Where an ask goes for a caller with no working directory: the
    /// project in focus, else the one configuration pins. Reading the focus
    /// file and scanning the roots touches the disk, so callers run it off
    /// the reactor.
    pub(in crate::daemon) fn resolve_target(&self) -> Result<AskTarget, ResolveError> {
        let config = ChatConfig::load(&self.config_environment).map_err(ResolveError::Config)?;
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
                "the agent's working directory is not a directory",
            )));
        }
        Ok(AskTarget {
            workdir,
            agent: config.agent,
        })
    }

    fn ask(
        &self,
        recorder: &mut TurnRecorder<'_>,
        request: ChatRequest,
        cancel: &CancelToken,
    ) -> Result<ChatStageKind, ChatError> {
        let config = ChatConfig::load(&self.config_environment).map_err(ChatError::Config)?;
        if !request.workdir.is_dir() {
            return Err(ChatError::Agent(AgentError::NoWorkdir(request.workdir)));
        }
        // The target is bound before the agent runs, so a refusal costs
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
        let text = self.turn(recorder, &request, &prompt, config.timeout, cancel)?;
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
    /// instructions are all the agent sees. The target is bound before the
    /// agent runs, so speech meant for an app with Aria off, or for an app
    /// the user has since left, is never sent.
    fn run_prompt(
        &self,
        request: PromptRequest,
        cancel: &CancelToken,
    ) -> Result<ChatStageKind, ChatError> {
        let config = ChatConfig::load(&self.config_environment).map_err(ChatError::Config)?;
        let delivery = Delivery::prepare(
            OutputMode::InsertOrCopy,
            self.target_policy(request.application.as_ref())?,
            config.delivery_timeout,
            cancel,
        )?;
        let text = self.prompt_answer(&request, config.timeout, cancel)?;
        if cancel.is_cancelled() {
            return Err(ChatError::Agent(AgentError::Cancelled));
        }
        let delivered = delivery.deliver(&text, cancel);
        Ok(replied(text, Some(delivered)))
    }

    fn prompt_answer(
        &self,
        request: &PromptRequest,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<String, ChatError> {
        let instructions = read_instructions(&request.file)?;
        let run = PromptRun {
            kind: request.kind,
            instructions: &instructions,
            transcript: &request.text,
        };
        Ok(self
            .agent(request.agent)
            .run_prompt(&run, cancel, timeout)?)
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
    /// prompts the agent, and records what continues it next time.
    fn turn(
        &self,
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

        let mut session = self.agent(request.agent).open(SessionSpec {
            workdir: &request.workdir,
            model: request.model.as_deref(),
            resume: resume.as_deref(),
        });
        let prompted = session.prompt(prompt, recorder, cancel, timeout);
        // The agent may have deleted the session it was asked to resume.
        // Kept, its id would fail every later ask on this key.
        if resume.is_some() && matches!(prompted, Err(AgentError::Failed(_))) {
            self.sessions.forget(&key)?;
        }
        prompted?;
        if let Some(resume) = session.resume_id() {
            self.sessions.record(key, resume)?;
        }
        Ok(std::mem::take(&mut recorder.text))
    }

    fn agent(&self, kind: ChatAgent) -> Box<dyn Agent> {
        (self.agents)(kind)
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
    agent: ChatAgent,
}

impl<'a> TurnRecorder<'a> {
    fn new(emitter: &'a ChatEventEmitter, ticket: Ticket, agent: ChatAgent) -> Self {
        Self {
            text: String::new(),
            last: None,
            emitter,
            ticket,
            agent,
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
            agent: self.agent,
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

fn build_agents(codex_home: PathBuf) -> AgentFactory {
    Box::new(move |agent| match agent {
        ChatAgent::ClaudeCode => Box::new(ClaudeCodeAgent::new(CLAUDE_BINARY.into())),
        ChatAgent::Codex => Box::new(CodexAgent::new(CODEX_BINARY.into(), codex_home.clone())),
    })
}

/// A reply's delivery, bound to its target before the agent runs.
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
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::mpsc::{Sender, channel};

    use crate::chat::PromptKind;
    use crate::chat::Session;

    use super::*;

    /// Records what it was asked and answers with a fresh id each turn.
    #[derive(Default)]
    pub(in crate::daemon::chat) struct Log {
        pub(in crate::daemon::chat) sessions: Vec<Option<String>>,
        pub(in crate::daemon::chat) prompts: Vec<String>,
    }

    pub(in crate::daemon::chat) struct FakeAgent(pub(in crate::daemon::chat) Arc<Mutex<Log>>);

    impl Agent for FakeAgent {
        fn open(&self, spec: SessionSpec<'_>) -> Box<dyn Session> {
            Box::new(FakeSession {
                log: Arc::clone(&self.0),
                resume: spec.resume.map(str::to_owned),
            })
        }

        fn run_prompt(
            &self,
            request: &PromptRun<'_>,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<String, AgentError> {
            let mut log = self.0.lock().expect("the log is not poisoned");
            log.prompts.push(request.prompt());
            Ok("Polished.".to_owned())
        }
    }

    struct FakeSession {
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
        ) -> Result<(), AgentError> {
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

    pub(in crate::daemon::chat) fn runtime_with(
        directory: &Path,
        agents: AgentFactory,
    ) -> (ChatRuntime, std::sync::mpsc::Receiver<ChatStage>) {
        let (runtime, received, _wake) = runtime_waking(directory, agents);
        (runtime, received)
    }

    fn runtime_waking(
        directory: &Path,
        agents: AgentFactory,
    ) -> (
        ChatRuntime,
        std::sync::mpsc::Receiver<ChatStage>,
        mpsc::Receiver<()>,
    ) {
        let (events, received) = channel();
        let (wake, woken) = mpsc::channel(1);
        let runtime = ChatRuntime::with_agents(
            Environment::in_home(directory.to_path_buf()),
            directory.join(SESSION_FILE),
            ChatEventEmitter::subscribed_by("aria-test-chat-events", move |stage| {
                let _ = events.send(stage);
            }),
            agents,
            Arc::new(
                PreferenceStore::open(directory.join("preferences.json"))
                    .expect("test preferences"),
            ),
            Some(wake),
        );
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
            agent: ChatAgent::ClaudeCode,
            application: None,
        }
    }

    #[test]
    fn a_prompt_request_names_only_a_stored_prompt_that_is_on() {
        use crate::preferences::{PreferencePatch, Prompt, PromptId, Prompts};

        let home = tempfile::tempdir().expect("a temp home");
        let (runtime, _events) = runtime_with(home.path(), logging_agents(&Arc::default()));
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
    fn a_prompt_run_for_an_app_no_longer_in_front_never_reaches_the_agent() {
        let home = tempfile::tempdir().expect("a temp home");
        let file = home.path().join("polish.md");
        std::fs::write(&file, "Fix grammar.").expect("the prompt file");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, _events) = runtime_with(home.path(), logging_agents(&log));
        let request = PromptRequest {
            application: Some(ApplicationId::new("com.example.left-behind").expect("id")),
            ..prompt_request(file)
        };

        let error = runtime
            .run_prompt(request, &CancelToken::never())
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
        let (runtime, _events) = runtime_with(home.path(), logging_agents(&log));

        let text = runtime
            .prompt_answer(
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
    fn a_prompt_run_without_its_file_never_reaches_the_agent() {
        let home = tempfile::tempdir().expect("a temp home");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, _events) = runtime_with(home.path(), logging_agents(&log));

        let error = runtime
            .prompt_answer(
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
        let (runtime, events) = runtime_with(home.path(), logging_agents(&log));

        run(&runtime, &events, 1, request("hello", home.path(), false));

        assert_eq!(log.lock().unwrap().prompts, ["hello"]);
        assert!(!home.path().join(".aria/memory").exists());
    }

    #[test]
    fn memory_on_shows_the_profile_and_keeps_a_typed_ask_out() {
        let home = home_with_memory();
        enroll_owner(home.path());
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events, mut woken) = runtime_waking(home.path(), logging_agents(&log));
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
        let (runtime, events, mut woken) = runtime_waking(home.path(), logging_agents(&log));
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
        let (runtime, events, mut woken) = runtime_waking(home.path(), logging_agents(&log));
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
        let (runtime, events) =
            runtime_with(home.path(), Box::new(|_agent| Box::new(FailingAgent)));
        let heard = ChatRequest {
            own_voice: true,
            ..request("hello", home.path(), false)
        };

        let kind = run(&runtime, &events, 1, heard.clone());

        assert!(matches!(kind, ChatStageKind::Failed { .. }));
        assert!(turns(home.path(), &heard).is_empty());
    }

    struct FailingAgent;

    impl Agent for FailingAgent {
        fn open(&self, _spec: SessionSpec<'_>) -> Box<dyn Session> {
            Box::new(FailingSession)
        }

        fn run_prompt(
            &self,
            _request: &PromptRun<'_>,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<String, AgentError> {
            Err(AgentError::Failed("down".to_owned()))
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
        ) -> Result<(), AgentError> {
            Err(AgentError::Failed("down".to_owned()))
        }

        fn resume_id(&self) -> Option<&str> {
            None
        }
    }

    pub(in crate::daemon::chat) fn logging_agents(log: &Arc<Mutex<Log>>) -> AgentFactory {
        let log = Arc::clone(log);
        Box::new(move |_agent| Box::new(FakeAgent(Arc::clone(&log))))
    }

    pub(in crate::daemon::chat) fn request(text: &str, workdir: &Path, new: bool) -> ChatRequest {
        ChatRequest {
            text: text.to_owned(),
            workdir: workdir.to_path_buf(),
            agent: ChatAgent::ClaudeCode,
            model: None,
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
        let agent = request.agent;
        runtime.execute(Ticket(ticket), ChatJob::Ask(request), CancelToken::never());
        let mut steps = Vec::new();
        loop {
            let stage = events.recv().expect("a stage arrives");
            assert_eq!(stage.ticket, Ticket(ticket));
            assert_eq!(stage.agent, agent);
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

    fn recorded(directory: &Path) -> String {
        std::fs::read_to_string(directory.join(SESSION_FILE)).expect("a record exists")
    }

    #[test]
    fn an_unstarted_ask_ends_as_cancelled_under_its_ticket() {
        let directory = tempfile::tempdir().expect("test directory");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(directory.path(), logging_agents(&log));

        runtime.cancel_unstarted(Ticket(9), ChatAgent::Codex);

        let stage = events.recv().expect("the refusal arrives");
        assert_eq!(stage.ticket, Ticket(9));
        assert_eq!(stage.agent, ChatAgent::Codex);
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
        let (runtime, events) = runtime_with(work, logging_agents(&log));

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
    fn conversations_are_kept_per_agent_and_directory() {
        let directory = tempfile::tempdir().expect("test directory");
        let work = directory.path();
        let elsewhere = tempfile::tempdir().expect("another directory");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(work, logging_agents(&log));

        run(&runtime, &events, 1, request("one", work, false));
        run(
            &runtime,
            &events,
            2,
            request("two", elsewhere.path(), false),
        );
        let mut codex = request("three", work, false);
        codex.agent = ChatAgent::Codex;
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
        let (runtime, events) = runtime_with(work, logging_agents(&log));
        run(&runtime, &events, 1, request("one", work, false));
        run(
            &runtime,
            &events,
            2,
            request("two", elsewhere.path(), false),
        );
        let (failing, failing_events) = runtime_with(
            work,
            Box::new(|_agent| Box::new(ClaudeCodeAgent::new("/nonexistent/claude".into()))),
        );

        let outcome = run(&failing, &failing_events, 3, request("three", work, true));

        assert!(
            matches!(outcome, ChatStageKind::Failed { .. }),
            "{outcome:?}"
        );
        let record = recorded(work);
        assert!(!record.contains("session-1"), "{record}");
        assert!(record.contains("session-2"), "{record}");
    }

    #[test]
    fn a_cancelled_ask_never_starts() {
        let directory = tempfile::tempdir().expect("test directory");
        let work = directory.path();
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(work, logging_agents(&log));
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
            logging_agents(&Arc::new(Mutex::new(Log::default()))),
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
            logging_agents(&Arc::new(Mutex::new(Log::default()))),
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
            logging_agents(&Arc::new(Mutex::new(Log::default()))),
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
            logging_agents(&Arc::new(Mutex::new(Log::default()))),
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
            logging_agents(&Arc::new(Mutex::new(Log::default()))),
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
        let (runtime, events) = runtime_with(directory.path(), logging_agents(&log));

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
        let (runtime, events) = runtime_with(
            work,
            Box::new(|_agent| Box::new(ClaudeCodeAgent::new("/nonexistent/claude".into()))),
        );

        let outcome = run(&runtime, &events, 1, request("hi", work, false));

        match outcome {
            ChatStageKind::Failed { code, message } => {
                assert_eq!(code, RpcErrorCode::AgentUnavailable);
                assert!(message.contains("/nonexistent/claude"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// An agent that answers only when told to, so a test can hold an ask
    /// open and watch what the actor does with the next one.
    pub(in crate::daemon::chat) struct ParkedAgent {
        pub(in crate::daemon::chat) started: Sender<u64>,
        pub(in crate::daemon::chat) release: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
    }

    impl Agent for ParkedAgent {
        fn open(&self, _spec: SessionSpec<'_>) -> Box<dyn Session> {
            Box::new(ParkedSession {
                started: self.started.clone(),
                release: Arc::clone(&self.release),
                resume: None,
            })
        }

        fn run_prompt(
            &self,
            _request: &PromptRun<'_>,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<String, AgentError> {
            Err(AgentError::Failed(
                "a parked agent only takes turns".to_owned(),
            ))
        }
    }

    struct ParkedSession {
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
        ) -> Result<(), AgentError> {
            let turn: u64 = text.parse().expect("prompts are turn numbers");
            let _ = self.started.send(turn);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if cancel.is_cancelled() {
                    return Err(AgentError::Cancelled);
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
