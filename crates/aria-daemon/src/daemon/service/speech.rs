//! The speech service: one utterance spoken aloud, stoppable.

use tonic::{Request, Response, Status};

use crate::ipc::wire;
use aria_voice::speech::SpeechRequest;

use super::super::speech::SpeechHandle;
use super::invalid_request;

pub(in crate::daemon) struct SpeechRpc {
    speech: SpeechHandle,
}

impl SpeechRpc {
    pub(in crate::daemon) fn new(speech: SpeechHandle) -> Self {
        Self { speech }
    }
}

#[tonic::async_trait]
impl wire::speech_service_server::SpeechService for SpeechRpc {
    async fn speak(
        &self,
        request: Request<wire::SpeakRequest>,
    ) -> Result<Response<wire::SpeakResponse>, Status> {
        const METHOD: &str = "SpeechService/Speak";
        let request = SpeechRequest::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;

        // Dropping this future — the client disconnected, cancelled, or ran
        // out its deadline — cancels the submitted operation.
        match self.speech.speak(request).await {
            Ok(outcome) => {
                super::served(METHOD);
                Ok(Response::new(outcome.into()))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }

    async fn stop(
        &self,
        _request: Request<wire::StopSpeechRequest>,
    ) -> Result<Response<wire::StopSpeechResponse>, Status> {
        const METHOD: &str = "SpeechService/Stop";
        match self.speech.stop().await {
            Ok(stop_requested) => {
                super::served(METHOD);
                Ok(Response::new(wire::StopSpeechResponse { stop_requested }))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }
}
