//! The vocabulary service: wire translation over the vocabulary runtime.

use super::super::vocabulary::Runtime;
use crate::ipc::wire;
use aria_settings::Directory;
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub(in crate::daemon) struct VocabularyRpc {
    runtime: Arc<Runtime>,
}

impl VocabularyRpc {
    pub(in crate::daemon) fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }
}

fn requested(
    method: &'static str,
    request: wire::VocabularyRequest,
) -> Result<Option<Directory>, Status> {
    Option::<Directory>::try_from(request).map_err(|error| super::invalid_request(method, &error))
}

#[tonic::async_trait]
impl wire::vocabulary_service_server::VocabularyService for VocabularyRpc {
    async fn refresh(
        &self,
        request: Request<wire::VocabularyRequest>,
    ) -> Result<Response<wire::VocabularyStatus>, Status> {
        const METHOD: &str = "VocabularyService/Refresh";
        let requested = requested(METHOD, request.into_inner())?;
        let status = self
            .runtime
            .refresh(requested)
            .await
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(status.into()))
    }

    async fn get_status(
        &self,
        request: Request<wire::VocabularyRequest>,
    ) -> Result<Response<wire::VocabularyStatus>, Status> {
        const METHOD: &str = "VocabularyService/GetStatus";
        let requested = requested(METHOD, request.into_inner())?;
        let status = self
            .runtime
            .status(requested)
            .map_err(|error| super::failed(METHOD, &error))?;
        // The app polls this, so info would fill the log.
        tracing::debug!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(status.into()))
    }

    async fn forget(
        &self,
        request: Request<wire::VocabularyRequest>,
    ) -> Result<Response<wire::VocabularyStatus>, Status> {
        const METHOD: &str = "VocabularyService/Forget";
        let requested = requested(METHOD, request.into_inner())?;
        let status = self
            .runtime
            .forget(requested)
            .await
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(status.into()))
    }
}
