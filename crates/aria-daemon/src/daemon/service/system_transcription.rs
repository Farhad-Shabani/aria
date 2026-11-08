//! The system transcription service: wire translation over the daemon's
//! system transcription slot.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::ipc::wire;

use super::super::system_transcription::SystemTranscriptionState;

pub(in crate::daemon) struct SystemTranscriptionRpc {
    state: Arc<SystemTranscriptionState>,
}

impl SystemTranscriptionRpc {
    pub(in crate::daemon) fn new(state: Arc<SystemTranscriptionState>) -> Self {
        Self { state }
    }

    /// The slot itself, for shutdown: draining has to stop a live session on
    /// a blocking task, rather than leave it to a Drop that lands on the
    /// runtime's driver thread once the grace period is already spent.
    pub(in crate::daemon) fn state(&self) -> Arc<SystemTranscriptionState> {
        Arc::clone(&self.state)
    }
}

#[tonic::async_trait]
impl wire::system_transcription_service_server::SystemTranscriptionService
    for SystemTranscriptionRpc
{
    async fn start(
        &self,
        request: Request<wire::StartSystemTranscriptionRequest>,
    ) -> Result<Response<wire::StartSystemTranscriptionResponse>, Status> {
        const METHOD: &str = "SystemTranscriptionService/Start";
        let output = Option::<std::path::PathBuf>::try_from(request.into_inner())
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let state = Arc::clone(&self.state);
        match super::blocking(METHOD, move || state.start(output)).await? {
            Ok(started) => {
                super::served(METHOD);
                Ok(Response::new(started.into()))
            }
            Err(failure) => Err(super::failed(METHOD, failure)),
        }
    }

    async fn stop(
        &self,
        _request: Request<wire::StopSystemTranscriptionRequest>,
    ) -> Result<Response<wire::StopSystemTranscriptionResponse>, Status> {
        const METHOD: &str = "SystemTranscriptionService/Stop";
        let state = Arc::clone(&self.state);
        match super::blocking(METHOD, move || state.stop()).await? {
            Ok(stopped) => {
                super::served(METHOD);
                Ok(Response::new(stopped.into()))
            }
            Err(failure) => Err(super::failed(METHOD, failure)),
        }
    }

    async fn get_status(
        &self,
        _request: Request<wire::GetSystemTranscriptionStatusRequest>,
    ) -> Result<Response<wire::GetSystemTranscriptionStatusResponse>, Status> {
        const METHOD: &str = "SystemTranscriptionService/GetStatus";
        let state = Arc::clone(&self.state);
        let status = super::blocking(METHOD, move || state.status()).await?;
        // The app polls this, so info would fill the log.
        tracing::debug!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(status.into()))
    }
}
