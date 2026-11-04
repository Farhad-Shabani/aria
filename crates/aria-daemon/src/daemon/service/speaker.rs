//! The speaker service: record a voice, list who is enrolled, forget one,
//! choose the owner.

use tonic::{Request, Response, Status};

use crate::ipc::{protocol, wire};
use aria_voice::enrollment::EnrollmentRequest;

use super::super::speaker::SpeakerHandle;
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
                super::served(METHOD);
                Ok(Response::new(result.into()))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }

    async fn list(
        &self,
        _request: Request<wire::ListSpeakersRequest>,
    ) -> Result<Response<wire::ListSpeakersResponse>, Status> {
        const METHOD: &str = "SpeakerService/List";
        match self.speaker.list().await {
            Ok(speakers) => {
                super::served(METHOD);
                Ok(Response::new(speakers.into()))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
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
                super::served(METHOD);
                Ok(Response::new(wire::RemoveSpeakerResponse {}))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }

    async fn set_owner(
        &self,
        request: Request<wire::SetOwnerRequest>,
    ) -> Result<Response<wire::SetOwnerResponse>, Status> {
        const METHOD: &str = "SpeakerService/SetOwner";
        let name = protocol::speaker_name(&request.into_inner().name)
            .map_err(|error| invalid_request(METHOD, &error))?;
        match self.speaker.set_owner(name).await {
            Ok(()) => {
                super::served(METHOD);
                Ok(Response::new(wire::SetOwnerResponse {}))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }
}
