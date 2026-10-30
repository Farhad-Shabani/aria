//! The chat service: one prompt to an assistant, answered with a ticket; the
//! reply follows on the event stream.

use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

use crate::chat::{AssistantError, ChatError, ChatRequest, PromptCall, Ticket};
use crate::config::AssistantId;
use crate::ipc::wire;
use crate::primitives::error_chain;
use crate::rpc::{RpcErrorCode, RpcFailure};

use super::super::chat::{ChatCallError, ChatHandle, ChatRuntime};
use super::super::host::DaemonLifetime;
use super::super::operation::CallError;
use super::invalid_request;

pub(in crate::daemon) struct ChatRpc {
    chat: ChatHandle,
    /// Resolving reads the disk and starts no assistant, so it answers here
    /// rather than queueing behind a running ask.
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
        let assistants = tokio::task::spawn_blocking(move || runtime.assistants(&cancel))
            .await
            .map_err(|_panicked| {
                super::failed(
                    METHOD,
                    RpcFailure::new(RpcErrorCode::Internal, "checking assistants failed"),
                )
            })?;
        tracing::info!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(wire::ListAssistantsResponse {
            assistants: assistants.into_iter().map(wire::Assistant::from).collect(),
        }))
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
        let models = tokio::task::spawn_blocking(move || runtime.models(assistant_id, &cancel))
            .await
            .map_err(|_panicked| {
                super::failed(
                    METHOD,
                    RpcFailure::new(RpcErrorCode::Internal, "reading assistant models failed"),
                )
            })?
            .map_err(|error| {
                let error = ChatError::Assistant(error);
                super::failed(
                    METHOD,
                    RpcFailure::new((&error).into(), error_chain(&error)),
                )
            })?;
        tracing::info!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(wire::ListAssistantModelsResponse {
            models: models
                .into_iter()
                .map(|model| wire::AssistantModel {
                    id: model.id,
                    name: model.name,
                })
                .collect(),
        }))
    }

    async fn ask(
        &self,
        request: Request<wire::AskRequest>,
    ) -> Result<Response<wire::AskResponse>, Status> {
        const METHOD: &str = "ChatService/Ask";
        self.require_persistent(METHOD)?;
        let request = ChatRequest::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        if !request.workdir.is_dir() {
            return Err(invalid_request(
                METHOD,
                &AssistantError::NoWorkdir(request.workdir),
            ));
        }

        let runtime = Arc::clone(&self.runtime);
        let request = tokio::task::spawn_blocking(move || runtime.prepare_ask(request))
            .await
            .map_err(|_panicked| {
                super::failed(
                    METHOD,
                    RpcFailure::new(
                        RpcErrorCode::Internal,
                        "reading assistant preferences failed",
                    ),
                )
            })?
            .map_err(|error| {
                super::failed(
                    METHOD,
                    RpcFailure::new((&error).into(), error_chain(&error)),
                )
            })?;
        match self.chat.ask(request).await {
            Ok(ticket) => {
                tracing::info!(method = METHOD, status = "ok", %ticket, "request served");
                Ok(Response::new(ticket.into()))
            }
            Err(error) => Err(super::failed(METHOD, failure(error))),
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
            Err(error) => Err(super::failed(METHOD, failure(error))),
        }
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
        let request = tokio::task::spawn_blocking(move || runtime.prompt_request(call))
            .await
            .map_err(|_panicked| {
                super::failed(
                    METHOD,
                    RpcFailure::new(RpcErrorCode::Internal, "reading the prompt failed"),
                )
            })?
            .map_err(|error| {
                super::failed(
                    METHOD,
                    RpcFailure::new((&error).into(), error_chain(&error)),
                )
            })?;
        match self.chat.run_prompt(request).await {
            Ok(ticket) => {
                tracing::info!(method = METHOD, status = "ok", %ticket, "request served");
                Ok(Response::new(ticket.into()))
            }
            Err(error) => Err(super::failed(METHOD, failure(error))),
        }
    }

    async fn resolve(
        &self,
        _request: Request<wire::ResolveAskRequest>,
    ) -> Result<Response<wire::ResolveAskResponse>, Status> {
        const METHOD: &str = "ChatService/Resolve";
        self.require_persistent(METHOD)?;
        let runtime = Arc::clone(&self.runtime);
        let resolved = tokio::task::spawn_blocking(move || runtime.resolve_target())
            .await
            .map_err(|_panicked| {
                super::failed(
                    METHOD,
                    RpcFailure::new(RpcErrorCode::Internal, "resolving the ask target failed"),
                )
            })?;

        match resolved {
            Ok(target) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(target.into()))
            }
            Err(error) => Err(super::failed(
                METHOD,
                RpcFailure::new((&error).into(), error_chain(&error)),
            )),
        }
    }
}

impl ChatRpc {
    /// A ticketed ask outlives its RPC, and an on-demand daemon does not
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

fn failure(error: ChatCallError) -> RpcFailure {
    match error {
        CallError::Unavailable => RpcFailure::new(
            RpcErrorCode::ShuttingDown,
            "the daemon is not accepting chat requests",
        ),
        CallError::JobFailed => RpcFailure::new(
            RpcErrorCode::Internal,
            "the chat operation ended unexpectedly",
        ),
        CallError::Runtime(never) => match never {},
    }
}
