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
use super::super::dictation::{
    AutoStopRule, DictationCallError, DictationHandle, DictationRuntime,
};
use super::super::operation::CallError;
use super::super::server::ActivityGuard;
use super::{invalid_request, settings};

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
            .submit(request, AutoStopRule::Setting, Some(events))
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
        CallError::Runtime(DictationRuntimeError::Settings(error)) => {
            settings::store_error("read", error)
        }
        CallError::Runtime(DictationRuntimeError::Initialization(error)) => {
            RpcFailure::new((&error).into(), error_chain(&error))
        }
        CallError::Runtime(DictationRuntimeError::Execution(error)) => {
            RpcFailure::new((&error).into(), error_chain(&error))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::thread;

    use crate::audio::canonical_fixture;
    use crate::daemon::testing::HostedDaemon;
    use crate::daemon::{
        ClientError, DaemonEvent, DeliveryMode, DictationOutcome, DictationRequest,
        DictationResult, DictationStage, StopMode,
    };
    use crate::dictation::testing::{
        EDGE_WAIT, Edge, HEARD, RecordsUntilStopped, recording_started,
    };
    use crate::ipc::wire;
    use crate::rpc::RpcErrorCode;

    /// A press that hands its transcript back instead of delivering it.
    /// It asks for complete delivery, since streaming, the default, hands none back.
    fn transcript_only(stop_mode: StopMode) -> DictationRequest {
        DictationRequest {
            delivery_mode: Some(DeliveryMode::Complete),
            transcript_only: true,
            ..DictationRequest::new(stop_mode)
        }
    }

    fn rpc_code(error: &ClientError) -> RpcErrorCode {
        match error {
            ClientError::Rpc { code, .. } => *code,
            other => panic!("expected a typed daemon failure, got {other}"),
        }
    }

    fn missing_model(home: &Path, capture: &Path) -> String {
        format!(
            "[stt]\nbackend = \"whisper\"\nmodel = \"{}\"\n[capture]\nbackend = \"wav_file\"\npath = \"{}\"\n",
            home.join("missing-model.bin").display(),
            capture.display(),
        )
    }

    #[test]
    fn a_press_without_a_model_fails_typed_and_the_daemon_keeps_serving() {
        let daemon = HostedDaemon::configured(|home| missing_model(home, &canonical_fixture(home)));
        let session = daemon.session();
        let watcher = daemon.session().events().expect("a watcher subscribes");
        let mut stages = Vec::new();

        let error = session
            .request_dictation(transcript_only(StopMode::OnSilence), |stage| {
                stages.push(stage.clone());
            })
            .expect_err("a missing model must fail the press");

        assert_eq!(rpc_code(&error), RpcErrorCode::TranscriptionFailed);
        assert!(
            matches!(stages.first(), Some(DictationStage::Recording(_))),
            "{stages:?}"
        );
        let broadcast = watcher
            .map(|event| event.expect("the watcher's stream stays readable"))
            .find(|event| matches!(event, DaemonEvent::Dictation(DictationStage::Failed(_))));
        assert!(broadcast.is_some(), "other sessions hear the failure");
        session
            .get_settings()
            .expect("a failed press leaves the daemon usable");
        drop(session);
        daemon.stop();
    }

    #[test]
    fn an_unreadable_capture_fails_as_a_capture_failure() {
        let daemon = HostedDaemon::configured(|home| missing_model(home, &home.join("absent.wav")));

        let error = daemon
            .session()
            .request_dictation(transcript_only(StopMode::OnSilence), |_| {})
            .expect_err("a capture that cannot open must fail the press");

        assert_eq!(rpc_code(&error), RpcErrorCode::CaptureFailed);
        daemon.stop();
    }

    #[test]
    fn a_second_press_is_busy_unless_it_toggles_the_first_off() {
        let (executor, edges) = RecordsUntilStopped::new();
        let daemon = HostedDaemon::scripted(executor);

        let (first, busy, toggle, late_stop) = thread::scope(|scope| {
            let first = scope.spawn(|| {
                daemon
                    .session()
                    .request_dictation(transcript_only(StopMode::Manual), |_| {})
            });
            let recording = recording_started(&edges);
            let session = daemon.session();
            let busy = session.request_dictation(transcript_only(StopMode::OnSilence), |_| {});
            let toggle = session.request_dictation(transcript_only(StopMode::Manual), |_| {});
            let first = first.join().expect("the first press does not panic");
            let late_stop = session
                .stop_dictation(recording)
                .expect("a late stop is answered");
            (first, busy, toggle, late_stop)
        });

        assert_eq!(
            rpc_code(&busy.expect_err("an on-silence press is busy")),
            RpcErrorCode::Busy
        );
        assert!(matches!(toggle, Ok(DictationOutcome::StopAccepted)));
        assert!(matches!(
            first,
            Ok(DictationOutcome::Completed(DictationResult { text, .. })) if text == HEARD
        ));
        assert!(!late_stop, "a finished recording takes no stop");
        assert_eq!(edges.try_recv(), Ok(Edge::Stopped));
        daemon.stop();
    }

    #[test]
    fn a_caller_that_disconnects_mid_press_cancels_it_and_frees_the_microphone() {
        let (executor, edges) = RecordsUntilStopped::new();
        let daemon = HostedDaemon::scripted(executor);
        let client = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the client runtime builds");

        // A raw gRPC call, because a `DaemonSession` can't be dropped mid-press.
        client.block_on(async {
            let channel = tonic::transport::Endpoint::from_shared(format!(
                "unix://{}",
                daemon.socket_path().display()
            ))
            .expect("the endpoint parses")
            .connect()
            .await
            .expect("the daemon accepts");
            let mut dictation =
                wire::dictation_service_client::DictationServiceClient::new(channel);
            let request = wire::StartDictationRequest::from(transcript_only(StopMode::Manual));
            let _abandoned = dictation.start(request).await.expect("the press starts");
            recording_started(&edges);
        });
        drop(client);

        assert_eq!(edges.recv_timeout(EDGE_WAIT), Ok(Edge::Cancelled));
        let session = daemon.session();
        thread::scope(|scope| {
            let next = scope
                .spawn(|| session.request_dictation(transcript_only(StopMode::Manual), |_| {}));
            let recording = recording_started(&edges);
            assert!(
                session
                    .stop_dictation(recording)
                    .expect("the stop is answered")
            );
            assert!(
                next.join().expect("the next press does not panic").is_ok(),
                "the next press is admitted"
            );
        });
        drop(session);
        daemon.stop();
    }
}
