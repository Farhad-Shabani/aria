//! The dictation service: one utterance recorded, transcribed, delivered.

use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

use crate::dictation::{DictationRequest, RecordingId};
use crate::ipc::wire;
use crate::primitives::error_chain;
use crate::rpc::{RpcErrorCode, RpcFailure};

use super::super::dictation::DictationRuntimeError;
use super::super::dictation::{DictationCallError, DictationHandle, DictationRuntime};
use super::super::operation::CallError;
use super::super::server::ActivityGuard;
use super::{invalid_request, preferences};

/// Buffers progress without blocking the daemon. Full queues drop stages;
/// clients use `Recording` to enable stop and `Transcribing` to release input.
const STAGE_CAPACITY: usize = 16;

type StartStream = Pin<Box<dyn Stream<Item = Result<wire::StartDictationResponse, Status>> + Send>>;

pub(in crate::daemon) struct DictationRpc {
    dictation: DictationHandle,
    /// Stops named recordings directly, including silence-triggered ones.
    runtime: Arc<DictationRuntime>,
}

impl DictationRpc {
    pub(in crate::daemon) fn new(
        dictation: DictationHandle,
        runtime: Arc<DictationRuntime>,
    ) -> Self {
        Self { dictation, runtime }
    }
}

#[tonic::async_trait]
impl wire::dictation_service_server::DictationService for DictationRpc {
    type StartStream = StartStream;

    async fn start(
        &self,
        mut request: Request<wire::StartDictationRequest>,
    ) -> Result<Response<Self::StartStream>, Status> {
        const METHOD: &str = "DictationService/Start";
        // Kept by the worker: an on-demand daemon otherwise sees the
        // handler return and shuts down mid-press.
        let activity = ActivityGuard::take(&mut request);
        if activity.is_none() {
            tracing::warn!(
                method = METHOD,
                "no activity guard; an on-demand daemon may idle out"
            );
        }
        let request = DictationRequest::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;

        let (stages, receiver) = mpsc::channel(STAGE_CAPACITY);
        let events = self.runtime.emitter_for_call({
            let stages = stages.clone();
            move |stage| {
                let _ = stages.try_send(Ok(stage.into()));
            }
        });
        let pending = self
            .dictation
            .submit(request, Some(events))
            .await
            .map_err(|error| super::failed(METHOD, failure(error)))?;

        // The worker owns `pending`: dropping it cancels the press, which is
        // what a client hanging up should do. The stream ends once every
        // sender is gone — this one, and the emitter thread's, which lives
        // as long as the press does.
        tokio::spawn(async move {
            let _activity = activity;
            tokio::select! {
                outcome = pending.wait() => {
                    let message = match outcome {
                        Ok(outcome) => {
                            tracing::info!(method = METHOD, status = "ok", "request served");
                            Ok(outcome.into())
                        }
                        Err(error) => Err(super::failed(METHOD, failure(error))),
                    };
                    let _ = stages.send(message).await;
                }
                () = stages.closed() => {}
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn stop(
        &self,
        request: Request<wire::StopDictationRequest>,
    ) -> Result<Response<wire::StopDictationResponse>, Status> {
        const METHOD: &str = "DictationService/Stop";
        let recording = match request.into_inner().recording_id {
            0 => {
                return Err(super::failed(
                    METHOD,
                    RpcFailure::new(RpcErrorCode::InvalidRequest, "a stop names a recording"),
                ));
            }
            id => RecordingId(id),
        };
        let was_recording = self.runtime.stop_recording(recording);
        tracing::info!(
            method = METHOD,
            status = "ok",
            %recording,
            was_recording,
            "request served"
        );
        Ok(Response::new(wire::StopDictationResponse { was_recording }))
    }
}

fn failure(error: DictationCallError) -> RpcFailure {
    match error {
        CallError::Unavailable => RpcFailure::new(
            RpcErrorCode::ShuttingDown,
            "the daemon is not accepting dictation requests",
        ),
        CallError::JobFailed => RpcFailure::new(
            RpcErrorCode::Internal,
            "the dictation operation ended unexpectedly",
        ),
        CallError::Runtime(DictationRuntimeError::Preferences(error)) => {
            preferences::store_error("read", error)
        }
        CallError::Runtime(DictationRuntimeError::Initialization(error)) => {
            RpcFailure::new((&error).into(), error_chain(&error))
        }
        CallError::Runtime(DictationRuntimeError::Execution(error)) => {
            RpcFailure::new((&error).into(), error_chain(&error))
        }
    }
}
