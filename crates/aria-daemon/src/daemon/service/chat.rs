//! The chat service: one prompt to an assistant, answered with a ticket; the
//! reply follows on the event stream.

use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

use super::super::chat::ChatError;
use crate::ipc::wire;
use aria_assistant::chat::{AskCall, DeliverCall, PromptCall, Ticket};
use aria_assistant::prompts::{PromptDraft, PromptSource};
use aria_settings::config::AssistantId;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_settings::{ChatKey, ChatName, PromptId};

use super::super::chat::{ChatHandle, ChatRuntime};
use super::super::host::DaemonLifetime;
use super::invalid_request;

pub(in crate::daemon) struct ChatRpc {
    chat: ChatHandle,
    /// Resolving reads the disk and starts no assistant, so it answers here
    /// rather than queueing behind a running chat.
    runtime: Arc<ChatRuntime>,
    lifetime: DaemonLifetime,
    shutdown: CancellationToken,
}

impl ChatRpc {
    pub(in crate::daemon) fn new(
        chat: ChatHandle,
        runtime: Arc<ChatRuntime>,
        lifetime: DaemonLifetime,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            chat,
            runtime,
            lifetime,
            shutdown,
        }
    }
}

#[tonic::async_trait]
impl wire::chat_service_server::ChatService for ChatRpc {
    async fn list_assistants(
        &self,
        _request: Request<wire::ListAssistantsRequest>,
    ) -> Result<Response<wire::ListAssistantsResponse>, Status> {
        const METHOD: &str = "ChatService/ListAssistants";
        let runtime = Arc::clone(&self.runtime);
        let cancellation = self.shutdown.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let cancel = CancelToken::from_fn(move || cancellation.is_cancelled());
        let assistants = super::blocking(METHOD, move || runtime.assistants(&cancel)).await?;
        super::served(METHOD);
        Ok(Response::new(assistants.into()))
    }

    async fn list_models(
        &self,
        request: Request<wire::ListAssistantModelsRequest>,
    ) -> Result<Response<wire::ListAssistantModelsResponse>, Status> {
        const METHOD: &str = "ChatService/ListModels";
        let assistant_id = AssistantId::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let runtime = Arc::clone(&self.runtime);
        let cancellation = self.shutdown.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let cancel = CancelToken::from_fn(move || cancellation.is_cancelled());
        let models = super::blocking(METHOD, move || runtime.models(assistant_id, &cancel))
            .await?
            .map_err(|error| super::failed(METHOD, &ChatError::Assistant(error)))?;
        super::served(METHOD);
        Ok(Response::new(models.into()))
    }

    async fn ask(
        &self,
        request: Request<wire::AskRequest>,
    ) -> Result<Response<wire::AskResponse>, Status> {
        const METHOD: &str = "ChatService/Ask";
        self.require_persistent(METHOD)?;
        let call = AskCall::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;

        let runtime = Arc::clone(&self.runtime);
        let request = super::blocking(METHOD, move || runtime.prepare_ask(call))
            .await?
            .map_err(|error| super::failed(METHOD, &error))?;
        match self.chat.ask(request).await {
            Ok((ticket, chat)) => {
                tracing::info!(method = METHOD, status = "ok", %ticket, "request served");
                Ok(Response::new((ticket, chat).into()))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }

    async fn cancel(
        &self,
        request: Request<wire::CancelAskRequest>,
    ) -> Result<Response<wire::CancelAskResponse>, Status> {
        const METHOD: &str = "ChatService/Cancel";
        self.require_persistent(METHOD)?;
        let ticket = Ticket::from(request.into_inner());

        match self.chat.cancel(ticket).await {
            Ok(was_pending) => {
                tracing::info!(method = METHOD, status = "ok", %ticket, was_pending, "request served");
                Ok(Response::new(wire::CancelAskResponse { was_pending }))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }

    async fn list_pending_asks(
        &self,
        request: Request<wire::ListPendingAsksRequest>,
    ) -> Result<Response<wire::ListPendingAsksResponse>, Status> {
        const METHOD: &str = "ChatService/ListPendingAsks";
        self.require_persistent(METHOD)?;
        let chat = ChatKey::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let asks = self
            .chat
            .pending(&chat)
            .await
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(asks.into()))
    }

    async fn cancel_chat(
        &self,
        request: Request<wire::CancelChatRequest>,
    ) -> Result<Response<wire::CancelChatResponse>, Status> {
        const METHOD: &str = "ChatService/CancelChat";
        self.require_persistent(METHOD)?;
        let chat = ChatKey::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let cancelled_count = self
            .chat
            .cancel_chat(&chat)
            .await
            .map_err(|error| super::failed(METHOD, &error))?;
        tracing::info!(
            method = METHOD,
            status = "ok",
            cancelled_count,
            "request served"
        );
        Ok(Response::new(wire::CancelChatResponse { cancelled_count }))
    }

    async fn delete_chat(
        &self,
        request: Request<wire::DeleteChatRequest>,
    ) -> Result<Response<wire::DeleteChatResponse>, Status> {
        const METHOD: &str = "ChatService/DeleteChat";
        let chat = ChatKey::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let deleted = self
            .chat
            .delete_chat(chat)
            .await
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(wire::DeleteChatResponse { deleted }))
    }

    async fn rename_chat(
        &self,
        request: Request<wire::RenameChatRequest>,
    ) -> Result<Response<wire::RenameChatResponse>, Status> {
        const METHOD: &str = "ChatService/RenameChat";
        let (chat, name) = <(ChatKey, ChatName)>::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let renamed = self
            .chat
            .rename_chat(chat, name)
            .await
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(renamed.into()))
    }

    async fn run_prompt(
        &self,
        request: Request<wire::RunPromptRequest>,
    ) -> Result<Response<wire::AskResponse>, Status> {
        const METHOD: &str = "ChatService/RunPrompt";
        self.require_persistent(METHOD)?;
        let call = PromptCall::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let runtime = Arc::clone(&self.runtime);
        let request = super::blocking(METHOD, move || runtime.prompt_request(call))
            .await?
            .map_err(|error| super::failed(METHOD, &error))?;
        match self.chat.run_prompt(request).await {
            Ok(ticket) => {
                tracing::info!(method = METHOD, status = "ok", %ticket, "request served");
                Ok(Response::new(ticket.into()))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }

    async fn save_prompt(
        &self,
        request: Request<wire::SavePromptRequest>,
    ) -> Result<Response<wire::SavePromptResponse>, Status> {
        const METHOD: &str = "ChatService/SavePrompt";
        let draft = PromptDraft::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let runtime = Arc::clone(&self.runtime);
        let prompt = super::blocking(METHOD, move || runtime.save_prompt(draft))
            .await?
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new((&prompt).into()))
    }

    async fn remove_prompt(
        &self,
        request: Request<wire::RemovePromptRequest>,
    ) -> Result<Response<wire::RemovePromptResponse>, Status> {
        const METHOD: &str = "ChatService/RemovePrompt";
        let id = PromptId::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let runtime = Arc::clone(&self.runtime);
        let removed = super::blocking(METHOD, move || runtime.remove_prompt(&id))
            .await?
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new((&removed).into()))
    }

    async fn get_prompt(
        &self,
        request: Request<wire::GetPromptRequest>,
    ) -> Result<Response<wire::GetPromptResponse>, Status> {
        const METHOD: &str = "ChatService/GetPrompt";
        let source = PromptSource::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let runtime = Arc::clone(&self.runtime);
        let contents = super::blocking(METHOD, move || runtime.prompt_contents(source))
            .await?
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(contents.into()))
    }

    async fn deliver(
        &self,
        request: Request<wire::DeliverRequest>,
    ) -> Result<Response<wire::DeliverResponse>, Status> {
        const METHOD: &str = "ChatService/Deliver";
        let call = DeliverCall::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let outcome = self
            .chat
            .deliver(call)
            .await
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(outcome.into()))
    }

    async fn resolve(
        &self,
        _request: Request<wire::ResolveChatTargetRequest>,
    ) -> Result<Response<wire::ResolveChatTargetResponse>, Status> {
        const METHOD: &str = "ChatService/Resolve";
        self.require_persistent(METHOD)?;
        let runtime = Arc::clone(&self.runtime);
        let resolved = super::blocking(METHOD, move || runtime.resolve_target()).await?;

        match resolved {
            Ok(target) => {
                super::served(METHOD);
                Ok(Response::new(target.into()))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }
}

impl ChatRpc {
    /// A ticketed chat outlives its RPC, and an on-demand daemon does not
    /// outlive the request that started it.
    fn require_persistent(&self, method: &'static str) -> Result<(), Status> {
        if self.lifetime == DaemonLifetime::OnDemand {
            return Err(super::failed(
                method,
                RpcFailure::new(
                    RpcErrorCode::Unsupported,
                    "chat needs the persistent aria daemon; start `aria daemon`",
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use aria_platform::delivery::DeliveryOutcome;

    use super::*;
    use crate::daemon::chat::{Log, logging_assistants, runtime_with};
    use crate::daemon::tasks::Supervised;
    use crate::ipc::wire::chat_service_server::ChatService;

    fn rpc(directory: &Path) -> (Arc<ChatRuntime>, ChatRpc) {
        let log = Arc::new(Mutex::new(Log::default()));
        let (runtime, _events) = runtime_with(directory, logging_assistants(&log));
        let runtime = Arc::new(runtime);
        let (handle, _actor) = crate::daemon::chat::channel(Arc::clone(&runtime));
        let rpc = ChatRpc::new(
            handle,
            Arc::clone(&runtime),
            DaemonLifetime::OnDemand,
            tokio_util::sync::CancellationToken::new(),
        );
        (runtime, rpc)
    }

    struct Harness {
        rpc: ChatRpc,
        shutdown: tokio_util::sync::CancellationToken,
        actor: tokio::task::JoinHandle<()>,
    }

    fn harness(runtime: ChatRuntime, lifetime: DaemonLifetime) -> Harness {
        let runtime = Arc::new(runtime);
        let (handle, actor) = crate::daemon::chat::channel(Arc::clone(&runtime));
        let shutdown = tokio_util::sync::CancellationToken::new();
        let actor = tokio::spawn(actor.run(shutdown.clone()));
        let rpc = ChatRpc::new(handle, runtime, lifetime, shutdown.clone());
        Harness {
            rpc,
            shutdown,
            actor,
        }
    }

    fn save_request(id: Option<&str>, instructions: &str) -> wire::SavePromptRequest {
        wire::SavePromptRequest {
            id: id.map(str::to_owned),
            name: Some("Summary".to_owned()),
            kind: wire::PromptKind::Compose as i32,
            file: None,
            instructions: Some(instructions.to_owned()),
        }
    }

    #[tokio::test]
    async fn a_saved_prompt_reaches_the_settings_cache_at_once() {
        let directory = tempfile::tempdir().expect("test directory");
        let (runtime, rpc) = rpc(directory.path());
        let status = runtime.cache().status();

        let saved = rpc
            .save_prompt(tonic::Request::new(save_request(None, "Three bullets.")))
            .await
            .expect("saves")
            .into_inner();

        assert!(status.has_changed().expect("the cache is open"));
        let prompts = runtime
            .settings()
            .snapshot()
            .expect("snapshot")
            .settings
            .prompts;
        let stored = prompts.find(&saved.id).expect("the cache holds it");
        assert_eq!(stored.file(), Path::new(&saved.file));
        assert_eq!(
            std::fs::read_to_string(&saved.file).expect("the file"),
            "Three bullets."
        );

        let refused = rpc
            .save_prompt(tonic::Request::new(save_request(
                Some("gone"),
                "Three bullets.",
            )))
            .await
            .expect_err("no prompt has that ID");
        let failure = crate::ipc::protocol::rpc_failure(&refused).expect("a verdict");
        assert_eq!(failure.code, RpcErrorCode::PromptMissing);
    }

    #[tokio::test]
    async fn an_on_demand_daemon_types_saved_text() {
        let directory = tempfile::tempdir().expect("test directory");
        let (mut runtime, _events) =
            runtime_with(directory.path(), logging_assistants(&Arc::default()));
        runtime.assistant_overrides.as_mut().unwrap().deliver =
            Some(Box::new(|_call, _cancel| Ok(DeliveryOutcome::Copied)));
        let harness = harness(runtime, DaemonLifetime::OnDemand);

        let delivered = harness
            .rpc
            .deliver(tonic::Request::new(wire::DeliverRequest {
                text: "Polished.".to_owned(),
                application_id: String::new(),
                output_mode: wire::OutputMode::InsertOrCopy.into(),
            }))
            .await
            .expect("the call waits for its own answer")
            .into_inner();

        assert_eq!(delivered.outcome(), wire::DeliveryOutcome::Copied);
        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    #[tokio::test]
    async fn an_ask_answers_with_the_chat_it_started_or_named() {
        let directory = tempfile::tempdir().expect("test directory");
        let (runtime, _events) =
            runtime_with(directory.path(), logging_assistants(&Arc::default()));
        let harness = harness(runtime, DaemonLifetime::Persistent);
        let workdir = directory.path().to_string_lossy().into_owned();
        let ask = |chat: wire::ask_request::Chat| {
            let request = wire::AskRequest {
                text: "hi".to_owned(),
                assistant_id: wire::AssistantId::Codex.into(),
                chat: Some(chat),
                ..Default::default()
            };
            let asked = harness.rpc.ask(tonic::Request::new(request));
            async { asked.await.expect("the ask is taken").into_inner().chat }
        };
        let started = || {
            wire::ask_request::Chat::Start(wire::StartChat {
                workdir: workdir.clone(),
                quote: None,
            })
        };

        let first = ask(started()).await.expect("a started ask names its chat");
        let second = ask(started()).await.expect("a started ask names its chat");
        let named = ask(wire::ask_request::Chat::Named(wire::NamedChat {
            name: first.name.clone(),
            ..Default::default()
        }))
        .await;
        let folder = ask(wire::ask_request::Chat::Folder(wire::FolderChat {
            workdir: workdir.clone(),
            new_session: false,
        }))
        .await;

        assert_eq!(first.assistant_id, wire::AssistantId::Codex as i32);
        assert_eq!(first.name, "hi", "named from its first question");
        assert_eq!(second.name, "hi 2", "a taken name takes a number");
        assert_eq!(named, Some(first));
        assert_eq!(folder, None, "a folder's own chat has no key");
        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    #[tokio::test]
    async fn the_chat_service_deletes_a_chat_and_refuses_one_it_cannot_name() {
        let directory = tempfile::tempdir().expect("test directory");
        let (runtime, _events) =
            runtime_with(directory.path(), logging_assistants(&Arc::default()));
        let harness = harness(runtime, DaemonLifetime::OnDemand);
        let delete = |chat| {
            harness
                .rpc
                .delete_chat(tonic::Request::new(wire::DeleteChatRequest { chat }))
        };

        let missing = delete(Some(wire::ChatKey {
            assistant_id: wire::AssistantId::Codex.into(),
            name: "plans".to_owned(),
        }))
        .await;
        let unnamed = delete(None).await.expect_err("names no chat");

        missing.expect("a missing chat deletes");
        assert_eq!(
            crate::ipc::protocol::rpc_failure(&unnamed)
                .expect("a verdict")
                .code,
            RpcErrorCode::InvalidRequest
        );
        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    #[tokio::test]
    async fn an_on_demand_daemon_neither_lists_nor_cancels_a_chats_asks() {
        let directory = tempfile::tempdir().expect("test directory");
        let (_runtime, rpc) = rpc(directory.path());
        let chat = || {
            Some(wire::ChatKey {
                assistant_id: wire::AssistantId::Codex.into(),
                name: "plans".to_owned(),
            })
        };
        let code = |refused: tonic::Status| {
            crate::ipc::protocol::rpc_failure(&refused)
                .expect("a verdict")
                .code
        };

        let listed = rpc
            .list_pending_asks(tonic::Request::new(wire::ListPendingAsksRequest {
                chat: chat(),
            }))
            .await
            .expect_err("an on-demand daemon keeps no asks");
        let cancelled = rpc
            .cancel_chat(tonic::Request::new(wire::CancelChatRequest {
                chat: chat(),
            }))
            .await
            .expect_err("an on-demand daemon keeps no asks");

        assert_eq!(code(listed), RpcErrorCode::Unsupported);
        assert_eq!(code(cancelled), RpcErrorCode::Unsupported);
    }

    #[tokio::test]
    async fn a_prompt_reads_its_instructions_or_says_why_it_cannot() {
        use wire::get_prompt_request::Source;
        use wire::get_prompt_response::Contents;

        let directory = tempfile::tempdir().expect("test directory");
        let (_runtime, rpc) = rpc(directory.path());
        let saved = rpc
            .save_prompt(tonic::Request::new(save_request(None, "Three bullets.")))
            .await
            .expect("saves")
            .into_inner();
        let file = |name: &str| directory.path().join(name).to_string_lossy().into_owned();
        std::fs::write(file("blank.md"), "").expect("blank");
        std::fs::write(file("large.md"), "x".repeat(16 * 1024 + 1)).expect("large");
        let get = |source| {
            rpc.get_prompt(tonic::Request::new(wire::GetPromptRequest {
                source: Some(source),
            }))
        };
        let contents = |response: Result<tonic::Response<wire::GetPromptResponse>, _>| {
            response.expect("reads").into_inner().contents
        };
        let code = |response: Result<_, tonic::Status>| {
            let refused = response.expect_err("refused");
            crate::ipc::protocol::rpc_failure(&refused)
                .expect("a verdict")
                .code
        };

        assert_eq!(
            contents(get(Source::PromptId(saved.id)).await),
            Some(Contents::Instructions("Three bullets.".to_owned()))
        );
        assert_eq!(
            contents(get(Source::File(file("blank.md"))).await),
            Some(Contents::Instructions(String::new()))
        );
        assert_eq!(
            contents(get(Source::File(file("gone.md"))).await),
            Some(Contents::Missing(true))
        );
        assert_eq!(
            code(get(Source::File(file("large.md"))).await),
            RpcErrorCode::PromptUnreadable
        );
        assert_eq!(
            code(get(Source::PromptId("gone".to_owned())).await),
            RpcErrorCode::PromptMissing
        );
    }

    #[tokio::test]
    async fn a_removed_prompt_leaves_the_list_and_keeps_its_file() {
        let directory = tempfile::tempdir().expect("test directory");
        let (runtime, rpc) = rpc(directory.path());
        let saved = rpc
            .save_prompt(tonic::Request::new(save_request(None, "Three bullets.")))
            .await
            .expect("saves")
            .into_inner();
        let remove = || {
            rpc.remove_prompt(tonic::Request::new(wire::RemovePromptRequest {
                id: saved.id.clone(),
            }))
        };

        let removed = remove().await.expect("removes").into_inner();

        assert_eq!(removed.file, saved.file);
        assert_eq!(
            std::fs::read_to_string(&removed.file).expect("the file stays"),
            "Three bullets."
        );
        let prompts = runtime
            .settings()
            .snapshot()
            .expect("snapshot")
            .settings
            .prompts;
        assert!(prompts.entries().is_empty());
        let refused = remove().await.expect_err("no prompt has that ID now");
        let failure = crate::ipc::protocol::rpc_failure(&refused).expect("a verdict");
        assert_eq!(failure.code, RpcErrorCode::PromptMissing);
    }
}
