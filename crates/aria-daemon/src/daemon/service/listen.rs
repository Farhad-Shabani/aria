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
        _request: Request<wire::StopListenRequest>,
    ) -> Result<Response<wire::StopListenResponse>, Status> {
        const METHOD: &str = "ListenService/Stop";
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
        _request: Request<wire::GetListenStatusRequest>,
    ) -> Result<Response<wire::GetListenStatusResponse>, Status> {
        const METHOD: &str = "ListenService/GetStatus";
        let state = Arc::clone(&self.state);
        let status = super::blocking(METHOD, move || state.status()).await?;
        // The app polls this, so info would fill the log.
        tracing::debug!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(status.into()))
    }
}
