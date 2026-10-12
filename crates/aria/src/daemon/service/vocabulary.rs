use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::ipc::wire;
use crate::preferences::PreferenceStore;
use crate::rpc::{RpcErrorCode, RpcFailure};
use crate::vocabulary::{Directory, State};

use super::super::vocabulary::{Error, Runtime};

pub(in crate::daemon) struct VocabularyRpc {
    runtime: Arc<Runtime>,
    preferences: Arc<PreferenceStore>,
}

impl VocabularyRpc {
    pub(in crate::daemon) fn new(runtime: Arc<Runtime>, preferences: Arc<PreferenceStore>) -> Self {
        Self {
            runtime,
            preferences,
        }
    }

    fn directory(&self, request: wire::VocabularyRequest) -> Result<Option<Directory>, Status> {
        match Option::<Directory>::try_from(request)
            .map_err(|error| super::invalid_request("VocabularyService", &error))?
        {
            Some(directory) => Ok(Some(directory)),
            None => self
                .preferences
                .snapshot()
                .map(|snapshot| snapshot.preferences.vocabulary_directory)
                .map_err(|_| failure(Error::Unavailable)),
        }
    }
}

#[tonic::async_trait]
impl wire::vocabulary_service_server::VocabularyService for VocabularyRpc {
    async fn refresh(
        &self,
        request: Request<wire::VocabularyRequest>,
    ) -> Result<Response<wire::VocabularyStatus>, Status> {
        let directory = self
            .directory(request.into_inner())?
            .ok_or_else(|| -> Status {
                RpcFailure::new(
                    RpcErrorCode::InvalidRequest,
                    "select a vocabulary directory first",
                )
                .into()
            })?;
        let status = self.runtime.refresh(directory).await.map_err(failure)?;
        Ok(Response::new(status.into()))
    }

    async fn get_status(
        &self,
        request: Request<wire::VocabularyRequest>,
    ) -> Result<Response<wire::VocabularyStatus>, Status> {
        let directory = self.directory(request.into_inner())?;
        let mut status = self.runtime.status(directory.as_ref());
        let enabled = self
            .preferences
            .snapshot()
            .map_err(|_| failure(Error::Unavailable))?
            .preferences
            .project_vocabulary;
        if !enabled {
            status.state = State::Disabled;
        }
        Ok(Response::new(status.into()))
    }

    async fn forget(
        &self,
        request: Request<wire::VocabularyRequest>,
    ) -> Result<Response<wire::VocabularyStatus>, Status> {
        let directory = self
            .directory(request.into_inner())?
            .ok_or_else(|| -> Status {
                RpcFailure::new(
                    RpcErrorCode::InvalidRequest,
                    "select a vocabulary directory first",
                )
                .into()
            })?;
        let status = self.runtime.forget(directory).await.map_err(failure)?;
        Ok(Response::new(status.into()))
    }
}

fn failure(error: Error) -> Status {
    let code = match error {
        Error::Busy => RpcErrorCode::Busy,
        Error::Unavailable => RpcErrorCode::Internal,
        Error::Unsupported => RpcErrorCode::Unsupported,
        Error::TooLarge => RpcErrorCode::PayloadTooLarge,
    };
    RpcFailure::new(code, error.to_string()).into()
}
