//! The speaker service: record a voice, list who is enrolled, forget one.

use tonic::{Request, Response, Status};

use crate::enrollment::{EnrollError, EnrollmentRequest};
use crate::ipc::{protocol, wire};
use crate::rpc::{RpcErrorCode, RpcFailure};

use super::super::operation::CallError;
use super::super::speaker::{SpeakerCallError, SpeakerHandle};
use super::invalid_request;

pub(in crate::daemon) struct SpeakerRpc {
    speaker: SpeakerHandle,
}

impl SpeakerRpc {
    pub(in crate::daemon) fn new(speaker: SpeakerHandle) -> Self {
        Self { speaker }
    }
}

#[tonic::async_trait]
impl wire::speaker_service_server::SpeakerService for SpeakerRpc {
    async fn enroll(
        &self,
        request: Request<wire::EnrollSpeakerRequest>,
    ) -> Result<Response<wire::EnrollSpeakerResponse>, Status> {
        const METHOD: &str = "SpeakerService/Enroll";
        let request = EnrollmentRequest::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        match self.speaker.enroll(request).await {
            Ok(result) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(wire::EnrollSpeakerResponse {
                    role: wire::SpeakerRole::from(result.role) as i32,
                    consistency: result.consistency,
                }))
            }
            Err(error) => Err(super::failed(METHOD, failure(error))),
        }
    }

    async fn list(
        &self,
        _request: Request<wire::ListSpeakersRequest>,
    ) -> Result<Response<wire::ListSpeakersResponse>, Status> {
        const METHOD: &str = "SpeakerService/List";
        match self.speaker.list().await {
            Ok(speakers) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(wire::ListSpeakersResponse {
                    speakers: speakers.iter().map(wire::EnrolledSpeaker::from).collect(),
                }))
            }
            Err(error) => Err(super::failed(METHOD, failure(error))),
        }
    }

    async fn remove(
        &self,
        request: Request<wire::RemoveSpeakerRequest>,
    ) -> Result<Response<wire::RemoveSpeakerResponse>, Status> {
        const METHOD: &str = "SpeakerService/Remove";
        let name = protocol::speaker_name(&request.into_inner().name)
            .map_err(|error| invalid_request(METHOD, &error))?;
        match self.speaker.remove(name).await {
            Ok(()) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(wire::RemoveSpeakerResponse {}))
            }
            Err(error) => Err(super::failed(METHOD, failure(error))),
        }
    }
}

fn failure(error: SpeakerCallError) -> RpcFailure {
    match error {
        CallError::Unavailable => RpcFailure::new(
            RpcErrorCode::ShuttingDown,
            "the daemon is not accepting speaker requests",
        ),
        CallError::JobFailed => RpcFailure::new(
            RpcErrorCode::Internal,
            "the speaker operation ended unexpectedly",
        ),
        CallError::Runtime(error) => enroll_failure(error),
    }
}

/// The message a client sees. Every variant already says what went wrong in
/// terms the person who ran the command can act on.
fn enroll_failure(error: EnrollError) -> RpcFailure {
    RpcFailure::new(error.code(), error.to_string())
}
