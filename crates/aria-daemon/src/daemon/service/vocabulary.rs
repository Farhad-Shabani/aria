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

    async fn add_word(
        &self,
        request: Request<wire::AddWordRequest>,
    ) -> Result<Response<wire::AddWordResponse>, Status> {
        const METHOD: &str = "VocabularyService/AddWord";
        let word = request.into_inner().word;
        let runtime = Arc::clone(&self.runtime);
        let added = super::blocking(METHOD, move || runtime.add_word(&word))
            .await?
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(added.into()))
    }

    async fn remove_word(
        &self,
        request: Request<wire::RemoveWordRequest>,
    ) -> Result<Response<wire::RemoveWordResponse>, Status> {
        const METHOD: &str = "VocabularyService/RemoveWord";
        let word = request.into_inner().word;
        let runtime = Arc::clone(&self.runtime);
        let removed = super::blocking(METHOD, move || runtime.remove_word(&word))
            .await?
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(removed.into()))
    }

    async fn list_words(
        &self,
        _request: Request<wire::ListWordsRequest>,
    ) -> Result<Response<wire::ListWordsResponse>, Status> {
        const METHOD: &str = "VocabularyService/ListWords";
        let runtime = Arc::clone(&self.runtime);
        let words = super::blocking(METHOD, move || runtime.list_words())
            .await?
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(wire::ListWordsResponse { words }))
    }
}
