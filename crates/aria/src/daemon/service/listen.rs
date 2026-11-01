//! The listen service: wire translation over the daemon's listen slot.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::ipc::wire;

use super::super::listen::ListenState;

pub(in crate::daemon) struct ListenRpc {
    state: Arc<ListenState>,
}

impl ListenRpc {
    pub(in crate::daemon) fn new(state: Arc<ListenState>) -> Self {
        Self { state }
    }

    /// The slot itself, for shutdown: draining has to stop a live session on
    /// a blocking task, rather than leave it to a Drop that lands on the
    /// runtime's driver thread once the grace period is already spent.
    pub(in crate::daemon) fn state(&self) -> Arc<ListenState> {
        Arc::clone(&self.state)
    }

    /// Slot operations lock and join threads, so they run off the async pool.
    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&ListenState) -> T + Send + 'static,
    ) -> Result<T, Status> {
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || operation(&state))
            .await
            .map_err(|_join| {
                Status::from(crate::rpc::RpcFailure::new(
                    crate::rpc::RpcErrorCode::Internal,
                    "the listen operation panicked",
                ))
            })
    }
}

#[tonic::async_trait]
impl wire::listen_service_server::ListenService for ListenRpc {
    async fn start(
        &self,
        request: Request<wire::StartListenRequest>,
    ) -> Result<Response<wire::StartListenResponse>, Status> {
        const METHOD: &str = "ListenService/Start";
        let output = Option::<std::path::PathBuf>::try_from(request.into_inner())
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        match self.blocking(move |state| state.start(output)).await? {
            Ok((transcript, already_listening)) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(wire::StartListenResponse {
                    already_listening,
                    transcript_path: transcript.display().to_string(),
                }))
            }
            Err(failure) => Err(super::failed(METHOD, failure)),
        }
    }

    async fn stop(
        &self,
        _request: Request<wire::StopListenRequest>,
    ) -> Result<Response<wire::StopListenResponse>, Status> {
        const METHOD: &str = "ListenService/Stop";
        match self.blocking(ListenState::stop).await? {
            Ok(outcome) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(wire::StopListenResponse {
                    was_listening: outcome.was_listening,
                    transcript_path: outcome.transcript.display().to_string(),
                }))
            }
            Err(failure) => Err(super::failed(METHOD, failure)),
        }
    }

    async fn get_status(
        &self,
        _request: Request<wire::GetListenStatusRequest>,
    ) -> Result<Response<wire::GetListenStatusResponse>, Status> {
        const METHOD: &str = "ListenService/GetStatus";
        let status = self.blocking(ListenState::status).await?;
        // The app polls this, so info would fill the log.
        tracing::debug!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(wire::GetListenStatusResponse {
            listening: status.listening,
            transcript_path: status.transcript.display().to_string(),
            error_message: status.error.unwrap_or_default(),
        }))
    }
}
