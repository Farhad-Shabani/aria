//! One chat, start to finish: the assistant named by the request, the
//! conversation it continues, and the reply's optional delivery. Ordering
//! between chats is the actor's job; here only the record file is shared.

use super::super::config::{load_chat, load_memory};
use super::super::context::Context;
use super::super::history::HistoryStore;
use super::super::subscriptions::Subscriptions;
use super::error::{ChatError, ResolveError};
use aria_assistant::chat::ChatConfig;
use aria_assistant::chat::{
    Ask, Assistant, AssistantError, AssistantModel, AssistantStatus, CLAUDE_BINARY, CODEX_BINARY,
    ChatEventEmitter, ChatReply, ChatRequest, ChatStage, ChatStageKind, ChatSubscribers,
    ChatTarget, ClaudeCodeAssistant, CodexAssistant, Conversations, DeliverCall, LocalAssistant,
    Projects, PromptCall, PromptChoice, PromptInput, PromptInstructions, PromptRequest, PromptRun,
    Session, SessionSpec, SessionStore, Step, StepSink, Ticket, read_instructions,
};
use aria_assistant::memory::MemoryConfig;
use aria_assistant::memory::{DB_FILE, Store, load_profile_block};
use aria_assistant::prompts::{self, PromptContents, PromptDraft, PromptSource, SaveError};
use aria_kit::cancel::CancelToken;
use aria_platform::delivery::{
    DeliveryError, DeliveryFactory, DeliveryOutcome, OutputMode, PlatformTextDelivery,
    TargetPolicy, TextDelivery,
};
use aria_primitives::error_chain;
use aria_provider::ProviderError;
use aria_settings::config::AssistantId;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_settings::{ApplicationId, Prompt, PromptId, PromptKind, SettingsCache, SettingsStore};
use aria_voice::config::SpeakerConfig;
use aria_voice::history::{HistoryContent, HistoryEntry, NewChat};
use aria_voice::speaker::{ProfileStore, Role};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Resume ids outlive the daemon and a reboot, like the assistants' own history.
const SESSION_FILE: &str = "chat-session.json";
/// How much of a conversation the local model rereads with each chat.
const RECALL_BYTES: usize = 32 * 1024;

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

    pub(in crate::daemon) fn tap_cancels(&self) -> bool {
        match self {
            Self::Ask(request) => request.tap_cancels,
            Self::Prompt(request) => request.tap_cancels,
        }
    }
}

pub(in crate::daemon) struct ChatRuntime {
    emitter: ChatEventEmitter,
    home: PathBuf,
    codex_home: PathBuf,
    sessions: SessionStore,
    #[cfg(test)]
    pub(in crate::daemon) assistant_overrides: Option<tests::AssistantOverrides>,
    /// Read per chat, so a reply never types into an app with Aria off.
    settings: Arc<SettingsStore>,
    /// Read per chat, so an edit applies to the next chat.
    cache: Arc<SettingsCache>,
    /// Nudges the memory backlog after a turn worth remembering. `None`
    /// in a daemon that runs no backlog.
    memory_wake: Option<mpsc::Sender<()>>,
    delivery: DeliveryFactory,
    typing: TypingTurns,
    /// Where chats and prompt results are saved, while history is on.
    history: Arc<HistoryStore>,
}

impl ChatRuntime {
    /// Resolves the assistant's settings per chat.
    pub(in crate::daemon) fn new(
        context: &Context<'_>,
        subscriptions: Arc<Subscriptions>,
        settings: Arc<SettingsStore>,
        cache: Arc<SettingsCache>,
        memory_wake: Option<mpsc::Sender<()>>,
        history: Arc<HistoryStore>,
    ) -> Self {
        Self {
            emitter: event_emitter(context, subscriptions),
            home: context.home.clone(),
            codex_home: context.codex_home.clone(),
            sessions: SessionStore::open(context.settings_path.with_file_name(SESSION_FILE)),
            settings,
            cache,
            memory_wake,
            delivery: context.delivery.clone(),
            typing: TypingTurns::default(),
            history,
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
                CodexAssistant::new(CODEX_BINARY.into(), self.codex_home.clone()).models(cancel)
            }
            AssistantId::LocalModel => self.local(None).models(cancel),
        }
    }

    /// The local assistant on the provider frozen at startup, with the model
    /// the settings name now. It recalls a conversation only from `history`.
    fn local<'a>(
        &self,
        history: Option<&'a HistoryStore>,
    ) -> LocalAssistant<SavedConversations<'a>> {
        let model = self.settings.snapshot().ok().and_then(|snapshot| {
            snapshot
                .settings
                .model_id(AssistantId::LocalModel)
                .map(str::to_owned)
        });
        LocalAssistant::new(
            self.cache.effective().choices().provider.as_ref(),
            model,
            SavedConversations { history },
        )
    }

    /// The history store while history is on. Read once as a run starts, so
    /// what a chat recalls and whether it is saved agree.
    fn enabled_history(&self) -> Result<Option<&HistoryStore>, ChatError> {
        let snapshot = self.settings.snapshot().map_err(ChatError::Settings)?;
        Ok(snapshot
            .settings
            .history_enabled
            .then_some(self.history.as_ref()))
    }

    pub(in crate::daemon) fn assistants(
        &self,
        cancel: &CancelToken,
    ) -> Vec<(AssistantId, AssistantStatus)> {
        AssistantId::ALL
            .iter()
            .map(|&assistant_id| (assistant_id, self.check_ready(assistant_id, cancel)))
            .collect()
    }

    fn check_ready(&self, assistant_id: AssistantId, cancel: &CancelToken) -> AssistantStatus {
        #[cfg(test)]
        if let Some(overrides) = &self.assistant_overrides {
            return (overrides.ready)(assistant_id, cancel);
        }
        match assistant_id {
            AssistantId::ClaudeCode => {
                ClaudeCodeAssistant::new(CLAUDE_BINARY.into()).check_ready(cancel)
            }
            AssistantId::Codex => CodexAssistant::new(CODEX_BINARY.into(), self.codex_home.clone())
                .check_ready(cancel),
            AssistantId::LocalModel => self.local(None).check_ready(cancel),
        }
    }

    pub(in crate::daemon) fn prepare_ask(
        &self,
        mut request: ChatRequest,
    ) -> Result<ChatRequest, ChatError> {
        if !request.workdir.is_dir() {
            return Err(ChatError::Assistant(AssistantError::NoWorkdir(
                request.workdir,
            )));
        }
        if request.model_id.is_none() {
            request.model_id = self
                .settings
                .snapshot()
                .map_err(ChatError::Settings)?
                .settings
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
                |_| ClaudeCodeAssistant::new(CLAUDE_BINARY.into()),
                ticket,
                job,
                cancellation,
            ),
            AssistantId::Codex => self.execute_with(
                |_| CodexAssistant::new(CODEX_BINARY.into(), self.codex_home.clone()),
                ticket,
                job,
                cancellation,
            ),
            AssistantId::LocalModel => {
                self.execute_with(|history| self.local(history), ticket, job, cancellation);
            }
        }
    }

    /// Builds the assistant from the history the run may recall from.
    fn execute_with<'a, A: Assistant>(
        &'a self,
        assistant: impl FnOnce(Option<&'a HistoryStore>) -> A,
        ticket: Ticket,
        job: ChatJob,
        cancellation: CancelToken,
    ) {
        let assistant_id = job.assistant_id();
        if cancellation.is_cancelled() {
            self.cancel_unstarted(ticket, assistant_id);
            return;
        }
        // Read as the run starts rather than at admission, so turning history
        // off while the job waited keeps it out of history. A prompt run with
        // no entry saves nothing, so it doesn't read the settings.
        let reads_history = match &job {
            ChatJob::Ask(_) => true,
            ChatJob::Prompt(request) => request.entry.is_some(),
        };
        let history = match reads_history.then(|| self.enabled_history()).transpose() {
            Ok(history) => history.flatten(),
            Err(error) => return self.report(ticket, assistant_id, Err(error)),
        };
        let assistant = assistant(history);
        let mut steps = StepRelay::new(&self.emitter, ticket, assistant_id);
        steps.step(Step::Thinking);
        let outcome = match job {
            ChatJob::Ask(request) => {
                self.ask(&assistant, &mut steps, request, history, &cancellation)
            }
            ChatJob::Prompt(request) => {
                self.run_prompt(&assistant, request, history, &cancellation)
            }
        };
        self.report(ticket, assistant_id, outcome);
    }

    /// Emits how a job ended.
    fn report(
        &self,
        ticket: Ticket,
        assistant_id: AssistantId,
        outcome: Result<ChatStageKind, ChatError>,
    ) {
        let kind = match outcome {
            Ok(kind) => kind,
            Err(error) => failed(&error),
        };
        if let ChatStageKind::Failed { code, .. } = &kind {
            tracing::warn!(%ticket, assistant = assistant_id.name(), code = code.label(), "chat job failed");
        } else {
            tracing::info!(%ticket, assistant = assistant_id.name(), "chat job replied");
        }
        self.emitter.emit(ChatStage {
            ticket,
            assistant_id,
            kind,
        });
    }

    /// The contents of a prompt's file. For a stored prompt, that is the file the list names now.
    pub(in crate::daemon) fn prompt_contents(
        &self,
        source: PromptSource,
    ) -> Result<PromptContents, ChatError> {
        let file = match source {
            PromptSource::File(file) => file,
            PromptSource::Stored(id) => self
                .settings
                .snapshot()
                .map_err(ChatError::Settings)?
                .settings
                .prompts
                .find(id.as_str())
                .ok_or(ChatError::UnknownPrompt)?
                .file()
                .to_path_buf(),
        };
        Ok(prompts::contents(&file)?)
    }

    /// Saves through the daemon's own store, so the prompt reaches its cache,
    /// and each client that watches settings, at once.
    pub(in crate::daemon) fn save_prompt(&self, draft: PromptDraft) -> Result<Prompt, SaveError> {
        prompts::save(&self.home, &self.settings, draft)
    }

    pub(in crate::daemon) fn remove_prompt(&self, id: &PromptId) -> Result<Prompt, SaveError> {
        prompts::remove(&self.settings, id)
    }

    /// The prompt and assistant as stored now. A later edit applies to the next run. A one-off
    /// prompt needs no stored prompt.
    pub(in crate::daemon) fn prompt_request(
        &self,
        call: PromptCall,
    ) -> Result<PromptRequest, ChatError> {
        let snapshot = self.settings.snapshot().map_err(ChatError::Settings)?;
        let (name, kind, instructions) = match call.prompt {
            PromptChoice::Stored(id) => {
                let prompt = snapshot
                    .settings
                    .prompts
                    .find(id.as_str())
                    .ok_or(ChatError::UnknownPrompt)?;
                if !prompt.enabled() {
                    return Err(ChatError::PromptOff);
                }
                (
                    prompt.name().to_owned(),
                    prompt.kind(),
                    PromptInstructions::File(prompt.file().to_path_buf()),
                )
            }
            PromptChoice::OneOff(instructions) => (
                prompts::one_off_name(&instructions),
                PromptKind::Compose,
                PromptInstructions::OneOff(instructions),
            ),
        };
        let assistant_id = snapshot.settings.resolve_assistant_id();
        let (text, entry) = match call.input {
            PromptInput::Text(text) => (text, None),
            PromptInput::Entry(id) => {
                let text = self
                    .history
                    .get(id)
                    .and_then(HistoryEntry::prompt_text)
                    .map_err(ChatError::History)?;
                (text, Some(id))
            }
        };
        Ok(PromptRequest {
            text,
            name,
            entry,
            kind,
            instructions,
            assistant_id,
            model_id: snapshot.settings.model_id(assistant_id).map(str::to_owned),
            application: call.application,
            tap_cancels: call.tap_cancels,
            output_mode: call.output_mode,
        })
    }

    /// Ends a chat that never ran, the way a cancelled run ends.
    pub(in crate::daemon) fn cancel_unstarted(&self, ticket: Ticket, assistant_id: AssistantId) {
        self.emitter.emit(ChatStage {
            ticket,
            assistant_id,
            kind: failed(&ChatError::Assistant(AssistantError::Cancelled)),
        });
    }

    /// Where a chat goes for a caller with no working directory: the
    /// project in focus, else the one the settings pin. Reading the focus
    /// file and scanning the roots touches the disk, so callers run it off
    /// the reactor.
    pub(in crate::daemon) fn resolve_target(&self) -> Result<ChatTarget, ResolveError> {
        let snapshot = self.settings.snapshot().map_err(ResolveError::Settings)?;
        let assistant_id = snapshot.settings.resolve_assistant_id();
        let config = self.chat_config();
        let projects = Projects::load_with(&self.home, &config).map_err(ResolveError::Projects)?;
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
        Ok(ChatTarget {
            workdir,
            assistant_id,
        })
    }

    fn chat_config(&self) -> ChatConfig {
        load_chat(&self.cache.effective())
    }

    fn ask<A: Assistant>(
        &self,
        assistant: &A,
        steps: &mut StepRelay<'_>,
        request: ChatRequest,
        history: Option<&HistoryStore>,
        cancel: &CancelToken,
    ) -> Result<ChatStageKind, ChatError> {
        let config = self.chat_config();
        let started = (SystemTime::now(), Instant::now());
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
                    &self.delivery,
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
        let Reply { text, conversation } =
            self.turn(assistant, steps, &request, &prompt, config.timeout, cancel)?;
        if let Some(memory) = &memory
            && request.own_voice
        {
            self.remember(memory, &request);
        }

        let delivered = match (refused, delivery) {
            (Some(code), _) => Some(Err(code)),
            (None, Some(delivery)) => Some(delivery.deliver(&text, &self.typing, cancel)),
            (None, None) => None,
        };
        // Saved before the reply goes out, so whoever hears it finds it saved.
        if let Some(history) = history {
            let failure = delivered
                .and_then(Result::err)
                .map(|code| RpcFailure::new(code, "the reply couldn't be typed"));
            let (created_at_ms, duration_ms) = elapsed_ms(started);
            history.save_chat(NewChat {
                request: &request.text,
                reply: &text,
                workdir: &request.workdir,
                conversation: conversation.as_deref(),
                created_at_ms,
                duration_ms,
                failure,
            });
        }
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
        history: Option<&HistoryStore>,
        cancel: &CancelToken,
    ) -> Result<ChatStageKind, ChatError> {
        let config = self.chat_config();
        let started = (SystemTime::now(), Instant::now());
        // Prepared before the run, so a front app that refuses typing fails
        // it before the assistant spends any time.
        let delivery = request
            .output_mode
            .map(|mode| {
                Delivery::prepare(
                    mode,
                    self.target_policy(request.application.as_ref())?,
                    config.delivery_timeout,
                    &self.delivery,
                    cancel,
                )
            })
            .transpose()?;
        let text = self.prompt_answer(assistant, &request, config.timeout, cancel)?;
        if cancel.is_cancelled() {
            return Err(ChatError::Assistant(AssistantError::Cancelled));
        }
        let delivered = delivery.map(|delivery| delivery.deliver(&text, &self.typing, cancel));
        // Saved before the reply goes out, so whoever shows it finds it saved.
        if let (Some(history), Some(entry)) = (history, request.entry)
            && !text.trim().is_empty()
        {
            let (created_at_ms, _) = elapsed_ms(started);
            history.save_result(entry, &request.name, &text, created_at_ms);
        }
        Ok(replied(text, delivered))
    }

    pub(in crate::daemon) fn deliver(
        &self,
        call: &DeliverCall,
        cancel: &CancelToken,
    ) -> Result<DeliveryOutcome, ChatError> {
        #[cfg(test)]
        if let Some(deliver) = self
            .assistant_overrides
            .as_ref()
            .and_then(|overrides| overrides.deliver.as_ref())
        {
            return deliver(call, cancel);
        }
        if cancel.is_cancelled() {
            return Err(ChatError::Delivery(DeliveryError::Cancelled));
        }
        let config = self.chat_config();
        Delivery::prepare(
            call.output_mode,
            self.target_policy(call.application.as_ref())?,
            config.delivery_timeout,
            &self.delivery,
            cancel,
        )?
        .send(&call.text, &self.typing, cancel)
        .map_err(ChatError::Delivery)
    }

    fn prompt_answer<A: Assistant>(
        &self,
        assistant: &A,
        request: &PromptRequest,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<String, ChatError> {
        let instructions = match &request.instructions {
            PromptInstructions::File(path) => read_instructions(path)?,
            PromptInstructions::OneOff(instructions) => instructions.clone(),
        };
        let run = PromptRun {
            model_id: request.model_id.as_deref(),
            kind: request.kind,
            instructions: instructions.as_str(),
            transcript: &request.text,
        };
        assistant
            .run_prompt(&run, cancel, timeout)
            .map_err(|error| {
                ChatError::Assistant(diagnosed(assistant, request.assistant_id, error, cancel))
            })
    }

    /// Refuses an app with Aria off and, when given, any app but `application`.
    fn target_policy(
        &self,
        application: Option<&ApplicationId>,
    ) -> Result<TargetPolicy, ChatError> {
        let overrides = self
            .settings
            .snapshot()
            .map_err(ChatError::Settings)?
            .settings
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
        steps: &mut StepRelay<'_>,
        request: &ChatRequest,
        prompt: &str,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<Reply, ChatError> {
        let key = request.key();
        // Forgetting happens before the chat, so a failed one cannot
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
        let prompted = session
            .prompt(prompt, steps, cancel, timeout)
            .map_err(|error| diagnosed(assistant, request.assistant_id, error, cancel));
        // The assistant may have deleted the session it was asked to resume.
        // Kept, its id would fail every later chat on this key.
        if resume.is_some() && matches!(prompted, Err(AssistantError::Failed(_))) {
            self.sessions.forget(&key)?;
        }
        let text = prompted?;
        let conversation = session.resume_id().map(str::to_owned);
        if let Some(resume) = &conversation {
            self.sessions.record(key, resume)?;
        }
        Ok(Reply { text, conversation })
    }

    /// The memory section when it is on.
    fn memory(&self) -> Option<MemoryConfig> {
        let settings = self.cache.effective();
        let config = load_memory(&settings.memory, &self.home);
        config.enabled.then_some(config)
    }

    /// Keeps what the owner said, for extraction later. Only a recording
    /// the own-voice gate judged reaches here, and only an enrolled owner
    /// can be the one it judged for.
    fn remember(&self, memory: &MemoryConfig, request: &ChatRequest) {
        if !self.owner_enrolled() {
            tracing::warn!("no owner is enrolled; the chat is not remembered");
            return;
        }
        let session = request.key().to_string();
        let recorded = Store::open(&memory.dir.join(DB_FILE))
            .and_then(|store| store.add_turn(&session, &request.text));
        if let Err(error) = recorded {
            tracing::warn!(error = %error_chain(&error), "chat turn not remembered");
            return;
        }
        // A full slot means a nudge is already waiting; one is enough.
        if let Some(wake) = &self.memory_wake {
            let _ = wake.try_send(());
        }
    }

    fn owner_enrolled(&self) -> bool {
        let settings = self.cache.effective();
        let config = SpeakerConfig::load(&settings.speaker, &self.home);
        aria_voice::speaker::usable_profiles(ProfileStore::open(&config.profiles_dir).read_all())
            .iter()
            .any(|profile| profile.role == Role::Owner)
    }
}

/// What one prompt to an assistant answered, and the conversation it went on.
struct Reply {
    text: String,
    conversation: Option<String>,
}

/// Relays each change of step as a `Working` stage.
struct StepRelay<'a> {
    last: Option<Step>,
    emitter: &'a ChatEventEmitter,
    ticket: Ticket,
    assistant_id: AssistantId,
}

impl<'a> StepRelay<'a> {
    fn new(emitter: &'a ChatEventEmitter, ticket: Ticket, assistant_id: AssistantId) -> Self {
        Self {
            last: None,
            emitter,
            ticket,
            assistant_id,
        }
    }
}

impl StepSink for StepRelay<'_> {
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

/// A failed run may mean the CLI is signed out, and an HTTP error may mean
/// the model server lacks the model. The error text differs across versions
/// and servers, so the readiness check decides.
fn diagnosed<A: Assistant>(
    assistant: &A,
    assistant_id: AssistantId,
    error: AssistantError,
    cancel: &CancelToken,
) -> AssistantError {
    if !matches!(
        error,
        AssistantError::Failed(_) | AssistantError::Provider(ProviderError::HttpStatus)
    ) {
        return error;
    }
    match assistant.check_ready(cancel) {
        AssistantStatus::SignedOut { fix } => AssistantError::SignedOut {
            assistant: assistant_id,
            fix,
        },
        AssistantStatus::ModelMissing { model } => AssistantError::ModelMissing { model },
        AssistantStatus::Ready
        | AssistantStatus::NotInstalled { .. }
        | AssistantStatus::NotConfigured { .. }
        | AssistantStatus::Unreachable { .. }
        | AssistantStatus::Unchecked(_) => error,
    }
}

/// Recalls a conversation from the history store, or nothing while history is off.
struct SavedConversations<'a> {
    history: Option<&'a HistoryStore>,
}

impl Conversations for SavedConversations<'_> {
    fn recall(&self, conversation: &str) -> Vec<Ask> {
        let Some(history) = self.history else {
            return Vec::new();
        };
        match history.conversation(conversation, RECALL_BYTES) {
            Ok(entries) => entries.into_iter().filter_map(ask).collect(),
            Err(error) => {
                tracing::warn!(error = %error_chain(&error), "the conversation could not be recalled");
                Vec::new()
            }
        }
    }
}

fn ask(entry: HistoryEntry) -> Option<Ask> {
    match entry.content {
        HistoryContent::Chat { reply } => Some(Ask {
            request: entry.text,
            reply,
        }),
        HistoryContent::Dictation(_) | HistoryContent::SystemTranscription { .. } => None,
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
        delivery: &DeliveryFactory,
        cancel: &CancelToken,
    ) -> Result<Self, ChatError> {
        let platform = delivery.create(timeout).map_err(ChatError::Delivery)?;
        policy
            .confirm_front(&platform)
            .map_err(ChatError::Delivery)?;
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
    fn deliver(
        self,
        text: &str,
        typing: &TypingTurns,
        cancel: &CancelToken,
    ) -> Result<DeliveryOutcome, RpcErrorCode> {
        self.send(text, typing, cancel).map_err(|failed| {
            tracing::warn!(error = %error_chain(&failed), "the reply was not delivered");
            RpcErrorCode::from(&ChatError::Delivery(failed))
        })
    }

    fn send(
        self,
        text: &str,
        typing: &TypingTurns,
        cancel: &CancelToken,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        let _turn = typing.take(cancel)?;
        let text = self.platform.caret(&self.target).insert(text);
        self.policy
            .confirm_front(&self.platform)
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
            })
    }
}

/// Typing takes turns across chats, prompt runs and saved text, so two texts
/// never interleave in one app. Only the typing holds a turn, never the
/// assistant's run before it.
///
/// The turn is a lock held across blocking work, because waiting is the point. Each delivery's own
/// timeout bounds how long a turn lasts. A separate typing thread can't do the typing, since a
/// prepared macOS target holds Accessibility handles that stay on the thread that made them.
#[derive(Default)]
struct TypingTurns(Mutex<()>);

impl TypingTurns {
    /// Waits for the text typing now, then refuses if cancelled meanwhile.
    fn take(&self, cancel: &CancelToken) -> Result<MutexGuard<'_, ()>, DeliveryError> {
        if cancel.is_cancelled() {
            return Err(DeliveryError::Cancelled);
        }
        // The lock guards no data, so a panic while typing leaves nothing
        // to repair.
        let turn = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if cancel.is_cancelled() {
            return Err(DeliveryError::Cancelled);
        }
        Ok(turn)
    }
}

/// When an operation began, and how long it has run, in milliseconds.
fn elapsed_ms((created, started): (SystemTime, Instant)) -> (u64, u64) {
    let created_at_ms = created
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    (created_at_ms, started.elapsed().as_millis() as u64)
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
    ChatEventEmitter::subscribed_by(aria_assistant::chat::EVENT_THREAD, move |stage| {
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

    use aria_assistant::chat::Instructions;
    use aria_settings::PromptKind;
    use aria_voice::history::{HistoryContent, HistoryError};

    use super::*;

    type ExecuteAssistant = dyn Fn(&ChatRuntime, Ticket, ChatJob, CancelToken) + Send + Sync;
    type DiscoverModels = dyn Fn(AssistantId, &CancelToken) -> Result<Vec<AssistantModel>, AssistantError>
        + Send
        + Sync;
    type CheckReady = dyn Fn(AssistantId, &CancelToken) -> AssistantStatus + Send + Sync;
    type DeliverText =
        dyn Fn(&DeliverCall, &CancelToken) -> Result<DeliveryOutcome, ChatError> + Send + Sync;

    pub(in crate::daemon) struct AssistantOverrides {
        pub(super) execute: Box<ExecuteAssistant>,
        pub(super) models: Box<DiscoverModels>,
        pub(super) ready: Box<CheckReady>,
        /// `None` types for real.
        pub(in crate::daemon) deliver: Option<Box<DeliverText>>,
    }

    /// Records what it was asked and answers with a fresh id each turn.
    #[derive(Default)]
    pub(in crate::daemon) struct Log {
        pub(in crate::daemon::chat) sessions: Vec<Option<String>>,
        pub(in crate::daemon::chat) prompts: Vec<String>,
    }

    pub(in crate::daemon) struct FakeAssistant(pub(in crate::daemon::chat) Arc<Mutex<Log>>);

    impl Assistant for FakeAssistant {
        type Session = FakeSession;

        fn models(&self, _cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
            Ok(Vec::new())
        }

        fn check_ready(&self, _cancel: &CancelToken) -> AssistantStatus {
            AssistantStatus::Ready
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
        use crate::daemon::{host::DaemonLifetime, service::ChatRpc};
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
                Arc::clone(&runtime),
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

    pub(in crate::daemon) struct FakeSession {
        log: Arc<Mutex<Log>>,
        resume: Option<String>,
    }

    impl Session for FakeSession {
        fn prompt(
            &mut self,
            text: &str,
            steps: &mut dyn StepSink,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<String, AssistantError> {
            let mut log = self.log.lock().expect("the log is not poisoned");
            log.sessions.push(self.resume.clone());
            log.prompts.push(text.to_owned());
            let turn = log.sessions.len();
            self.resume = Some(format!("session-{turn}"));
            steps.step(Step::Thinking);
            steps.step(Step::Reading);
            steps.step(Step::Reading);
            steps.step(Step::Searching);
            Ok(format!("reply {turn}"))
        }

        fn resume_id(&self) -> Option<&str> {
            self.resume.as_deref()
        }
    }

    pub(in crate::daemon) fn runtime_with<A: Assistant + 'static>(
        directory: &Path,
        assistant_factory: impl Fn(AssistantId) -> A + Send + Sync + 'static,
    ) -> (ChatRuntime, std::sync::mpsc::Receiver<ChatStage>) {
        let (runtime, received, _wake) = runtime_waking(directory, assistant_factory);
        (runtime, received)
    }

    impl ChatRuntime {
        pub(in crate::daemon) fn settings(&self) -> &SettingsStore {
            &self.settings
        }

        pub(in crate::daemon) fn cache(&self) -> &SettingsCache {
            &self.cache
        }
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
        let for_models = Arc::clone(&assistant_factory);
        let cache = Arc::new(
            SettingsCache::open(
                aria_settings::default_path(directory),
                directory.to_path_buf(),
            )
            .expect("test settings"),
        );
        let runtime = ChatRuntime {
            home: directory.to_path_buf(),
            codex_home: directory.join(".codex"),
            sessions: SessionStore::open(directory.join(SESSION_FILE)),
            emitter: ChatEventEmitter::subscribed_by("aria-test-chat-events", move |stage| {
                let _ = events.send(stage);
            }),
            assistant_overrides: Some(AssistantOverrides {
                execute: Box::new(move |runtime, ticket, job, cancel| {
                    let assistant_id = job.assistant_id();
                    runtime.execute_with(|_| for_execution(assistant_id), ticket, job, cancel);
                }),
                models: Box::new(move |assistant_id, cancel| {
                    for_models(assistant_id).models(cancel)
                }),
                ready: Box::new(move |assistant_id, cancel| {
                    assistant_factory(assistant_id).check_ready(cancel)
                }),
                deliver: None,
            }),
            settings: Arc::new(SettingsStore::new(Arc::clone(&cache))),
            cache,
            memory_wake: Some(wake),
            delivery: DeliveryFactory::detect(directory),
            typing: TypingTurns::default(),
            history: Arc::new(HistoryStore::open(&directory.join(".aria/history"))),
        };
        (runtime, received, woken)
    }

    /// A home whose memory section is on, with a profile block written.
    fn home_with_memory() -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("a temp home");
        std::fs::create_dir_all(home.path().join(".aria/memory")).expect("the memory directory");
        std::fs::write(
            home.path().join(".aria/settings.toml"),
            "[memory]\nenabled = true\n",
        )
        .expect("the settings file");
        std::fs::write(
            home.path().join(".aria/memory/profile.md"),
            "## About the user\n- Is Sam\n",
        )
        .expect("the profile");
        home
    }

    fn turns(home: &Path, request: &ChatRequest) -> Vec<String> {
        let store = Store::open(&home.join(".aria/memory").join(DB_FILE)).expect("opens");
        store
            .turns(&request.key().to_string(), 0, i64::MAX)
            .expect("reads")
    }

    fn enroll_owner(home: &Path) {
        aria_voice::speaker::ProfileStore::open(home.join(".aria/speakers"))
            .save(
                &aria_kit::speaker::enrolled_voice("sam", Role::Owner, vec![1.0]),
                aria_voice::speaker::Replace::Never,
            )
            .expect("the owner enrolls");
    }

    fn prompt_request(file: PathBuf) -> PromptRequest {
        PromptRequest {
            text: "um send it friday".to_owned(),
            name: "Polish".to_owned(),
            entry: None,
            kind: PromptKind::Rewrite,
            instructions: PromptInstructions::File(file),
            assistant_id: AssistantId::ClaudeCode,
            model_id: None,
            application: None,
            tap_cancels: false,
            output_mode: Some(OutputMode::InsertOrCopy),
        }
    }

    #[test]
    fn a_chat_into_a_missing_workdir_is_refused_as_invalid() {
        let home = tempfile::tempdir().unwrap();
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&Arc::default()));
        let missing = home.path().join("gone");

        let error = runtime
            .prepare_ask(request("hello", &missing, false))
            .expect_err("a missing workdir is refused");

        assert!(matches!(
            error,
            ChatError::Assistant(AssistantError::NoWorkdir(ref path)) if *path == missing
        ));
        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::InvalidRequest);
    }

    #[test]
    fn chat_models_are_bound_at_admission_and_request_overrides_win() {
        use aria_settings::SettingsPatch;
        let home = tempfile::tempdir().unwrap();
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&Arc::default()));
        runtime
            .settings
            .modify(SettingsPatch {
                assistant_models: [
                    (AssistantId::ClaudeCode, "claude-first".to_owned()),
                    (AssistantId::Codex, "codex-first".to_owned()),
                ]
                .into(),
                ..Default::default()
            })
            .unwrap();
        let admitted = runtime
            .prepare_ask(request("hello", home.path(), false))
            .unwrap();
        let written = runtime
            .settings
            .modify(SettingsPatch {
                assistant_models: [(AssistantId::ClaudeCode, "claude-later".to_owned())].into(),
                ..Default::default()
            })
            .unwrap()
            .revision;
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
        assert_eq!(runtime.settings.snapshot().unwrap().revision, written);
    }

    #[test]
    fn prompts_and_chat_targets_share_the_saved_assistant() {
        use aria_settings::{Prompt, PromptId, Prompts, SettingsPatch};

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".aria")).unwrap();
        std::fs::write(
            home.path().join(".aria/settings.toml"),
            "[assistant]\nworkdir = '~'\n",
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
            .settings
            .modify(SettingsPatch {
                assistant_id: Some(AssistantId::Codex),
                assistant_models: [
                    (AssistantId::ClaudeCode, "claude-choice".to_owned()),
                    (AssistantId::Codex, "codex-choice".to_owned()),
                ]
                .into(),
                prompts: Some(Prompts::new(vec![prompt]).unwrap()),
                ..Default::default()
            })
            .unwrap();
        let call = || PromptCall {
            input: PromptInput::Text("hi".to_owned()),
            prompt: PromptChoice::Stored(PromptId::new("polish").unwrap()),
            application: None,
            tap_cancels: false,
            output_mode: None,
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
            .settings
            .modify(SettingsPatch {
                assistant_id: Some(AssistantId::ClaudeCode),
                ..Default::default()
            })
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
        use aria_settings::{Prompt, PromptId, Prompts, SettingsPatch};

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
            .settings
            .modify(SettingsPatch {
                prompts: Some(prompts),
                ..SettingsPatch::default()
            })
            .expect("stored");

        let call = |id: &str| PromptCall {
            input: PromptInput::Text("hi".to_owned()),
            prompt: PromptChoice::Stored(PromptId::new(id).expect("id")),
            application: None,
            tap_cancels: false,
            output_mode: None,
        };
        let request = runtime
            .prompt_request(call("on"))
            .expect("an enabled prompt");
        assert_eq!(
            request.instructions,
            PromptInstructions::File(home.path().join("on.md"))
        );
        assert!(matches!(
            runtime.prompt_request(call("off")),
            Err(ChatError::PromptOff)
        ));
        let gone = runtime.prompt_request(call("gone"));
        assert!(matches!(gone, Err(ChatError::UnknownPrompt)));
        assert_eq!(
            gone.as_ref().map_err(RpcErrorCode::from).err(),
            Some(RpcErrorCode::PromptMissing)
        );
    }

    #[test]
    fn a_prompt_run_for_an_app_no_longer_in_front_never_reaches_the_assistant() {
        let home = tempfile::tempdir().expect("a temp home");
        let file = prompt_file(home.path(), "Fix grammar.");
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
                runtime.enabled_history().expect("settings"),
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
    fn typing_takes_turns_and_a_cancel_while_waiting_types_nothing() {
        let turns = Arc::new(TypingTurns::default());
        let first = turns.take(&CancelToken::never()).expect("the first turn");
        let cancel = CancelToken::new();
        let waiting = {
            let (turns, cancel) = (Arc::clone(&turns), cancel.clone());
            std::thread::spawn(move || turns.take(&cancel).map(drop))
        };

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !waiting.is_finished(),
            "the second waits while the first types"
        );
        cancel.cancel();
        drop(first);

        let second = waiting.join().expect("the waiting thread ends");
        assert!(
            matches!(second, Err(DeliveryError::Cancelled)),
            "{second:?}"
        );
        assert!(
            turns.take(&CancelToken::never()).is_ok(),
            "the turn is free again"
        );
    }

    #[test]
    fn a_delivery_types_only_into_the_app_it_names() {
        let home = tempfile::tempdir().expect("a temp home");
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&Arc::default()));

        let error = runtime
            .deliver(
                &DeliverCall {
                    text: "Polished.".to_owned(),
                    application: Some(ApplicationId::new("com.example.left-behind").expect("id")),
                    output_mode: OutputMode::InsertOrCopy,
                },
                &CancelToken::never(),
            )
            .expect_err("the named app is not in front");

        assert!(matches!(
            error,
            ChatError::Delivery(DeliveryError::TargetChanged)
        ));
    }

    #[test]
    fn a_prompt_run_without_an_output_mode_replies_without_checking_or_typing() {
        let home = home_with_memory();
        let file = prompt_file(home.path(), "Fix grammar.");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&log));
        let request = PromptRequest {
            application: Some(ApplicationId::new("com.example.left-behind").expect("id")),
            output_mode: None,
            ..prompt_request(file)
        };

        let reply = runtime
            .run_prompt(
                &FakeAssistant(Arc::clone(&log)),
                request,
                runtime.enabled_history().expect("settings"),
                &CancelToken::never(),
            )
            .expect("a run that types nothing needs no app in front");

        assert_eq!(
            reply,
            ChatStageKind::Replied(ChatReply {
                text: "Polished.".to_owned(),
                delivery_error: None,
                copied: false,
            })
        );
    }

    #[test]
    fn a_prompt_run_hands_one_fresh_run_the_instructions_then_the_transcript() {
        let home = home_with_memory();
        let instructions = prompt_file(home.path(), "Fix grammar.");
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
    fn a_signed_out_prompt_run_says_so() {
        let home = home_with_memory();
        let instructions = prompt_file(home.path(), "Fix grammar.");
        let (runtime, _events) = runtime_with(home.path(), |_assistant_id| FailingAssistant {
            signed_out: true,
        });

        let error = runtime
            .prompt_answer(
                &FailingAssistant { signed_out: true },
                &prompt_request(instructions),
                Duration::from_secs(5),
                &CancelToken::never(),
            )
            .expect_err("the assistant is signed out");

        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::AssistantSignedOut);
        assert_eq!(error.to_string(), "Claude Code isn't signed in; sign in");
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
        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::PromptUnreadable);
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
    fn memory_on_shows_the_profile_and_keeps_a_typed_chat_out() {
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
    fn a_chat_in_the_owners_voice_is_kept_and_wakes_the_backlog() {
        let home = home_with_memory();
        enroll_owner(home.path());
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events, mut woken) = runtime_waking(home.path(), logging_assistants(&log));
        let heard = ChatRequest {
            own_voice: true,
            ..request("hello", home.path(), false)
        };

        run(&runtime, &events, 1, heard.clone());

        assert_eq!(turns(home.path(), &heard), ["hello"]);
        assert!(woken.try_recv().is_ok());
    }

    #[test]
    fn a_local_chat_in_the_owners_voice_is_kept() {
        let home = home_with_memory();
        enroll_owner(home.path());
        let endpoint = serve_local_reply("hi there");
        std::fs::write(
            home.path().join(".aria/settings.toml"),
            format!(
                "[memory]\nenabled = true\n[provider]\nendpoint = \"{endpoint}\"\n\
                 [assistant.models]\nlocal-model = \"m\"\n"
            ),
        )
        .expect("the settings file");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events, mut woken) = runtime_waking(home.path(), logging_assistants(&log));
        let heard = ChatRequest {
            own_voice: true,
            assistant_id: AssistantId::LocalModel,
            ..request("hello", home.path(), false)
        };

        let ended = run(&runtime, &events, 1, heard.clone());

        assert!(matches!(ended, ChatStageKind::Replied(_)), "{ended:?}");
        assert_eq!(turns(home.path(), &heard), ["hello"]);
        assert!(woken.try_recv().is_ok());
    }

    /// A loopback model server that streams `reply` to every request.
    fn serve_local_reply(reply: &str) -> String {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let address = listener.local_addr().expect("its address");
        let chunk = format!(
            r#"{{"choices":[{{"delta":{{"content":"{reply}"}},"finish_reason":"stop"}}]}}"#
        );
        let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.expect("a connection");
                let mut reader = BufReader::new(stream.try_clone().expect("a reader"));
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("a header");
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().expect("a length");
                    }
                }
                let mut request = vec![0; length];
                reader.read_exact(&mut request).expect("the body");
                stream.write_all(response.as_bytes()).expect("the reply");
            }
        });
        format!("http://{address}/v1/chat/completions")
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

    fn chats(runtime: &ChatRuntime) -> Vec<HistoryEntry> {
        runtime
            .history
            .list(aria_voice::history::HistoryQuery {
                section: Some(aria_voice::history::HistorySection::Chat),
                query: String::new(),
                before_id: None,
                limit: 100,
            })
            .expect("history lists")
            .entries
    }

    #[test]
    fn a_replied_chat_is_saved_with_the_question_as_asked_and_its_reply() {
        let home = home_with_memory();
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, events) = runtime_with(home.path(), logging_assistants(&log));

        let reply = replied(run(
            &runtime,
            &events,
            1,
            request("When?", home.path(), false),
        ));

        let saved = chats(&runtime);
        let [saved] = saved.as_slice() else {
            panic!("one chat, got {}", saved.len());
        };
        assert_eq!(saved.text, "When?", "the memory profile stays out");
        assert_eq!(saved.content, HistoryContent::Chat { reply });
        assert_eq!(saved.project.as_deref(), Some(home.path()));
        let conversation = runtime
            .history
            .conversation("session-1", 1024)
            .expect("history reads");
        let ids: Vec<u64> = conversation.iter().map(|chat| chat.id).collect();
        assert_eq!(ids, [saved.id], "filed under its conversation");
    }

    #[test]
    fn a_chat_with_history_off_or_that_failed_is_not_saved() {
        let home = tempfile::tempdir().expect("a temp home");
        std::fs::create_dir_all(home.path().join(".aria")).expect("the settings folder");
        std::fs::write(
            home.path().join(".aria/settings.toml"),
            "[history]\nenabled = false\n",
        )
        .expect("the settings file");
        let (off, events) = runtime_with(home.path(), logging_assistants(&Arc::default()));
        replied(run(&off, &events, 1, request("hi", home.path(), false)));
        assert!(chats(&off).is_empty());

        let home = tempfile::tempdir().expect("a temp home");
        let (failing, events) = runtime_with(home.path(), |_assistant_id| FailingAssistant {
            signed_out: false,
        });
        let kind = run(&failing, &events, 1, request("hi", home.path(), false));
        assert!(matches!(kind, ChatStageKind::Failed { .. }));
        assert!(chats(&failing).is_empty());
    }

    fn prompt_file(home: &Path, text: &str) -> PathBuf {
        let file = home.join("polish.md");
        std::fs::write(&file, text).expect("the prompt file");
        file
    }

    fn now_ms() -> u64 {
        elapsed_ms((SystemTime::now(), Instant::now())).0
    }

    fn saved_entry(runtime: &ChatRuntime, home: &Path, reply: &str) -> u64 {
        runtime.history.save_chat(NewChat {
            request: "When?",
            reply,
            workdir: home,
            conversation: None,
            created_at_ms: now_ms(),
            duration_ms: 0,
            failure: None,
        });
        chats(runtime)[0].id
    }

    /// Stores `file` as the prompt `name` and admits a run of it on `entry`.
    fn entry_request(
        runtime: &ChatRuntime,
        entry: u64,
        name: &str,
        file: PathBuf,
    ) -> Result<PromptRequest, ChatError> {
        use aria_settings::{Prompt, PromptId, Prompts, SettingsPatch};

        let prompt = Prompt::new(
            PromptId::new("polish").expect("id"),
            name.to_owned(),
            PromptKind::Rewrite,
            file,
            true,
            None,
        )
        .expect("prompt");
        runtime
            .settings
            .modify(SettingsPatch {
                prompts: Some(Prompts::new(vec![prompt]).expect("prompts")),
                ..SettingsPatch::default()
            })
            .expect("stored");
        runtime.prompt_request(PromptCall {
            input: PromptInput::Entry(entry),
            prompt: PromptChoice::Stored(PromptId::new("polish").expect("id")),
            application: None,
            tap_cancels: false,
            output_mode: None,
        })
    }

    fn run_on(
        runtime: &ChatRuntime,
        assistant: &impl Assistant,
        entry: u64,
        file: PathBuf,
    ) -> Result<ChatStageKind, ChatError> {
        let request = entry_request(runtime, entry, "Polish", file)?;
        runtime.run_prompt(
            assistant,
            request,
            runtime.enabled_history().expect("settings"),
            &CancelToken::never(),
        )
    }

    #[test]
    fn a_prompt_run_on_an_entry_reads_its_reply_and_saves_the_result_under_it() {
        let home = home_with_memory();
        let file = prompt_file(home.path(), "Fix grammar.");
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&log));
        let entry = saved_entry(&runtime, home.path(), "Friday at noon.");

        let request = entry_request(&runtime, entry, "Polish", file).expect("the run is admitted");
        assert_eq!(
            (request.entry, request.name.as_str(), request.text.as_str()),
            (Some(entry), "Polish", "Friday at noon.")
        );
        runtime
            .run_prompt(
                &FakeAssistant(Arc::clone(&log)),
                request,
                runtime.enabled_history().expect("settings"),
                &CancelToken::never(),
            )
            .expect("the run replies");

        assert!(log.lock().unwrap().prompts[0].contains("Friday at noon."));
        let results = runtime.history.results(entry).expect("results list");
        let [result] = results.as_slice() else {
            panic!("one result, got {}", results.len());
        };
        assert_eq!(
            (result.prompt_name.as_str(), result.text.as_str()),
            ("Polish", "Polished.")
        );
    }

    #[test]
    fn a_one_off_prompt_runs_its_own_instructions_as_compose_and_saves_under_its_first_line() {
        let home = home_with_memory();
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&log));
        let entry = saved_entry(&runtime, home.path(), "Friday at noon.");

        // No prompt is stored: a one-off needs none.
        let request = runtime
            .prompt_request(PromptCall {
                input: PromptInput::Entry(entry),
                prompt: PromptChoice::OneOff(
                    Instructions::new("\n  List the action items. \nKeep it short.".into())
                        .unwrap(),
                ),
                application: None,
                tap_cancels: false,
                output_mode: None,
            })
            .expect("the run is admitted");
        assert_eq!(
            (request.name.as_str(), request.kind),
            ("List the action items.", PromptKind::Compose)
        );
        runtime
            .run_prompt(
                &FakeAssistant(Arc::clone(&log)),
                request,
                runtime.enabled_history().expect("settings"),
                &CancelToken::never(),
            )
            .expect("the run replies");

        let prompt = log.lock().unwrap().prompts.pop().expect("one prompt run");
        assert!(prompt.starts_with("\n  List the action items. \nKeep it short."));
        let results = runtime.history.results(entry).expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].prompt_name, "List the action items.");
    }

    #[test]
    fn a_prompt_cannot_run_on_a_blank_entry() {
        let home = tempfile::tempdir().expect("a temp home");
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&Arc::default()));
        runtime.history.save_chat(NewChat {
            request: "When?",
            reply: " \n",
            workdir: home.path(),
            conversation: None,
            created_at_ms: now_ms(),
            duration_ms: 0,
            failure: None,
        });
        let entry = chats(&runtime)[0].id;

        let error = entry_request(&runtime, entry, "Polish", home.path().join("polish.md"))
            .expect_err("the entry has no text");

        assert!(matches!(
            error,
            ChatError::History(HistoryError::EntryEmpty)
        ));
        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::HistoryEntryEmpty);
    }

    #[test]
    fn a_prompt_runs_only_on_a_mac_audio_transcript_read_whole() {
        let home = tempfile::tempdir().expect("a temp home");
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&Arc::default()));
        let folder = home.path().join("transcripts");
        std::fs::create_dir(&folder).expect("the transcript folder");
        let transcript = folder.join("session.txt");
        std::fs::write(&transcript, "[00:00:01] Ship it Friday.").expect("the transcript");
        let (created_at_ms, _) = elapsed_ms((SystemTime::now(), Instant::now()));
        runtime
            .history
            .save_transcript(&transcript, created_at_ms, 1_000, None);
        let entry = runtime
            .history
            .list(aria_voice::history::HistoryQuery {
                section: Some(aria_voice::history::HistorySection::SystemTranscription),
                query: String::new(),
                before_id: None,
                limit: 1,
            })
            .expect("history lists")
            .entries[0]
            .id;
        let file = home.path().join("polish.md");
        assert_eq!(
            entry_request(&runtime, entry, "Polish", file.clone())
                .expect("the whole transcript is read")
                .text,
            "[00:00:01] Ship it Friday."
        );

        std::fs::remove_dir_all(&folder).expect("the transcript's folder goes");
        let error =
            entry_request(&runtime, entry, "Polish", file).expect_err("only the excerpt is left");

        assert!(matches!(
            error,
            ChatError::History(HistoryError::TranscriptUnreadable)
        ));
        assert_eq!(
            RpcErrorCode::from(&error),
            RpcErrorCode::HistoryTranscriptUnreadable
        );
    }

    #[test]
    fn a_prompt_run_with_history_off_or_that_failed_saves_no_result() {
        let home = tempfile::tempdir().expect("a temp home");
        std::fs::create_dir_all(home.path().join(".aria")).expect("the settings folder");
        std::fs::write(
            home.path().join(".aria/settings.toml"),
            "[history]\nenabled = false\n",
        )
        .expect("the settings file");
        let file = prompt_file(home.path(), "Fix grammar.");
        let log = Arc::new(Mutex::new(Log::default()));
        let (off, _events) = runtime_with(home.path(), logging_assistants(&log));
        let entry = saved_entry(&off, home.path(), "Friday at noon.");
        run_on(&off, &FakeAssistant(log), entry, file).expect("the run replies");
        assert!(off.history.results(entry).expect("results list").is_empty());

        let home = tempfile::tempdir().expect("a temp home");
        prompt_file(home.path(), "Fix grammar.");
        let (failing, _events) = runtime_with(home.path(), |_assistant_id| FailingAssistant {
            signed_out: false,
        });
        let entry = saved_entry(&failing, home.path(), "Friday at noon.");
        let failed = FailingAssistant { signed_out: false };
        assert!(run_on(&failing, &failed, entry, home.path().join("polish.md")).is_err());
        assert!(
            failing
                .history
                .results(entry)
                .expect("results list")
                .is_empty()
        );
    }

    #[test]
    fn a_prompt_cannot_run_on_a_missing_entry() {
        let home = tempfile::tempdir().expect("a temp home");
        let (runtime, _events) = runtime_with(home.path(), logging_assistants(&Arc::default()));

        assert!(matches!(
            entry_request(&runtime, 99, "Polish", home.path().join("polish.md")),
            Err(ChatError::History(HistoryError::Missing))
        ));
    }

    #[test]
    fn a_failed_turn_is_not_kept() {
        let home = home_with_memory();
        enroll_owner(home.path());
        let (runtime, events) = runtime_with(home.path(), |_assistant_id| FailingAssistant {
            signed_out: false,
        });
        let heard = ChatRequest {
            own_voice: true,
            ..request("hello", home.path(), false)
        };

        let kind = run(&runtime, &events, 1, heard.clone());

        assert!(matches!(kind, ChatStageKind::Failed { .. }));
        assert!(turns(home.path(), &heard).is_empty());
    }

    #[test]
    fn a_server_error_for_a_model_the_server_lacks_says_to_load_it() {
        let error = diagnosed(
            &ServerWithout("qwen"),
            AssistantId::LocalModel,
            AssistantError::Provider(ProviderError::HttpStatus),
            &CancelToken::never(),
        );

        assert!(
            matches!(&error, AssistantError::ModelMissing { model } if model == "qwen"),
            "{error:?}"
        );
        assert_eq!(
            RpcErrorCode::from(&ChatError::Assistant(error)),
            RpcErrorCode::AssistantUnavailable
        );
    }

    /// A model server that answers but doesn't list its one model.
    struct ServerWithout(&'static str);

    impl Assistant for ServerWithout {
        type Session = FailingSession;

        fn models(&self, _cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
            Ok(Vec::new())
        }

        fn check_ready(&self, _cancel: &CancelToken) -> AssistantStatus {
            AssistantStatus::ModelMissing {
                model: self.0.to_owned(),
            }
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
            Err(AssistantError::Provider(ProviderError::HttpStatus))
        }
    }

    struct FailingAssistant {
        signed_out: bool,
    }

    impl Assistant for FailingAssistant {
        type Session = FailingSession;

        fn models(&self, _cancel: &CancelToken) -> Result<Vec<AssistantModel>, AssistantError> {
            Err(AssistantError::Failed("down".to_owned()))
        }

        fn check_ready(&self, _cancel: &CancelToken) -> AssistantStatus {
            if self.signed_out {
                AssistantStatus::SignedOut { fix: "sign in" }
            } else {
                AssistantStatus::Ready
            }
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
            _steps: &mut dyn StepSink,
            _cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<String, AssistantError> {
            Err(AssistantError::Failed("down".to_owned()))
        }

        fn resume_id(&self) -> Option<&str> {
            None
        }
    }

    pub(in crate::daemon) fn logging_assistants(
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
            tap_cancels: false,
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
    fn an_explicit_chat_assistant_does_not_change_the_shared_choice() {
        let directory = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Log::default()));
        let assistant_factory = move |assistant_id| {
            assert_eq!(assistant_id, AssistantId::ClaudeCode);
            FakeAssistant(Arc::clone(&log))
        };
        let (runtime, events) = runtime_with(directory.path(), assistant_factory);
        let written = runtime
            .settings
            .modify(aria_settings::SettingsPatch {
                assistant_id: Some(AssistantId::Codex),
                ..Default::default()
            })
            .unwrap()
            .revision;

        let reply = run(
            &runtime,
            &events,
            1,
            request("hello", directory.path(), false),
        );

        assert_eq!(replied(reply), "reply 1");
        let snapshot = runtime.settings.snapshot().unwrap();
        assert_eq!(snapshot.settings.assistant_id, Some(AssistantId::Codex));
        assert_eq!(snapshot.revision, written, "the chat writes no setting");
    }

    #[test]
    fn an_unstarted_chat_ends_as_cancelled_under_its_ticket() {
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
    fn forgetting_drops_only_the_asked_conversation_even_when_the_chat_fails() {
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
    fn a_signed_out_turn_says_so_and_keeps_the_conversation() {
        for signed_out in [true, false] {
            let directory = tempfile::tempdir().expect("test directory");
            let work = directory.path();
            let log = Arc::new(Mutex::new(Log::default()));
            let (runtime, events) = runtime_with(work, logging_assistants(&log));
            run(&runtime, &events, 1, request("one", work, false));
            let (failing, failing_events) =
                runtime_with(work, move |_assistant_id| FailingAssistant { signed_out });

            let outcome = run(&failing, &failing_events, 2, request("two", work, false));

            let ChatStageKind::Failed { code, message } = outcome else {
                panic!("unexpected {outcome:?}");
            };
            let record = std::fs::read_to_string(work.join(SESSION_FILE)).expect("a record");
            if signed_out {
                assert_eq!(code, RpcErrorCode::AssistantSignedOut);
                assert_eq!(message, "Claude Code isn't signed in; sign in");
                assert!(record.contains("session-1"), "{record}");
            } else {
                assert_eq!(code, RpcErrorCode::AssistantFailed);
                assert!(!record.contains("session-1"), "{record}");
            }
        }
    }

    #[tokio::test]
    async fn listed_assistants_carry_their_status_and_fix() {
        use crate::daemon::{host::DaemonLifetime, service::ChatRpc};
        use crate::ipc::wire::{self, chat_service_server::ChatService};

        let directory = tempfile::tempdir().expect("test directory");
        let log = Arc::new(Mutex::new(Log::default()));
        let (mut runtime, _events) = runtime_with(directory.path(), logging_assistants(&log));
        runtime.assistant_overrides.as_mut().unwrap().ready =
            Box::new(|assistant_id, _cancel| match assistant_id {
                AssistantId::ClaudeCode => AssistantStatus::SignedOut { fix: "sign in" },
                AssistantId::Codex => AssistantStatus::Ready,
                AssistantId::LocalModel => AssistantStatus::ModelMissing {
                    model: "qwen".to_owned(),
                },
            });
        let runtime = Arc::new(runtime);
        let (handle, _actor) = crate::daemon::chat::channel(Arc::clone(&runtime));
        let rpc = ChatRpc::new(
            handle,
            Arc::clone(&runtime),
            DaemonLifetime::Persistent,
            tokio_util::sync::CancellationToken::new(),
        );

        let listed = rpc
            .list_assistants(tonic::Request::new(wire::ListAssistantsRequest {}))
            .await
            .expect("the list")
            .into_inner()
            .assistants;

        let statuses: Vec<_> = listed
            .iter()
            .map(|assistant| (assistant.id, assistant.status, assistant.fix.as_str()))
            .collect();
        assert_eq!(
            statuses,
            [
                (
                    wire::AssistantId::ClaudeCode as i32,
                    wire::AssistantStatus::SignedOut as i32,
                    "sign in"
                ),
                (
                    wire::AssistantId::Codex as i32,
                    wire::AssistantStatus::Ready as i32,
                    ""
                ),
                (
                    wire::AssistantId::LocalModel as i32,
                    wire::AssistantStatus::ModelMissing as i32,
                    "load qwen into the model server, such as with ollama pull"
                ),
            ]
        );
    }

    #[test]
    fn a_cancelled_chat_never_starts() {
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

    /// A home with a settings file, a project root holding one git repo, and
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
            home.path().join(".aria/settings.toml"),
            format!("[assistant]\nproject_roots = ['{}']\n", roots.display()),
        )
        .expect("the settings file");
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
            home.path().join(".aria/settings.toml"),
            format!("[assistant]\nworkdir = '{}'\n", pinned.display()),
        )
        .expect("the settings file");
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
    fn resolving_a_file_as_a_working_directory_is_an_invalid_request() {
        let (home, _) = home_with_project(false);
        std::fs::write(
            home.path().join(".aria/settings.toml"),
            "[assistant]\nworkdir = '~/.aria/settings.toml'\n",
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

    /// An assistant that answers only when told to, so a test can hold a chat
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

        fn check_ready(&self, _cancel: &CancelToken) -> AssistantStatus {
            AssistantStatus::Ready
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
            _steps: &mut dyn StepSink,
            cancel: &CancelToken,
            _timeout: Duration,
        ) -> Result<String, AssistantError> {
            let turn: u64 = text.parse().expect("prompts are turn numbers");
            let _ = self.started.send(turn);
            if !park(&self.release, cancel) {
                return Err(AssistantError::Cancelled);
            }
            self.resume = Some(format!("session-{turn}"));
            Ok(format!("reply {turn}"))
        }

        fn resume_id(&self) -> Option<&str> {
            self.resume.as_deref()
        }
    }

    /// Waits for a release or the deadline. Returns `false` when cancelled first.
    fn park(release: &Mutex<std::sync::mpsc::Receiver<()>>, cancel: &CancelToken) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if cancel.is_cancelled() {
                return false;
            }
            // Held only to look, since a job waiting under the lock would keep the other from
            // seeing its cancel.
            let released = release.lock().expect("not poisoned").try_recv();
            if released.is_ok() || std::time::Instant::now() >= deadline {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// What a parked delivery reports as it starts, since it has no ticket
    /// its caller knows.
    pub(in crate::daemon::chat) const DELIVERY: u64 = u64::MAX;
    /// What a parked delivery reports as a cancel ends it.
    pub(in crate::daemon::chat) const DELIVERY_CANCELLED: u64 = u64::MAX - 1;

    impl ChatRuntime {
        /// Every job parks until released, reporting its ticket as it starts,
        /// so a test sees only how the actor schedules it. A delivery types
        /// once released.
        pub(in crate::daemon::chat) fn parking(
            mut self,
            started: Sender<u64>,
            release: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
        ) -> Self {
            let overrides = self.assistant_overrides.as_mut().expect("a test runtime");
            let (deliver_started, deliver_release) = (started.clone(), Arc::clone(&release));
            overrides.execute = Box::new(move |_runtime, ticket, _job, cancel| {
                let _ = started.send(ticket.0);
                park(&release, &cancel);
            });
            overrides.deliver = Some(Box::new(move |_call, cancel| {
                let _ = deliver_started.send(DELIVERY);
                if park(&deliver_release, cancel) {
                    Ok(DeliveryOutcome::Inserted)
                } else {
                    let _ = deliver_started.send(DELIVERY_CANCELLED);
                    Err(ChatError::Delivery(DeliveryError::Cancelled))
                }
            }));
            self
        }
    }
}
