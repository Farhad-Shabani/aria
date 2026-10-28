use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use aria_kit::audio::{RecordingReader, pcm16_from_f32};
use aria_kit::backend::BackendError;
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{OperationSource, OperationStep, TranscriptUpdate};
use tonic::transport::Channel;

use super::identity::{ClientIdentity, MAX_DOCUMENT_BYTES};
use super::{OUTPUT_POLL, OUTPUT_STALL_TIMEOUT, OVERALL_TIMEOUT, Paths, QUEUE_CAPACITY, tls};
use crate::config::ServerEndpoint;
use crate::inference::{
    Completion, Event, Info, MAX_AUDIO_SAMPLES, MAX_AUDIO_SECONDS, MAX_RESPONSE_BYTES,
    MAX_TEXT_BYTES, Start, Strategy,
};
use crate::ipc::{protocol, wire};
use crate::rpc::RpcErrorCode;
use crate::transcription::{
    Hypothesis, Language, Transcriber, Transcription, TranscriptionAudio, TranscriptionError, Whole,
};

pub(super) const ADMIN_TIMEOUT: Duration = Duration::from_secs(10);
const CANCEL_POLL: Duration = Duration::from_millis(25);
/// 100 ms of audio per message.
const AUDIO_CHUNK_SAMPLES: u64 = 1600;

#[derive(Debug, thiserror::Error)]
pub enum RemoteClientError {
    #[error("cannot load remote configuration")]
    Config(#[source] crate::config::ConfigError),
    #[error("cannot prepare remote client storage")]
    Storage,
    #[error("another pairing operation is active or its lock is unavailable")]
    PairingLock,
    #[error("remote invitation is invalid or exceeds {} KiB", MAX_DOCUMENT_BYTES / 1024)]
    Invitation,
    #[error(
        "remote endpoint must be an HTTPS origin without credentials, path, query, or fragment"
    )]
    InvalidEndpoint,
    #[error("client fingerprint must be 64 hexadecimal digits")]
    InvalidFingerprint,
    #[error("remote identity is missing, invalid or not paired with this endpoint")]
    Identity,
    #[error(
        "remote trust was written but durability is unconfirmed; pair again before relying on it"
    )]
    DurabilityUnconfirmed,
    #[error(
        "remote revocation durability is unconfirmed; repair persistence before relying on it across restart"
    )]
    RevocationUnconfirmed,
    #[error("the pairing was removed, but the disk did not confirm it; forget again ({0})")]
    ForgetUnconfirmed(String),
    #[error("cannot start the remote client runtime")]
    Runtime,
    #[error("cannot establish the authenticated remote connection")]
    Connection,
    #[error("remote operation failed ({0})")]
    Rpc(RpcErrorCode),
    #[error("remote operation timed out")]
    Timeout,
    #[error("remote operation was cancelled")]
    Cancelled,
    #[error("remote server does not provide the selected engine or recognition strategy")]
    Unsupported,
    #[error("remote response is invalid")]
    Response,
    #[error("remote audio exceeds {MAX_AUDIO_SECONDS} seconds")]
    AudioTooLong,
    #[error("cannot read audio for remote transcription")]
    Audio,
    #[error("remote output was not consumed in time")]
    Backpressure,
}

#[derive(Clone)]
pub(crate) struct RemoteTranscriber {
    connection: Arc<Connection>,
    required_engine: Option<&'static str>,
}

/// What the server runs, as one operation found it.
struct Binding {
    engine: &'static str,
    strategy: Strategy,
}

/// Shared by every operation on a transcriber, so a press reuses one
/// handshake.
struct Connection {
    channel: Channel,
    runtime: RemoteRuntime,
}

/// Shut down without waiting, because the last transcriber may be dropped
/// inside another runtime.
struct RemoteRuntime {
    handle: tokio::runtime::Handle,
    runtime: Option<tokio::runtime::Runtime>,
}

impl RemoteRuntime {
    fn new() -> Result<Self, RemoteClientError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("aria-remote")
            .enable_all()
            .build()
            .map_err(|_| RemoteClientError::Runtime)?;
        Ok(Self {
            handle: runtime.handle().clone(),
            runtime: Some(runtime),
        })
    }
}

impl Drop for RemoteRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl RemoteTranscriber {
    /// Opens without the network. Each operation asks the server what it
    /// runs, so a server that is down or changed fails only that operation.
    pub(crate) fn open(home: &Path, endpoint: &ServerEndpoint) -> Result<Self, TranscriptionError> {
        Self::open_pinned(home, endpoint).map_err(transcription_error)
    }

    fn open_pinned(home: &Path, endpoint: &ServerEndpoint) -> Result<Self, RemoteClientError> {
        let identity = ClientIdentity::open(&Paths::in_home(home).client_identity())
            .map_err(identity_error)?;
        let server = identity.pinned_server(endpoint).map_err(identity_error)?;
        let endpoint = tls::endpoint(&identity, server).map_err(|_| RemoteClientError::Identity)?;
        let runtime = RemoteRuntime::new()?;
        let channel = {
            let _runtime = runtime.handle.enter();
            endpoint.connect_lazy()
        };
        Ok(Self {
            connection: Arc::new(Connection { channel, runtime }),
            required_engine: None,
        })
    }

    /// Refuses operations on a server that runs anything but `engine`.
    pub(crate) fn require_engine(self, engine: &str) -> Result<Self, TranscriptionError> {
        let engine = crate::config::backend::local(engine)
            .ok_or_else(|| transcription_error(RemoteClientError::Unsupported))?;
        Ok(Self {
            required_engine: Some(engine),
            ..self
        })
    }

    fn bind(&self, cancel: &CancelToken) -> Result<Binding, RemoteClientError> {
        let channel = self.connection.channel.clone();
        let info =
            self.connection
                .runtime
                .handle
                .block_on(bounded(cancel, ADMIN_TIMEOUT, async {
                    let response = inference_client(channel)
                        .get_info(wire::InferenceInfoRequest {})
                        .await
                        .map_err(rpc_error)?;
                    Info::try_from(response.into_inner()).map_err(|_| RemoteClientError::Response)
                }))?;
        let engine =
            crate::config::backend::local(&info.engine).ok_or(RemoteClientError::Unsupported)?;
        if self
            .required_engine
            .is_some_and(|required| required != engine)
        {
            return Err(RemoteClientError::Unsupported);
        }
        Ok(Binding {
            engine,
            strategy: info.strategy,
        })
    }
}

fn inference_client(
    channel: Channel,
) -> wire::inference_service_client::InferenceServiceClient<Channel> {
    wire::inference_service_client::InferenceServiceClient::new(channel)
        .max_decoding_message_size(MAX_RESPONSE_BYTES)
}

/// Uploads `recording` while forwarding validated updates to `output`.
async fn stream(
    channel: Channel,
    start: Start,
    recording: RecordingReader,
    output: &mpsc::SyncSender<Event>,
) -> Result<(), RemoteClientError> {
    let (requests, outgoing) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
    let progress = Arc::new(UploadProgress::default());
    let validator = ResponseValidator::new(start.engine.clone());
    let uploading = upload(start, recording, requests, Arc::clone(&progress));
    let receiving = receive(channel, outgoing, validator, &progress, output);
    let ((), terminal) = tokio::try_join!(uploading, receiving)?;
    for event in terminal {
        send_event(output, event).await?;
    }
    Ok(())
}

#[derive(Default)]
struct UploadProgress {
    sent: AtomicU64,
    finished: AtomicBool,
}

async fn receive(
    channel: Channel,
    outgoing: tokio::sync::mpsc::Receiver<wire::InferenceRequest>,
    mut validator: ResponseValidator,
    progress: &UploadProgress,
    output: &mpsc::SyncSender<Event>,
) -> Result<Vec<Event>, RemoteClientError> {
    let mut request = tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(outgoing));
    request.set_timeout(OVERALL_TIMEOUT);
    let mut responses = inference_client(channel)
        .transcribe(request)
        .await
        .map_err(rpc_error)?
        .into_inner();
    while let Some(response) = responses.message().await.map_err(rpc_error)? {
        let event = Event::try_from(response).map_err(|_| RemoteClientError::Response)?;
        let sent = progress.sent.load(Ordering::Acquire);
        let uploaded = progress.finished.load(Ordering::Acquire);
        match validator.accept(event, sent, uploaded)? {
            Accepted::Forward(event) => send_event(output, event).await?,
            Accepted::Held => {}
            Accepted::Finished(terminal) => {
                if responses.message().await.map_err(rpc_error)?.is_some() {
                    return Err(RemoteClientError::Response);
                }
                return Ok(terminal);
            }
        }
    }
    Err(RemoteClientError::Response)
}

async fn send_event(
    output: &mpsc::SyncSender<Event>,
    mut event: Event,
) -> Result<(), RemoteClientError> {
    let deadline = tokio::time::Instant::now() + OUTPUT_STALL_TIMEOUT;
    loop {
        match output.try_send(event) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => return Err(RemoteClientError::Cancelled),
            Err(mpsc::TrySendError::Full(pending)) => event = pending,
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RemoteClientError::Backpressure);
        }
        tokio::time::sleep(OUTPUT_POLL).await;
    }
}

/// Checks one response stream against what was uploaded and what came
/// before. Per-message bounds are already enforced by the protocol layer.
struct ResponseValidator {
    engine: String,
    finalized: String,
    retained_from: u64,
    read_to: u64,
    claim: Option<Claim>,
}

/// A completed transcript the server announced before its tail arrived.
struct Claim {
    transcript: Completion,
    held: Vec<Event>,
}

enum Accepted {
    Forward(Event),
    Held,
    Finished(Vec<Event>),
}

impl ResponseValidator {
    fn new(engine: impl Into<String>) -> Self {
        Self {
            engine: engine.into(),
            finalized: String::new(),
            retained_from: 0,
            read_to: 0,
            claim: None,
        }
    }

    fn accept(
        &mut self,
        event: Event,
        sent: u64,
        uploaded: bool,
    ) -> Result<Accepted, RemoteClientError> {
        match event {
            Event::Update {
                update,
                retained_from,
                read_to,
                completed,
            } => self.check_update(update, retained_from, read_to, completed, sent),
            Event::Complete(transcript) => self.check_complete(transcript, uploaded),
        }
    }

    fn check_update(
        &mut self,
        update: TranscriptUpdate,
        retained_from: u64,
        read_to: u64,
        completed: Option<Completion>,
        sent: u64,
    ) -> Result<Accepted, RemoteClientError> {
        valid(self.retained_from <= retained_from && self.read_to <= read_to && read_to <= sent)?;
        let unchecked_from = if self.claim.is_some() {
            self.finalized.len()
        } else {
            0
        };
        match &update {
            TranscriptUpdate::Finalized(segment) => {
                valid(
                    segment
                        .span()
                        .is_none_or(|span| span.start >= self.retained_from && span.end <= read_to),
                )?;
                self.append(segment.as_str())?;
            }
            TranscriptUpdate::Provisional { .. } => {
                valid(self.claim.is_none() && completed.is_none())?;
            }
        }
        self.retained_from = retained_from;
        self.read_to = read_to;
        if let Some(transcript) = completed {
            self.claim(transcript)?;
        }
        let event = Event::Update {
            update,
            retained_from,
            read_to,
            completed: None,
        };
        match &mut self.claim {
            Some(claim) => {
                valid(claim.extends(&self.finalized, unchecked_from))?;
                claim.held.push(event);
                Ok(Accepted::Held)
            }
            None => Ok(Accepted::Forward(event)),
        }
    }

    fn append(&mut self, segment: &str) -> Result<(), RemoteClientError> {
        if !self.finalized.is_empty() {
            self.finalized.push(' ');
        }
        self.finalized.push_str(segment);
        valid(self.finalized.len() <= MAX_TEXT_BYTES)
    }

    fn claim(&mut self, transcript: Completion) -> Result<(), RemoteClientError> {
        valid(transcript.engine == self.engine && !transcript.text.trim().is_empty())?;
        match &self.claim {
            Some(claim) => valid(claim.transcript.text == transcript.text),
            None => {
                self.claim = Some(Claim {
                    transcript,
                    held: Vec::new(),
                });
                Ok(())
            }
        }
    }

    /// Complete must repeat exactly the finalized text, after the whole upload.
    /// The first held update carries the claim, so delivery sees it once.
    fn check_complete(
        &mut self,
        transcript: Completion,
        uploaded: bool,
    ) -> Result<Accepted, RemoteClientError> {
        valid(
            uploaded
                && transcript.engine == self.engine
                && !transcript.text.is_empty()
                && transcript.text == self.finalized,
        )?;
        let mut terminal = match self.claim.take() {
            Some(claim) => {
                valid(claim.transcript.text == transcript.text)?;
                claim.held
            }
            None => Vec::new(),
        };
        if let Some(Event::Update { completed, .. }) = terminal.first_mut() {
            *completed = Some(transcript.clone());
        }
        terminal.push(Event::Complete(transcript));
        Ok(Accepted::Finished(terminal))
    }
}

impl Claim {
    /// The finalized text from `unchecked_from` on is a prefix of the claim,
    /// ending on a word boundary.
    fn extends(&self, finalized: &str, unchecked_from: usize) -> bool {
        let claimed = &self.transcript.text;
        claimed.get(unchecked_from..finalized.len()) == Some(&finalized[unchecked_from..])
            && claimed
                .as_bytes()
                .get(finalized.len())
                .is_none_or(|&next| next == b' ')
    }
}

fn valid(condition: bool) -> Result<(), RemoteClientError> {
    if condition {
        Ok(())
    } else {
        Err(RemoteClientError::Response)
    }
}

/// One remote operation running on the connection's runtime. Its result
/// channel is separate so a failure preempts updates still queued.
struct Session {
    events: Receiver<Event>,
    result: Receiver<Result<(), RemoteClientError>>,
    stop: CancelToken,
}

impl Session {
    fn start(
        connection: &Connection,
        start: Start,
        recording: RecordingReader,
        cancel: CancelToken,
    ) -> Self {
        let (output, events) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (finished, result) = mpsc::sync_channel(1);
        let stop = CancelToken::new();
        let stopped = stop.clone();
        let channel = connection.channel.clone();
        connection.runtime.handle.spawn(async move {
            let cancel =
                CancelToken::from_fn(move || cancel.is_cancelled() || stopped.is_cancelled());
            let result = bounded(
                &cancel,
                OVERALL_TIMEOUT,
                stream(channel, start, recording, &output),
            )
            .await;
            let _ = finished.send(result);
        });
        Self {
            events,
            result,
            stop,
        }
    }

    fn next(&mut self, cancel: &CancelToken) -> Result<Event, RemoteClientError> {
        loop {
            if cancel.is_cancelled() {
                return Err(RemoteClientError::Cancelled);
            }
            if let Ok(Err(error)) = self.result.try_recv() {
                return Err(error);
            }
            match self.events.recv_timeout(CANCEL_POLL) {
                Ok(event) => return Ok(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(match self.result.try_recv() {
                        Ok(Err(error)) => error,
                        _ => RemoteClientError::Response,
                    });
                }
            }
        }
    }

    /// The next update the operation exposes. The device reader is filled at
    /// least through the server's read position, so local VAD and history
    /// cover the audio the server judged.
    fn step(
        &mut self,
        recording: &mut RecordingReader,
        previews: bool,
        cancel: &CancelToken,
    ) -> Result<OperationStep, TranscriptionError> {
        loop {
            match self.next(cancel).map_err(transcription_error)? {
                Event::Update {
                    update: TranscriptUpdate::Provisional { .. },
                    ..
                } if !previews => {}
                Event::Update {
                    update,
                    retained_from,
                    read_to,
                    completed,
                } => {
                    recording
                        .fill_available()
                        .map_err(TranscriptionError::Capture)?;
                    if completed.is_some() {
                        recording.fill().map_err(TranscriptionError::Capture)?;
                    } else {
                        recording
                            .fill_through(read_to)
                            .map_err(TranscriptionError::Capture)?;
                    }
                    let completed = completed
                        .map(|transcript| crate::transcription::Transcript::new(transcript.text))
                        .transpose()?;
                    return Ok(OperationStep::Update {
                        update,
                        retained_from,
                        read_to,
                        completed,
                    });
                }
                Event::Complete(transcript) => {
                    recording.fill().map_err(TranscriptionError::Capture)?;
                    let end = recording.held_to();
                    return Ok(OperationStep::Complete {
                        transcript: crate::transcription::Transcript::new(transcript.text)?,
                        retained_from: end,
                        read_to: end,
                    });
                }
            }
        }
    }
}

/// Waits for the worker, so no upload outlives the operation that owns it.
impl Drop for Session {
    fn drop(&mut self) {
        self.stop.cancel();
        let _ = self.result.recv();
    }
}

impl Transcriber for RemoteTranscriber {
    fn validate_language(
        &self,
        _language: &Language,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        if cancel.is_cancelled() {
            Err(TranscriptionError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// One bounded input, run as a whole-recording operation on the server.
    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        let mut recording = RecordingReader::from_samples(audio.require_samples("remote")?);
        Transcription::<_, Whole>::delegated(self.clone())
            .transcribe_with(&mut recording, language, cancel)
            .map(|transcript| Hypothesis::new(transcript.into_text()))
    }

    /// The server runs the operation. Its session opens on the first step,
    /// from a window fork, so audio captured before then is uploaded once.
    fn open_operation(
        &self,
        language: &Language,
        incremental: bool,
        cancel: &CancelToken,
    ) -> Result<Option<OperationSource>, TranscriptionError> {
        let binding = self.bind(cancel).map_err(transcription_error)?;
        let start = Start {
            engine: binding.engine.to_owned(),
            language: language.clone(),
            strategy: if incremental {
                binding.strategy
            } else {
                Strategy::Whole
            },
        };
        let connection = Arc::clone(&self.connection);
        let cancel = cancel.clone();
        let mut session = None;
        Ok(Some(OperationSource::new(
            binding.engine,
            move |recording, previews| {
                session
                    .get_or_insert_with(|| {
                        Session::start(
                            &connection,
                            start.clone(),
                            recording.fork_window(),
                            cancel.clone(),
                        )
                    })
                    .step(recording, previews, &cancel)
            },
        )))
    }
}

fn transcription_error(error: RemoteClientError) -> TranscriptionError {
    match error {
        RemoteClientError::Cancelled => TranscriptionError::Cancelled,
        RemoteClientError::Rpc(RpcErrorCode::NoSpeech) => TranscriptionError::NoSpeech,
        error @ (RemoteClientError::Config(_)
        | RemoteClientError::Storage
        | RemoteClientError::PairingLock
        | RemoteClientError::Invitation
        | RemoteClientError::InvalidEndpoint
        | RemoteClientError::InvalidFingerprint
        | RemoteClientError::Identity
        | RemoteClientError::DurabilityUnconfirmed
        | RemoteClientError::RevocationUnconfirmed
        | RemoteClientError::ForgetUnconfirmed(_)
        | RemoteClientError::Runtime
        | RemoteClientError::Connection
        | RemoteClientError::Rpc(_)
        | RemoteClientError::Timeout
        | RemoteClientError::Unsupported
        | RemoteClientError::Response
        | RemoteClientError::AudioTooLong
        | RemoteClientError::Audio
        | RemoteClientError::Backpressure) => TranscriptionError::Inference {
            backend: "remote",
            source: BackendError::engine(error),
        },
    }
}

async fn upload(
    start: Start,
    mut recording: RecordingReader,
    requests: tokio::sync::mpsc::Sender<wire::InferenceRequest>,
    progress: Arc<UploadProgress>,
) -> Result<(), RemoteClientError> {
    use wire::inference_request::Body;
    if recording.held_from() != 0 {
        return Err(RemoteClientError::Audio);
    }
    send_request(&requests, Body::Start(start.into())).await?;
    let mut position = 0;
    loop {
        recording
            .fill_available()
            .map_err(|_| RemoteClientError::Audio)?;
        if recording.held_to() > MAX_AUDIO_SAMPLES {
            return Err(RemoteClientError::AudioTooLong);
        }
        while position < recording.held_to() {
            let end = (position + AUDIO_CHUNK_SAMPLES).min(recording.held_to());
            let bytes = recording
                .span(position..end)
                .iter()
                .flat_map(|sample| pcm16_from_f32(*sample).to_le_bytes())
                .collect();
            send_request(&requests, Body::Audio(bytes)).await?;
            progress.sent.store(end, Ordering::Release);
            position = end;
        }
        recording.release_before(position);
        if recording.reached_end() {
            progress.finished.store(true, Ordering::Release);
            return send_request(&requests, Body::Stop(wire::InferenceStop {})).await;
        }
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

async fn send_request(
    requests: &tokio::sync::mpsc::Sender<wire::InferenceRequest>,
    body: wire::inference_request::Body,
) -> Result<(), RemoteClientError> {
    requests
        .send(wire::InferenceRequest { body: Some(body) })
        .await
        .map_err(|_| RemoteClientError::Connection)
}

pub(super) fn run<T>(
    future: impl Future<Output = Result<T, RemoteClientError>>,
) -> Result<T, RemoteClientError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| RemoteClientError::Runtime)?;
    let result = runtime.block_on(future);
    runtime.shutdown_background();
    result
}

pub(super) async fn bounded<T>(
    cancel: &CancelToken,
    timeout: Duration,
    future: impl Future<Output = Result<T, RemoteClientError>>,
) -> Result<T, RemoteClientError> {
    if cancel.is_cancelled() {
        return Err(RemoteClientError::Cancelled);
    }
    let cancelled = async {
        loop {
            tokio::time::sleep(CANCEL_POLL).await;
            if cancel.is_cancelled() {
                break;
            }
        }
    };
    tokio::select! {
        biased;
        () = cancelled => Err(RemoteClientError::Cancelled),
        result = tokio::time::timeout(timeout, future) => result.map_err(|_| RemoteClientError::Timeout)?,
    }
}

pub(super) fn identity_error(error: super::identity::Error) -> RemoteClientError {
    use super::identity::Error;
    match error {
        Error::DurabilityUnconfirmed(_) => RemoteClientError::DurabilityUnconfirmed,
        Error::Document(_)
        | Error::RevokeUnconfirmed(_)
        | Error::InvalidIdentity
        | Error::InvalidInvitation
        | Error::InvalidEndpoint
        | Error::Unpaired
        | Error::Unauthenticated
        | Error::Full
        | Error::Busy
        | Error::Stopped
        | Error::Certificate(_)
        | Error::Invite(_)
        | Error::Tls(_) => RemoteClientError::Identity,
    }
}

pub(super) fn rpc_error(status: tonic::Status) -> RemoteClientError {
    match protocol::rpc_failure(&status).map(|failure| failure.code) {
        Some(RpcErrorCode::Cancelled) => RemoteClientError::Cancelled,
        Some(RpcErrorCode::DurabilityUnconfirmed) => RemoteClientError::DurabilityUnconfirmed,
        Some(code) => RemoteClientError::Rpc(code),
        None => RemoteClientError::Connection,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::inference::host::tests::{Fixture, Observed};
    use crate::transcription::{Stream, Streaming};
    use aria_kit::audio::Recording;
    use aria_kit::transcription::{AudioInputKind, Cadence, TimedWord, TranscriptSegment};

    #[tokio::test]
    async fn live_native_words_arrive_before_stop_and_use_one_server_stream() {
        let server = Fixture::start().await;
        let client = server.paired_client().await;
        let observed = Arc::clone(&server.observed);
        let (mut recording, mut writer) = Recording::open();
        writer.push(&[0.5; 320]);
        let (first, received) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let transcription = Transcription::<_, Streaming>::delegated(client.transcriber());
            let mut operation = transcription.start(&Language::Automatic).unwrap();
            assert_eq!(operation.served_by(), Some("whisper"));
            let segment = operation.next_finalized(&mut recording).unwrap().unwrap();
            assert!(!recording.reached_end());
            assert_eq!(segment.span(), None);
            assert_eq!(segment.word_spans(), None);
            assert_eq!(segment.source_window(), Some(0..160));
            first.send(segment.into_text()).unwrap();
            let transcript = operation.finish(&mut recording).unwrap();
            assert!(recording.reached_end());
            transcript.into_text()
        });
        let first = tokio::time::timeout(Duration::from_secs(3), received)
            .await
            .unwrap()
            .unwrap();
        assert!(first.contains("native"));
        writer.finish();
        let text = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert!(text.starts_with(&first));
        assert_eq!(observed.native.load(Ordering::SeqCst), 1);
        assert_eq!(observed.calls.load(Ordering::SeqCst), 0);
        server.stop().await;
    }

    #[tokio::test]
    async fn saved_native_audio_drains_bursts_after_a_slow_consumer() {
        let server = Fixture::start().await;
        let client = server.paired_client().await;
        let task = tokio::task::spawn_blocking(move || {
            let remote = client.transcriber();
            let transcription = Transcription::<_, Streaming>::delegated(remote);
            let mut recording = RecordingReader::from_samples(&[0.5; 32_000]);
            let mut operation = transcription.start(&Language::Automatic).unwrap();
            assert!(operation.next_finalized(&mut recording).unwrap().is_some());
            std::thread::sleep(Duration::from_millis(150));
            let transcript = operation.finish(&mut recording).unwrap();
            assert_eq!(transcript.as_str().split_whitespace().count(), 402);
        });
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(server.observed.native.load(Ordering::SeqCst), 1);
        server.stop().await;
    }

    struct Agreement;

    impl Transcriber for Agreement {
        fn validate_language(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn transcribe_with(
            &self,
            _: TranscriptionAudio<'_>,
            _: &Language,
            _: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            Ok(Hypothesis::with_words(
                "hello world",
                vec![
                    TimedWord {
                        start: 0.0,
                        end: 0.01,
                    },
                    TimedWord {
                        start: 0.01,
                        end: 0.02,
                    },
                ],
            ))
        }
    }

    #[tokio::test]
    async fn normal_streaming_dictation_previews_delivers_and_keeps_history() {
        use crate::audio::{ReplayPace, WavReplayCapture, fixture_of};
        use crate::dictation::testing::{CallLog, Loud, settings};
        use crate::dictation::{
            DeliveryMode, DictationEvent, DictationEventEmitter, DictationPipeline, DictationStage,
            ExecutionControl, OutputMode, StopMode,
        };

        let server = Fixture::with_model(
            "whisper",
            AudioInputKind::Samples,
            Some(Stream::Agreement(Cadence::from_millis(100, 1000))),
            Box::new(Agreement),
            Arc::new(Observed::default()),
        )
        .await;
        let client = server.paired_client().await;
        let audio = tempfile::tempdir().unwrap();
        let fixture = fixture_of(audio.path(), 32_000);
        let capture = WavReplayCapture::new(
            audio.path(),
            Duration::from_secs(5),
            &fixture,
            ReplayPace::Realtime,
        )
        .unwrap();
        let log = CallLog::default();
        let delivered = log.clone();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let running = Arc::clone(&control);
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::task::spawn_blocking(move || {
            let remote = client.transcriber();
            let pipeline = DictationPipeline::new(
                capture,
                Transcription::<_, Streaming>::delegated(remote),
                (log.history(), log.cursor()),
            )
            .hearing_speech(Some(Loud));
            let events = DictationEventEmitter::forwarded_by(move |event| {
                if matches!(
                    &event,
                    DictationEvent::Ghost(_)
                        | DictationEvent::Stage(DictationStage::SegmentDelivered)
                ) {
                    let _ = sender.send(event);
                }
            });
            pipeline
                .execute(
                    settings(DeliveryMode::Streaming, OutputMode::Insert),
                    &running,
                    &events,
                    None,
                )
                .unwrap()
        });
        let mut previewed = false;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match events.recv().await.unwrap() {
                    DictationEvent::Ghost(ghost) if ghost.text.as_ref() == "hello world" => {
                        previewed = true;
                    }
                    DictationEvent::Stage(DictationStage::SegmentDelivered) => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert!(previewed);
        assert!(
            delivered
                .entries()
                .contains(&"deliver:hello world".to_owned())
        );
        assert!(control.request_recording_stop());
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.text, "hello world");
        let entries = delivered.entries();
        assert!(entries.contains(&"engine:whisper".to_owned()));
        assert!(entries.contains(&"completed:hello world".to_owned()));
        server.stop().await;
    }

    #[tokio::test]
    async fn a_whole_only_server_serves_streaming_dictation_under_its_own_engine() {
        use std::sync::Mutex;

        use crate::dictation::testing::{CallLog, Loud, settings};
        use crate::dictation::{
            DeliveryMode, DictationEventEmitter, DictationPipeline, ExecutionControl, OutputMode,
            StopMode,
        };
        use aria_kit::audio::{AudioCapture, CaptureError, RecordingMode, StopSignal};

        struct Capture(Mutex<Option<RecordingReader>>);
        impl AudioCapture for Capture {
            fn start(
                &self,
                _: RecordingMode,
                _: StopSignal,
            ) -> Result<RecordingReader, CaptureError> {
                Ok(self.0.lock().unwrap().take().unwrap())
            }
        }

        let server = Fixture::with_model(
            "parakeet",
            AudioInputKind::Samples,
            None,
            Box::new(Agreement),
            Arc::new(Observed::default()),
        )
        .await;
        let client = server.paired_client().await;
        let (recording, mut writer) = Recording::open();
        writer.push(&[0.5; 1600]);
        writer.finish();
        let log = CallLog::default();
        let delivered = log.clone();
        let result = tokio::task::spawn_blocking(move || {
            DictationPipeline::new(
                Capture(Mutex::new(Some(recording))),
                Transcription::<_, Streaming>::delegated(client.transcriber()),
                (log.history(), log.cursor()),
            )
            .hearing_speech(Some(Loud))
            .execute(
                settings(DeliveryMode::Streaming, OutputMode::Insert),
                &ExecutionControl::for_test(StopMode::Manual),
                &DictationEventEmitter::unsubscribed(),
                None,
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.text, "hello world");
        let entries = delivered.entries();
        assert!(entries.contains(&"engine:parakeet".to_owned()));
        assert!(entries.contains(&"deliver:hello world".to_owned()));
        server.stop().await;
    }

    #[tokio::test]
    async fn later_speech_does_not_validate_a_delayed_silence_guess() {
        use std::sync::Mutex;

        use crate::dictation::testing::{CallLog, Loud, settings};
        use crate::dictation::{
            DeliveryMode, DictationEventEmitter, DictationPipeline, ExecutionControl, OutputMode,
            StopMode,
        };
        use aria_kit::audio::{AudioCapture, CaptureError, RecordingMode, StopSignal};
        use aria_kit::transcription::{ChunkStream, Emitted};

        struct Capture(Mutex<Option<RecordingReader>>);
        impl AudioCapture for Capture {
            fn start(
                &self,
                _: RecordingMode,
                _: StopSignal,
            ) -> Result<RecordingReader, CaptureError> {
                Ok(self.0.lock().unwrap().take().unwrap())
            }
        }

        struct DecodeGate {
            entered: tokio::sync::Notify,
            release: Mutex<mpsc::Receiver<()>>,
        }
        struct Delayed(Arc<DecodeGate>);
        impl Transcriber for Delayed {
            fn validate_language(
                &self,
                _: &Language,
                _: &CancelToken,
            ) -> Result<(), TranscriptionError> {
                Ok(())
            }
            fn transcribe_with(
                &self,
                _: TranscriptionAudio<'_>,
                _: &Language,
                _: &CancelToken,
            ) -> Result<Hypothesis, TranscriptionError> {
                panic!("the native decoder owns this recording")
            }
            fn open_stream(
                &self,
                _: &Language,
                _: &CancelToken,
            ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
                Ok(Some(Box::new(Decoder {
                    gate: Arc::clone(&self.0),
                    first: true,
                })))
            }
        }
        struct Decoder {
            gate: Arc<DecodeGate>,
            first: bool,
        }
        impl ChunkStream for Decoder {
            fn chunk_samples(&self) -> usize {
                1600
            }
            fn feed(&mut self, audio: &[f32]) -> Result<Emitted, TranscriptionError> {
                let words = if std::mem::take(&mut self.first) {
                    assert!(audio.iter().all(|sample| *sample == 0.0));
                    self.gate.entered.notify_one();
                    self.gate
                        .release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap();
                    Hypothesis::with_words(
                        "Mm-hmm. hello",
                        vec![
                            TimedWord {
                                start: 0.0,
                                end: 0.05,
                            },
                            TimedWord {
                                start: 0.05,
                                end: 0.1,
                            },
                        ],
                    )
                } else {
                    Hypothesis::new("")
                };
                Ok(Emitted {
                    words,
                    joins_previous: false,
                })
            }
        }

        for delivery in [DeliveryMode::Complete, DeliveryMode::Streaming] {
            let (release, released) = mpsc::channel();
            let gate = Arc::new(DecodeGate {
                entered: tokio::sync::Notify::new(),
                release: Mutex::new(released),
            });
            let server = Fixture::with_model(
                "whisper",
                AudioInputKind::Samples,
                Some(Stream::Native),
                Box::new(Delayed(Arc::clone(&gate))),
                Arc::new(Observed::default()),
            )
            .await;
            let client = server.paired_client().await;
            let (recording, mut writer) = Recording::open();
            writer.push(&[0.0; 1600]);
            let log = CallLog::default();
            let delivered = log.clone();
            let task = tokio::task::spawn_blocking(move || {
                let remote = client.transcriber();
                let pipeline = DictationPipeline::new(
                    Capture(Mutex::new(Some(recording))),
                    Transcription::<_, Streaming>::delegated(remote),
                    (log.history(), log.cursor()),
                )
                .hearing_speech(Some(Loud));
                pipeline
                    .execute(
                        settings(delivery, OutputMode::Insert),
                        &ExecutionControl::for_test(StopMode::Manual),
                        &DictationEventEmitter::unsubscribed(),
                        None,
                    )
                    .unwrap()
            });
            tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
                .await
                .unwrap();
            writer.push(&[0.5; 1600]);
            writer.finish();
            release.send(()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            server.stop().await;
            assert_eq!(result.text, "hello", "{delivery:?}");
            let text: String = delivered
                .entries()
                .iter()
                .filter_map(|entry| entry.strip_prefix("deliver:"))
                .collect();
            assert_eq!(text, "hello", "{delivery:?}");
            assert!(delivered.entries().contains(&"completed:hello".to_owned()));
        }
    }

    #[tokio::test]
    async fn cancelling_idle_audio_joins_the_remote_worker_without_ending_capture() {
        let server = Fixture::start().await;
        let client = server.paired_client().await;
        let (mut recording, writer) = Recording::open();
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let task = tokio::task::spawn_blocking(move || {
            let remote = client.transcriber();
            let transcription = Transcription::<_, Streaming>::delegated(remote);
            let mut operation = transcription
                .start_with(&Language::Automatic, &worker_cancel)
                .unwrap();
            assert!(matches!(
                operation.next_update(&mut recording),
                Err(TranscriptionError::Cancelled)
            ));
            assert!(recording.ended_at().is_none());
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while server.observed.native.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        writer.finish();
        server.stop().await;
    }

    /// The two halves of [`ResponseValidator::accept`], as the stream feeds them.
    impl ResponseValidator {
        fn update(
            &mut self,
            update: TranscriptUpdate,
            retained_from: u64,
            read_to: u64,
            completed: Option<Completion>,
            sent: u64,
        ) -> Result<Option<Event>, RemoteClientError> {
            let event = Event::Update {
                update,
                retained_from,
                read_to,
                completed,
            };
            Ok(match self.accept(event, sent, false)? {
                Accepted::Forward(event) => Some(event),
                Accepted::Held => None,
                Accepted::Finished(_) => unreachable!("an update never finishes the stream"),
            })
        }

        fn complete(
            &mut self,
            transcript: Completion,
            uploaded: bool,
        ) -> Result<Vec<Event>, RemoteClientError> {
            match self.accept(Event::Complete(transcript), 0, uploaded)? {
                Accepted::Finished(terminal) => Ok(terminal),
                Accepted::Forward(_) | Accepted::Held => unreachable!("Complete ends the stream"),
            }
        }
    }

    fn transcript(text: &str) -> Completion {
        Completion {
            text: text.into(),
            engine: "whisper".into(),
        }
    }

    fn finalized(text: &str, span: std::ops::Range<u64>) -> TranscriptUpdate {
        TranscriptUpdate::Finalized(TranscriptSegment::new(text, Some(span)).unwrap())
    }

    #[test]
    fn a_completion_claim_holds_the_tail_until_complete() {
        let words = QUEUE_CAPACITY * 3;
        let text = vec!["word"; words].join(" ");
        let mut responses = ResponseValidator::new("whisper");
        for index in 0..words as u64 {
            assert!(
                responses
                    .update(
                        finalized("word", index..index + 1),
                        index,
                        words as u64,
                        Some(transcript(&text)),
                        words as u64,
                    )
                    .unwrap()
                    .is_none()
            );
        }
        let held = &responses.claim.as_ref().unwrap().held;
        assert_eq!(held.len(), words);
        assert!(held.iter().all(|event| matches!(
            event,
            Event::Update {
                completed: None,
                ..
            }
        )));
        let terminal = responses.complete(transcript(&text), true).unwrap();
        assert_eq!(terminal.len(), words + 1);
        assert!(matches!(
            terminal.first(),
            Some(Event::Update {
                completed: Some(_),
                ..
            })
        ));
        assert!(terminal[1..words].iter().all(|event| matches!(
            event,
            Event::Update {
                completed: None,
                ..
            }
        )));
        assert!(matches!(terminal.last(), Some(Event::Complete(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_output_times_out() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        assert!(
            sender
                .try_send(Event::Complete(transcript("first")))
                .is_ok()
        );
        let began = tokio::time::Instant::now();
        assert!(matches!(
            send_event(&sender, Event::Complete(transcript("second"))).await,
            Err(RemoteClientError::Backpressure)
        ));
        assert_eq!(began.elapsed(), OUTPUT_STALL_TIMEOUT);
    }

    #[test]
    fn responses_reject_regressing_unread_or_released_positions() {
        let mut responses = ResponseValidator::new("whisper");
        responses
            .update(finalized("hello", 0..10), 5, 10, None, 20)
            .unwrap();
        for (retained_from, read_to, span) in [
            (5, 9, 5..9),
            (4, 10, 5..10),
            (5, 10, 4..10),
            (5, 21, 5..20),
            (5, 10, 5..11),
        ] {
            assert!(
                responses
                    .update(finalized("world", span), retained_from, read_to, None, 20)
                    .is_err()
            );
        }
        assert!(
            responses
                .update(finalized("world", 10..20), 10, 20, None, 20)
                .unwrap()
                .is_some()
        );
        assert!(responses.complete(transcript("hello world"), true).is_ok());
    }

    #[test]
    fn responses_reject_inconsistent_completion_and_oversized_text() {
        let mut responses = ResponseValidator::new("whisper");
        assert!(
            responses
                .update(
                    finalized("hello", 0..10),
                    0,
                    20,
                    Some(transcript("different")),
                    20
                )
                .is_err()
        );
        let mut responses = ResponseValidator::new("whisper");
        responses
            .update(finalized("hello", 0..10), 0, 20, None, 20)
            .unwrap();
        assert!(responses.complete(transcript("hello"), false).is_err());
        let mut responses = ResponseValidator::new("whisper");
        responses
            .update(finalized("hello", 0..10), 0, 20, None, 20)
            .unwrap();
        assert!(responses.complete(transcript("different"), true).is_err());

        let mut responses = ResponseValidator::new("whisper");
        let mut wrong_engine = transcript("hello");
        wrong_engine.engine = "parakeet".into();
        assert!(
            responses
                .update(finalized("hello", 0..10), 0, 20, Some(wrong_engine), 20)
                .is_err()
        );

        let mut responses = ResponseValidator::new("whisper");
        responses
            .update(
                finalized("hello", 0..10),
                0,
                20,
                Some(transcript("hello")),
                20,
            )
            .unwrap();
        assert!(matches!(
            responses.update(
                TranscriptUpdate::Provisional {
                    text: String::new(),
                    source_window: None
                },
                0,
                20,
                None,
                20
            ),
            Err(RemoteClientError::Response)
        ));

        let mut responses = ResponseValidator::new("whisper");
        let maximum = "x".repeat(MAX_TEXT_BYTES);
        responses
            .update(finalized(&maximum, 0..10), 0, 20, None, 20)
            .unwrap();
        assert!(
            responses
                .update(finalized("more", 10..20), 0, 20, None, 20)
                .is_err()
        );
    }

    #[test]
    fn a_disconnected_session_preserves_its_worker_error() {
        let (sender, events) = mpsc::sync_channel(1);
        let (finished, result) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            finished.send(Err(RemoteClientError::Identity)).unwrap();
            drop(sender);
        });
        let mut session = Session {
            events,
            result,
            stop: CancelToken::new(),
        };
        assert!(matches!(
            session.next(&CancelToken::never()),
            Err(RemoteClientError::Identity)
        ));
        drop(session);
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn own_voice_rejection_precedes_upload_and_corrections_stay_on_device() {
        use crate::dictation::DictationPipeline;
        use crate::dictation::testing::Loud;
        use crate::transcription::Transcription;
        use crate::vocabulary::{PersonalRules, Vocabulary};
        use aria_kit::speaker::{Diarizer, Embedder, SpeakerError, VoiceprintTracker};

        struct Voice([f32; 2]);
        impl Embedder for Voice {
            fn embed(&mut self, _: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
                Ok(Some(self.0.to_vec()))
            }
            fn dimension(&self) -> usize {
                2
            }
        }

        let server = super::super::host::tests::Fixture::start().await;
        let client = server.paired_client().await;
        let observed = std::sync::Arc::clone(&server.observed);
        tokio::task::spawn_blocking(move || {
            let samples = vec![0.25; 64_000];
            let matcher = Vocabulary::compile(
                &[],
                PersonalRules::new(
                    vec![],
                    vec![("raw model words".into(), "ProjectOnly".into())],
                ),
            )
            .unwrap();
            for (voice, accepted) in [([0.0, 1.0], false), ([1.0, 0.0], true)] {
                let enrolled = vec![aria_kit::speaker::enrolled_voice(
                    "owner",
                    crate::speaker::Role::Owner,
                    vec![1.0, 0.0],
                )];
                let turns = VoiceprintTracker::new(Diarizer::new(Voice(voice), enrolled))
                    .with_segmenter(aria_kit::speaker::LevelSegmenter)
                    .unwrap();
                let remote = client.transcriber();
                let pipeline =
                    DictationPipeline::new((), Transcription::<_, Whole>::delegated(remote), ())
                        .hearing_speech(Some(Loud))
                        .tracking_voices(Some(turns))
                        .editing(crate::dictation::TextLayers::new().removing_disfluencies(true));
                let mut recording = crate::audio::RecordingReader::from_samples(&samples);
                let result = pipeline.transcribe(
                    &mut recording,
                    &Language::Automatic,
                    true,
                    Some(&matcher),
                    "test",
                    &CancelToken::never(),
                );
                if accepted {
                    assert_eq!(result.unwrap().transcript.as_str(), "ProjectOnly");
                    assert_eq!(
                        observed.samples.lock().unwrap().as_slice(),
                        std::slice::from_ref(&samples)
                    );
                } else {
                    assert!(matches!(
                        result,
                        Err(crate::dictation::DictationError::NoSpeech)
                    ));
                    assert!(observed.samples.lock().unwrap().is_empty());
                }
            }
        })
        .await
        .unwrap();
        server.stop().await;
    }

    #[tokio::test]
    async fn upload_preserves_pcm_and_explicit_completion_with_bounded_chunks() {
        let samples: Vec<_> = (0..AUDIO_CHUNK_SAMPLES + 1)
            .map(|index| if index % 2 == 0 { 0.5 } else { -0.5 })
            .collect();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let progress = Arc::new(UploadProgress::default());
        let uploading = upload(
            Start {
                engine: "test".into(),
                language: Language::Automatic,
                strategy: Strategy::Streaming,
            },
            RecordingReader::from_samples(&samples),
            sender,
            Arc::clone(&progress),
        );
        let collecting = async {
            let mut messages = Vec::new();
            while let Some(message) = receiver.recv().await {
                messages.push(message);
            }
            messages
        };
        let (result, messages) = tokio::join!(uploading, collecting);
        result.unwrap();
        assert!(progress.finished.load(Ordering::Acquire));
        assert_eq!(progress.sent.load(Ordering::Acquire), samples.len() as u64);
        assert_eq!(messages.len(), 4);
        let Some(wire::inference_request::Body::Start(start)) = &messages[0].body else {
            panic!("opening start");
        };
        assert_eq!(start.strategy, i32::from(Strategy::Streaming));
        let chunks: Vec<_> = messages[1..3]
            .iter()
            .flat_map(|message| {
                let Some(wire::inference_request::Body::Audio(bytes)) = &message.body else {
                    panic!("audio chunk");
                };
                assert!(bytes.len() as u64 <= AUDIO_CHUNK_SAMPLES * 2);
                bytes
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            })
            .collect();
        assert_eq!(
            chunks,
            samples
                .iter()
                .map(|sample| pcm16_from_f32(*sample))
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            messages.last().unwrap().body,
            Some(wire::inference_request::Body::Stop(_))
        ));
    }

    #[test]
    fn cancellation_and_deadline_stop_a_stalled_remote_request() {
        let cancel = CancelToken::new();
        run(async {
            let request = bounded(
                &cancel,
                OVERALL_TIMEOUT,
                std::future::pending::<Result<(), RemoteClientError>>(),
            );
            let trigger = async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                cancel.cancel();
            };
            let (result, ()) = tokio::join!(request, trigger);
            assert!(matches!(result, Err(RemoteClientError::Cancelled)));
            let result = bounded(
                &CancelToken::never(),
                Duration::from_millis(1),
                std::future::pending::<Result<(), RemoteClientError>>(),
            )
            .await;
            assert!(matches!(result, Err(RemoteClientError::Timeout)));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn remote_errors_do_not_repeat_untrusted_messages() {
        let status: tonic::Status =
            crate::rpc::RpcFailure::new(RpcErrorCode::Internal, "private transcript").into();
        let error = rpc_error(status);
        assert_eq!(error.to_string(), "remote operation failed (internal)");
        assert!(!format!("{error:?}").contains("private transcript"));
    }

    #[test]
    fn an_unpaired_home_builds_nothing_and_creates_no_credentials() {
        let home = tempfile::tempdir().unwrap();
        assert!(matches!(
            RemoteTranscriber::open_pinned(
                home.path(),
                &ServerEndpoint::parse("https://server.example").unwrap(),
            ),
            Err(RemoteClientError::Identity)
        ));
        assert!(!home.path().join(".aria/inference/client.json").exists());
    }
}
