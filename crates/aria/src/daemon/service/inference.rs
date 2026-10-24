use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::TaskTracker;
use tonic::{Request, Response, Status};

use crate::daemon::inference::engine::{Engine, Input};
use crate::config::ServerEndpoint;
use crate::daemon::inference::identity::{self, Admission, ServerIdentity};
use crate::daemon::inference::{
    Event, IDLE_INPUT_TIMEOUT, MAX_AUDIO_BYTES, MAX_AUDIO_MESSAGE_BYTES, OVERALL_TIMEOUT,
    QUEUE_CAPACITY, Start, Transcript,
};
use crate::daemon::relay::presented;
use crate::ipc::wire;
use crate::rpc::{RpcErrorCode, RpcFailure};

pub(in crate::daemon) struct InferenceRpc {
    engine: Arc<Engine>,
    identity: Arc<ServerIdentity>,
    jobs: TaskTracker,
}

impl InferenceRpc {
    pub(in crate::daemon) fn new(
        engine: Arc<Engine>,
        identity: Arc<ServerIdentity>,
        jobs: TaskTracker,
    ) -> Self {
        Self {
            engine,
            identity,
            jobs,
        }
    }
}

#[tonic::async_trait]
impl wire::inference_service_server::InferenceService for InferenceRpc {
    type TranscribeStream =
        Pin<Box<dyn Stream<Item = Result<wire::InferenceResponse, Status>> + Send>>;
    async fn get_info(
        &self,
        request: Request<wire::InferenceInfoRequest>,
    ) -> Result<Response<wire::InferenceInfoResponse>, Status> {
        authenticate(&self.identity, &request)?;
        Ok(Response::new(self.engine.info().into()))
    }

    async fn transcribe(
        &self,
        request: Request<tonic::Streaming<wire::InferenceRequest>>,
    ) -> Result<Response<Self::TranscribeStream>, Status> {
        let deadline = Instant::now() + request_timeout(&request)?;
        let peer = presented(&request).ok_or_else(unauthenticated)?;
        let admission = self
            .identity
            .admit(peer)
            .map_err(|error| Status::from(RpcFailure::from(error)))?;
        let cancel = admission.cancellation().clone();
        let cancel_on_drop = cancel.clone().drop_guard();
        let mut requests = request.into_inner();
        let start = until(&cancel, deadline, read_start(&mut requests)).await?;
        let (prepared, input) = self.engine.prepare(start)?;
        let (events, updates) = mpsc::channel(QUEUE_CAPACITY);
        let (finish, terminal) = oneshot::channel();
        let engine = Arc::clone(&self.engine);
        let worker = self.jobs.spawn_blocking(move || {
            let result = engine.transcribe(prepared, &admission.cancel_token(), &events);
            (result, admission)
        });
        let supervising = cancel.clone();
        self.jobs.spawn(async move {
            let _cancel_on_exit = supervising.clone().drop_guard();
            let upload = supervise(&mut requests, input, worker);
            let _ = finish.send(until(&supervising, deadline, upload).await);
        });
        Ok(Response::new(Box::pin(Output::new(
            updates,
            terminal,
            cancel,
            cancel_on_drop,
        ))))
    }
}

/// Runs `operation` until it ends, `cancel` fires, or `deadline` passes.
async fn until<T>(
    cancel: &CancellationToken,
    deadline: Instant,
    operation: impl Future<Output = Result<T, RpcFailure>>,
) -> Result<T, RpcFailure> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(cancelled()),
        result = tokio::time::timeout_at(deadline, operation) => {
            result.map_err(|_| timeout("the inference request timed out"))?
        }
    }
}

async fn read_start<S>(requests: &mut S) -> Result<Start, RpcFailure>
where
    S: Stream<Item = Result<wire::InferenceRequest, Status>> + Unpin,
{
    let Some(wire::InferenceRequest {
        body: Some(wire::inference_request::Body::Start(start)),
    }) = next_request(requests).await?
    else {
        return Err(invalid("the first inference message must be Start"));
    };
    Start::try_from(start).map_err(|_| invalid("invalid inference opening"))
}

type Worker = JoinHandle<(Result<Transcript, RpcFailure>, Admission)>;

/// Feeds the upload to the worker. A worker that fails first ends the call
/// without reading the rest of the upload.
async fn supervise<S>(
    requests: &mut S,
    input: Input,
    mut worker: Worker,
) -> Result<Transcript, RpcFailure>
where
    S: Stream<Item = Result<wire::InferenceRequest, Status>> + Unpin,
{
    let ingestion = collect(requests, input);
    tokio::pin!(ingestion);
    tokio::select! {
        biased;
        result = &mut ingestion => {
            result?;
            worker_result(worker.await)
        }
        result = &mut worker => worker_result(result),
    }
}

fn worker_result(
    result: Result<(Result<Transcript, RpcFailure>, Admission), tokio::task::JoinError>,
) -> Result<Transcript, RpcFailure> {
    let (result, _admission) = result.map_err(|_| {
        RpcFailure::new(
            RpcErrorCode::Internal,
            "the inference worker stopped unexpectedly",
        )
    })?;
    result
}

/// The response stream. A failure ends it at once and discards queued text.
/// A success sends every queued update, then Complete.
struct Output {
    updates: mpsc::Receiver<Event>,
    terminal: oneshot::Receiver<Result<Transcript, RpcFailure>>,
    stage: Stage,
    cancel: CancellationToken,
    _cancel_on_drop: DropGuard,
}

enum Stage {
    Streaming,
    Draining(wire::InferenceResponse),
    Done,
}

impl Output {
    fn new(
        updates: mpsc::Receiver<Event>,
        terminal: oneshot::Receiver<Result<Transcript, RpcFailure>>,
        cancel: CancellationToken,
        cancel_on_drop: DropGuard,
    ) -> Self {
        Self {
            updates,
            terminal,
            stage: Stage::Streaming,
            cancel,
            _cancel_on_drop: cancel_on_drop,
        }
    }
}

impl Stream for Output {
    type Item = Result<wire::InferenceResponse, Status>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Stage::Streaming = self.stage {
            if let Poll::Ready(result) = Pin::new(&mut self.terminal).poll(context) {
                match result {
                    Ok(Ok(transcript)) => {
                        self.stage = Stage::Draining(Event::Complete(transcript).into());
                    }
                    failure => {
                        self.stage = Stage::Done;
                        self.updates.close();
                        let error = match failure {
                            Ok(Err(error)) => Status::from(error),
                            _ => internal("the inference supervisor stopped unexpectedly"),
                        };
                        return Poll::Ready(Some(Err(error)));
                    }
                }
            } else if self.cancel.is_cancelled() {
                // Cancelled text must not escape before the failure that ends it.
                return Poll::Pending;
            }
        }
        if let Stage::Done = self.stage {
            return Poll::Ready(None);
        }
        match self.updates.poll_recv(context) {
            Poll::Ready(Some(event)) => Poll::Ready(Some(Ok(event.into()))),
            Poll::Ready(None) => match std::mem::replace(&mut self.stage, Stage::Done) {
                Stage::Draining(complete) => Poll::Ready(Some(Ok(complete))),
                streaming => {
                    self.stage = streaming;
                    Poll::Pending
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

async fn collect<S>(stream: &mut S, mut writer: Input) -> Result<(), RpcFailure>
where
    S: Stream<Item = Result<wire::InferenceRequest, Status>> + Unpin,
{
    let mut bytes = 0usize;
    let mut stopped = false;
    while let Some(message) = next_request(stream).await? {
        if stopped {
            return Err(invalid("no message may follow inference Stop"));
        }
        match message.body {
            Some(wire::inference_request::Body::Audio(chunk)) => {
                if chunk.len() > MAX_AUDIO_MESSAGE_BYTES
                    || bytes.saturating_add(chunk.len()) > MAX_AUDIO_BYTES
                {
                    return Err(RpcFailure::new(
                        RpcErrorCode::PayloadTooLarge,
                        "inference audio exceeds its limit",
                    ));
                }
                if !chunk.len().is_multiple_of(2) {
                    return Err(invalid("inference audio ends inside a PCM sample"));
                }
                bytes += chunk.len();
                let samples: Vec<_> = chunk
                    .chunks_exact(2)
                    .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
                    .collect();
                writer.push(&samples)?;
            }
            Some(wire::inference_request::Body::Stop(_)) => stopped = true,
            _ => return Err(invalid("expected inference audio or Stop")),
        }
    }
    if !stopped {
        return Err(invalid("inference upload ended without Stop"));
    }
    if bytes == 0 {
        return Err(invalid("inference audio is empty"));
    }
    writer.finish();
    Ok(())
}

/// The caller's gRPC deadline, capped at the server's own.
fn request_timeout<T>(request: &Request<T>) -> Result<Duration, RpcFailure> {
    let Some(value) = request.metadata().get("grpc-timeout") else {
        return Ok(OVERALL_TIMEOUT);
    };
    value
        .to_str()
        .ok()
        .and_then(grpc_timeout)
        .map(|duration| duration.min(OVERALL_TIMEOUT))
        .ok_or_else(|| invalid("invalid inference deadline"))
}

/// Parses gRPC's `grpc-timeout`: up to eight digits and a unit.
fn grpc_timeout(value: &str) -> Option<Duration> {
    if !(2..=9).contains(&value.len()) {
        return None;
    }
    let (amount, unit) = value.split_at(value.len() - 1);
    if !amount.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let amount: u64 = amount.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(amount * 3600),
        "M" => Duration::from_secs(amount * 60),
        "S" => Duration::from_secs(amount),
        "m" => Duration::from_millis(amount),
        "u" => Duration::from_micros(amount),
        "n" => Duration::from_nanos(amount),
        _ => return None,
    })
}

fn cancelled() -> RpcFailure {
    RpcFailure::new(RpcErrorCode::Cancelled, "inference was cancelled")
}

/// The next request, failing when the client goes quiet past the idle bound.
async fn next_request<S>(stream: &mut S) -> Result<Option<wire::InferenceRequest>, RpcFailure>
where
    S: Stream<Item = Result<wire::InferenceRequest, Status>> + Unpin,
{
    tokio::time::timeout(IDLE_INPUT_TIMEOUT, stream.next())
        .await
        .map_err(|_| timeout("the inference upload stalled"))?
        .transpose()
        .map_err(|status| {
            if matches!(
                status.code(),
                tonic::Code::OutOfRange | tonic::Code::ResourceExhausted
            ) {
                RpcFailure::new(
                    RpcErrorCode::PayloadTooLarge,
                    "inference audio exceeds its limit",
                )
            } else {
                RpcFailure::new(
                    RpcErrorCode::Cancelled,
                    "the inference upload was interrupted",
                )
            }
        })
}

pub(in crate::daemon) struct InferencePairingRpc {
    identity: Arc<ServerIdentity>,
    jobs: TaskTracker,
}

impl InferencePairingRpc {
    pub(in crate::daemon) fn new(identity: Arc<ServerIdentity>, jobs: TaskTracker) -> Self {
        Self { identity, jobs }
    }
}

#[tonic::async_trait]
impl wire::pairing_service_server::PairingService for InferencePairingRpc {
    async fn pair(
        &self,
        request: Request<wire::PairRequest>,
    ) -> Result<Response<wire::PairResponse>, Status> {
        let peer = presented(&request).ok_or_else(unauthenticated)?;
        let code = request.into_inner().code;
        let identity = Arc::clone(&self.identity);
        self.jobs
            .spawn_blocking(move || identity.pair(peer, &code))
            .await
            .map_err(|_| internal("pairing stopped unexpectedly"))?
            .map_err(|error| Status::from(RpcFailure::from(error)))?;
        Ok(Response::new(wire::PairResponse {}))
    }
}

pub(in crate::daemon) struct InferenceAdminRpc {
    identity: Arc<ServerIdentity>,
    jobs: TaskTracker,
}

impl InferenceAdminRpc {
    pub(in crate::daemon) fn new(identity: Arc<ServerIdentity>, jobs: TaskTracker) -> Self {
        Self { identity, jobs }
    }
}

#[tonic::async_trait]
impl wire::inference_admin_service_server::InferenceAdminService for InferenceAdminRpc {
    async fn issue_invite(
        &self,
        request: Request<wire::InferenceInviteRequest>,
    ) -> Result<Response<wire::InferenceInviteResponse>, Status> {
        let endpoint = ServerEndpoint::parse(&request.into_inner().endpoint)
            .ok_or(identity::Error::InvalidEndpoint)
            .map_err(|error| Status::from(RpcFailure::from(error)))?;
        let invitation = self
            .identity
            .issue(&endpoint)
            .map_err(|error| Status::from(RpcFailure::from(error)))?;
        let invitation_json = serde_json::to_vec(&invitation)
            .map_err(|_| internal("encoding the invitation failed"))?;
        Ok(Response::new(wire::InferenceInviteResponse {
            invitation_json,
        }))
    }

    async fn revoke(
        &self,
        request: Request<wire::InferenceRevokeRequest>,
    ) -> Result<Response<wire::InferenceRevokeResponse>, Status> {
        let pin: [u8; 32] = request
            .into_inner()
            .fingerprint
            .try_into()
            .map_err(|_| Status::from(invalid("a fingerprint must contain 32 bytes")))?;
        let identity = Arc::clone(&self.identity);
        self.jobs
            .spawn_blocking(move || identity.revoke(pin))
            .await
            .map_err(|_| internal("revocation stopped unexpectedly"))?
            .map_err(|error| Status::from(RpcFailure::from(error)))?;
        Ok(Response::new(wire::InferenceRevokeResponse {}))
    }
}

/// Admits a call only from a client this server has paired.
pub(in crate::daemon) fn authenticate<T>(
    identity: &ServerIdentity,
    request: &Request<T>,
) -> Result<(), Status> {
    match presented(request) {
        Some(pin) if identity.contains(&pin) => Ok(()),
        _ => Err(unauthenticated()),
    }
}

fn unauthenticated() -> Status {
    RpcFailure::new(
        RpcErrorCode::Unauthenticated,
        "pair this client with the inference server first",
    )
    .into()
}

fn invalid(message: &'static str) -> RpcFailure {
    RpcFailure::new(RpcErrorCode::InvalidRequest, message)
}
fn timeout(message: &'static str) -> RpcFailure {
    RpcFailure::new(RpcErrorCode::OverallTimeout, message)
}
fn internal(message: &'static str) -> Status {
    RpcFailure::new(RpcErrorCode::Internal, message).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn stalled_uploads_fail_without_waiting_for_the_overall_deadline() {
        let began = tokio::time::Instant::now();
        let mut stalled = futures_util::stream::pending();
        let result = next_request(&mut stalled).await;
        assert_eq!(result.unwrap_err().code, RpcErrorCode::OverallTimeout);
        assert_eq!(began.elapsed(), IDLE_INPUT_TIMEOUT);
    }

    #[tokio::test]
    async fn transport_failure_is_not_a_successful_upload_end() {
        let mut interrupted = tokio_stream::iter([Err(Status::cancelled("private input"))]);
        let error = next_request(&mut interrupted).await.unwrap_err();
        assert_eq!(error.code, RpcErrorCode::Cancelled);
        assert!(!error.message.contains("private input"));

        let mut oversized = tokio_stream::iter([Err(Status::out_of_range("private input"))]);
        assert_eq!(
            next_request(&mut oversized).await.unwrap_err().code,
            RpcErrorCode::PayloadTooLarge
        );
    }
    #[test]
    fn streaming_deadlines_honor_all_grpc_units_and_the_server_cap() {
        for (value, expected) in [
            ("1H", OVERALL_TIMEOUT),
            ("2M", Duration::from_secs(120)),
            ("3S", Duration::from_secs(3)),
            ("4m", Duration::from_millis(4)),
            ("5u", Duration::from_micros(5)),
            ("6n", Duration::from_nanos(6)),
        ] {
            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert("grpc-timeout", value.parse().unwrap());
            assert_eq!(request_timeout(&request).unwrap(), expected);
        }
        for value in ["", "1", "S", "1x", "-1S", "100000000S"] {
            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert("grpc-timeout", value.parse().unwrap());
            assert!(request_timeout(&request).is_err());
        }
    }

    #[tokio::test]
    async fn a_full_output_queue_cannot_hide_the_terminal_failure() {
        let (events, updates) = mpsc::channel(QUEUE_CAPACITY);
        for _ in 0..QUEUE_CAPACITY {
            events
                .try_send(Event::Update {
                    update: crate::transcription::TranscriptUpdate::Provisional(
                        "private text".into(),
                    ),
                    retained_from: 0,
                    read_to: 0,
                    completed: None,
                })
                .unwrap();
        }
        let (finish, terminal) = oneshot::channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut output = Output::new(
            updates,
            terminal,
            cancel.clone(),
            cancel.clone().drop_guard(),
        );
        assert!(
            finish
                .send(Err(RpcFailure::new(
                    RpcErrorCode::Busy,
                    "inference output is not being consumed"
                )))
                .is_ok()
        );
        assert_eq!(
            output.next().await.unwrap().unwrap_err().code(),
            tonic::Code::ResourceExhausted
        );
        assert!(output.next().await.is_none());
        drop(output);
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn completion_follows_every_buffered_update() {
        let (events, updates) = mpsc::channel(QUEUE_CAPACITY);
        events
            .try_send(Event::Update {
                update: crate::transcription::TranscriptUpdate::Provisional("words".into()),
                retained_from: 0,
                read_to: 0,
                completed: None,
            })
            .unwrap();
        drop(events);
        let (finish, terminal) = oneshot::channel();
        assert!(
            finish
                .send(Ok(Transcript {
                    text: "words".into(),
                    engine: "whisper".into()
                }))
                .is_ok()
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let mut output = Output::new(updates, terminal, cancel.clone(), cancel.drop_guard());
        assert!(matches!(
            output.next().await.unwrap().unwrap().body,
            Some(wire::inference_response::Body::Update(_))
        ));
        assert!(matches!(
            output.next().await.unwrap().unwrap().body,
            Some(wire::inference_response::Body::Complete(_))
        ));
        assert!(output.next().await.is_none());
    }
    #[tokio::test]
    async fn malformed_ingress_removes_its_spool_and_wakes_its_reader() {
        let directory = tempfile::tempdir().unwrap();
        let (mut reader, writer) = crate::audio::Recording::open_spooled(
            directory.path(),
            Duration::from_secs(crate::daemon::inference::MAX_AUDIO_SECONDS),
        )
        .unwrap();
        let mut requests = tokio_stream::iter([
            Ok(wire::InferenceRequest {
                body: Some(wire::inference_request::Body::Audio(vec![1, 0])),
            }),
            Ok(wire::InferenceRequest {
                body: Some(wire::inference_request::Body::Stop(wire::InferenceStop {})),
            }),
            Err(Status::cancelled("private transport failure")),
        ]);
        let error = collect(&mut requests, Input::Wav(writer))
            .await
            .unwrap_err();
        assert_eq!(error.code, RpcErrorCode::Cancelled);
        assert!(reader.fill().is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
    #[tokio::test]
    async fn cancellation_holds_buffered_text_until_the_terminal_failure_arrives() {
        let (events, updates) = mpsc::channel(QUEUE_CAPACITY);
        events
            .try_send(Event::Update {
                update: crate::transcription::TranscriptUpdate::Provisional("private words".into()),
                retained_from: 0,
                read_to: 0,
                completed: None,
            })
            .unwrap();
        let (finish, terminal) = oneshot::channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut output = Output::new(
            updates,
            terminal,
            cancel.clone(),
            cancel.clone().drop_guard(),
        );
        cancel.cancel();
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(Pin::new(&mut output).poll_next(&mut context).is_pending());
        assert!(finish.send(Err(cancelled())).is_ok());
        assert_eq!(
            output.next().await.unwrap().unwrap_err().code(),
            tonic::Code::Cancelled
        );
        assert!(output.next().await.is_none());
    }
}
