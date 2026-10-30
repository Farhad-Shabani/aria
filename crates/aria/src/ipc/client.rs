//! Typed gRPC client for the Aria daemon.
//!
//! Synchronous: each client owns a single-threaded tokio runtime and blocks on it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tonic::transport::{Channel, Endpoint};

use crate::chat::{AskTarget, AssistantModel, ChatRequest, ChatStage, Ticket};
use crate::config::AssistantId;
use crate::dictation::{DictationOutcome, DictationRequest, DictationStage, RecordingId};
use crate::enrollment::{EnrolledSpeaker, EnrollmentRequest, EnrollmentResult, EnrollmentStage};
use crate::listen::ListenStage;
use crate::preferences::{PreferencePatch, PreferenceSnapshot, UpdateResult};
use crate::rpc::{RpcErrorCode, RpcFailure};
use crate::speech::{SpeechOutcome, SpeechRequest, SpeechStage};

use super::{protocol, wire};

use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;

/// Bounded by the longest dictation: recording, transcription, delivery.
const DICTATION_TIMEOUT: Duration = Duration::from_secs(670);
/// Base client deadline for one utterance; grows with the text.
const SPEECH_BASE_TIMEOUT: Duration = Duration::from_secs(45);
const SPEECH_TIMEOUT_BYTES_PER_SECOND: u64 = 10;
/// For everything that answers immediately: preferences, subscriptions.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(40);
/// Stop waits for the listen session's final decode to drain.
const LISTEN_STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// One enrollment sample waits out the daemon's no-speech timeout and then
/// however long the person speaks. Ten samples is the ceiling the daemon
/// accepts, and the margin is for the model load before the first one.
const ENROLL_TIMEOUT: Duration = Duration::from_secs(10 * 45 + 60);
const READINESS_TIMEOUT: Duration = Duration::from_secs(1);

/// A tokio runtime that is safe to drop anywhere: plain `Runtime` drop
/// blocks — and panics inside another runtime's async context.
struct ClientRuntime {
    runtime: Option<tokio::runtime::Runtime>,
}

impl ClientRuntime {
    fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        Ok(Self {
            runtime: Some(runtime),
        })
    }

    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.runtime
            .as_ref()
            .expect("the runtime exists until drop")
            .block_on(future)
    }

    fn enter(&self) -> tokio::runtime::EnterGuard<'_> {
        self.runtime
            .as_ref()
            .expect("the runtime exists until drop")
            .enter()
    }
}

impl Drop for ClientRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("failed to establish daemon connection at {}", path.display())]
pub struct ConnectError {
    path: PathBuf,
    #[source]
    source: std::io::Error,
}

impl ConnectError {
    pub(crate) fn is_daemon_unavailable(&self) -> bool {
        matches!(
            self.source.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("failed to build the daemon client runtime")]
    Runtime(#[source] std::io::Error),

    #[error("{}", code.reason(message))]
    Rpc { code: RpcErrorCode, message: String },

    #[error("daemon request failed with status {}: {}", .0.code().description(), .0.message())]
    Status(tonic::Status),

    #[error("the daemon is not ready to serve")]
    NotServing,

    #[error("the running daemon does not provide this service")]
    NotProvided,

    #[error("daemon request timed out after {}s", .0.as_secs())]
    Timeout(Duration),

    #[error("daemon returned a malformed response: {0}")]
    MalformedResponse(String),
}

impl ClientError {
    pub(crate) fn is_transient_readiness_failure(&self) -> bool {
        match self {
            Self::NotServing | Self::Timeout(_) => true,
            // Cancelled covers a daemon torn down mid-request.
            Self::Status(status) => matches!(
                status.code(),
                tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Cancelled
            ),
            Self::Runtime(_)
            | Self::Rpc { .. }
            | Self::NotProvided
            | Self::MalformedResponse(_) => false,
        }
    }
}

pub(crate) struct DaemonClient {
    runtime: Arc<ClientRuntime>,
    channel: Channel,
}

impl DaemonClient {
    /// Probes the socket synchronously, so an absent daemon surfaces here;
    /// the gRPC channel itself connects on first use.
    pub(crate) fn connect(socket_path: impl AsRef<Path>) -> Result<Self, ConnectError> {
        let socket_path = socket_path.as_ref();
        let probe = std::os::unix::net::UnixStream::connect(socket_path).map_err(|source| {
            ConnectError {
                path: socket_path.to_path_buf(),
                source,
            }
        })?;
        drop(probe);

        let runtime = ClientRuntime::new().map_err(|source| ConnectError {
            path: socket_path.to_path_buf(),
            source,
        })?;
        let endpoint = Endpoint::from_shared(format!("unix://{}", socket_path.display())).map_err(
            |error| ConnectError {
                path: socket_path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error),
            },
        )?;
        // The channel's background worker spawns onto the ambient runtime.
        let channel = {
            let _runtime_context = runtime.enter();
            endpoint.connect_lazy()
        };

        Ok(Self {
            runtime: Arc::new(runtime),
            channel,
        })
    }

    /// Checks once whether the daemon is healthy. The caller retries on
    /// [`ClientError::is_transient_readiness_failure`].
    pub(crate) fn check_ready(&mut self) -> Result<(), ClientError> {
        self.check_service_ready("")
    }
}

/// Where a daemon is in its life, judged once from its socket.
pub(crate) enum Liveness {
    /// Nothing accepts on the socket.
    NotRunning(ConnectError),
    /// The socket accepts but the daemon has not reported readiness yet.
    Starting(ClientError),
    Ready(DaemonClient),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProbeError {
    #[error(transparent)]
    Connect(ConnectError),

    #[error(transparent)]
    Readiness(ClientError),
}

/// One liveness check. Callers that wait loop over it with their own deadline.
pub(crate) fn probe_daemon(socket_path: &Path) -> Result<Liveness, ProbeError> {
    let mut client = match DaemonClient::connect(socket_path) {
        Ok(client) => client,
        Err(error) if error.is_daemon_unavailable() => return Ok(Liveness::NotRunning(error)),
        Err(error) => return Err(ProbeError::Connect(error)),
    };
    match client.check_ready() {
        Ok(()) => Ok(Liveness::Ready(client)),
        Err(error) if error.is_transient_readiness_failure() => Ok(Liveness::Starting(error)),
        Err(error) => Err(ProbeError::Readiness(error)),
    }
}

impl DaemonClient {
    pub(crate) fn check_service_ready(&mut self, service: &str) -> Result<(), ClientError> {
        match self.service_status(service)? {
            ServingStatus::Serving => Ok(()),
            ServingStatus::Unknown | ServingStatus::NotServing => Err(ClientError::NotServing),
            // The daemon never registered the service, so waiting will not help.
            ServingStatus::ServiceUnknown => Err(ClientError::NotProvided),
        }
    }

    pub(crate) fn service_status(&mut self, service: &str) -> Result<ServingStatus, ClientError> {
        let mut health = HealthClient::new(self.channel.clone());
        let request = HealthCheckRequest {
            service: service.to_owned(),
        };
        let response = self
            .runtime
            .block_on(with_deadline(READINESS_TIMEOUT, health.check(request)))?;

        match response {
            Ok(response) => Ok(response.into_inner().status()),
            // The health service answers NOT_FOUND for a name nobody registered.
            Err(status) if status.code() == tonic::Code::NotFound => {
                Ok(ServingStatus::ServiceUnknown)
            }
            Err(status) => Err(status_error(status)),
        }
    }

    /// Runs one press to its end. Each stage passes through `observe`
    /// first, so a caller can act on the recording's start and end.
    ///
    /// One deadline spans the whole press, from opening the call to its
    /// last message. The daemon ends a press sooner itself; this is the
    /// backstop for one that stopped answering. tonic's own timeout would
    /// not do: it covers the call only until the stream is handed back.
    pub(crate) fn request_dictation(
        &self,
        request: DictationRequest,
        observe: impl FnMut(&DictationStage),
    ) -> Result<DictationOutcome, ClientError> {
        let mut dictation =
            wire::dictation_service_client::DictationServiceClient::new(self.channel.clone());
        // Built inside the runtime: the timer needs its driver.
        let (deadline, opened) = self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + DICTATION_TIMEOUT;
            let opened = tokio::time::timeout_at(
                deadline,
                dictation.start(wire::StartDictationRequest::from(request)),
            )
            .await;
            (deadline, opened)
        });
        let mut stream = opened
            .map_err(|_elapsed| ClientError::Timeout(DICTATION_TIMEOUT))?
            .map_err(status_error)?
            .into_inner();

        let messages = std::iter::from_fn(|| {
            let next = self
                .runtime
                .block_on(async { tokio::time::timeout_at(deadline, stream.message()).await });
            match next {
                Ok(Ok(Some(message))) => Some(Ok(message)),
                Ok(Ok(None)) => None,
                Ok(Err(status)) => Some(Err(status_error(status))),
                Err(_elapsed) => Some(Err(ClientError::Timeout(DICTATION_TIMEOUT))),
            }
        });
        outcome_from(messages, observe)
    }

    pub(crate) fn request_speech(
        &self,
        request: SpeechRequest,
    ) -> Result<SpeechOutcome, ClientError> {
        let deadline = SPEECH_BASE_TIMEOUT
            + Duration::from_secs(
                request.text.as_str().len() as u64 / SPEECH_TIMEOUT_BYTES_PER_SECOND,
            );
        let mut speech =
            wire::speech_service_client::SpeechServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                deadline,
                speech.speak(request_with_timeout(
                    wire::SpeakRequest::from(request),
                    deadline,
                )),
            ))?
            .map_err(status_error)?;

        SpeechOutcome::try_from(response.into_inner())
            .map_err(|error| ClientError::MalformedResponse(error.to_string()))
    }

    /// Ends the named recording; answers whether it was the one running.
    /// The `request_dictation` it belongs to then ends with the transcript.
    pub(crate) fn stop_dictation(&self, recording: RecordingId) -> Result<bool, ClientError> {
        let mut dictation =
            wire::dictation_service_client::DictationServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                dictation.stop(request_with_timeout(
                    wire::StopDictationRequest {
                        recording_id: recording.0,
                    },
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;

        Ok(response.into_inner().was_recording)
    }

    pub(crate) fn list_assistant_models(
        &self,
        assistant_id: AssistantId,
    ) -> Result<Vec<AssistantModel>, ClientError> {
        let mut chat = wire::chat_service_client::ChatServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                MODEL_LIST_TIMEOUT,
                chat.list_models(request_with_timeout(
                    wire::ListAssistantModelsRequest {
                        assistant_id: wire::AssistantId::from(assistant_id) as i32,
                    },
                    MODEL_LIST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;
        response
            .into_inner()
            .models
            .into_iter()
            .map(|model| {
                let model = AssistantModel {
                    id: model.id,
                    name: model.name,
                };
                if !model.is_valid() {
                    return Err(ClientError::MalformedResponse(
                        "invalid assistant model".to_owned(),
                    ));
                }
                Ok(model)
            })
            .collect()
    }

    /// Where an ask goes when the caller has no working directory.
    pub(crate) fn resolve_ask(&self) -> Result<AskTarget, ClientError> {
        let mut chat = wire::chat_service_client::ChatServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                chat.resolve(request_with_timeout(
                    wire::ResolveAskRequest {},
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;

        AskTarget::try_from(response.into_inner())
            .map_err(|error| ClientError::MalformedResponse(error.to_string()))
    }

    /// Answers with the ticket as soon as the ask is taken; the reply
    /// follows on the event stream.
    pub(crate) fn ask(&self, request: ChatRequest) -> Result<Ticket, ClientError> {
        let mut chat = wire::chat_service_client::ChatServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                chat.ask(request_with_timeout(
                    wire::AskRequest::from(request),
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;

        Ok(Ticket::from(response.into_inner()))
    }

    /// Ends the ask; answers whether it was still pending.
    pub(crate) fn cancel_ask(&self, ticket: Ticket) -> Result<bool, ClientError> {
        let mut chat = wire::chat_service_client::ChatServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                chat.cancel(request_with_timeout(
                    wire::CancelAskRequest::from(ticket),
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;

        Ok(response.into_inner().was_pending)
    }

    /// Stops the current utterance, if any; answers whether one was running.
    pub(crate) fn stop_speech(&self) -> Result<bool, ClientError> {
        let mut speech =
            wire::speech_service_client::SpeechServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                speech.stop(request_with_timeout(
                    wire::StopSpeechRequest {},
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;

        Ok(response.into_inner().stop_requested)
    }

    pub(crate) fn list_history(
        &self,
        query: crate::history::HistoryQuery,
    ) -> Result<crate::history::HistoryPage, ClientError> {
        let mut history =
            wire::history_service_client::HistoryServiceClient::new(self.channel.clone());
        let request = wire::ListHistoryRequest {
            query: query.query,
            before_id: query.before_id,
            limit: u32::try_from(query.limit).map_err(|_| ClientError::Rpc {
                code: RpcErrorCode::InvalidRequest,
                message: "invalid history limit".to_owned(),
            })?,
        };
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                history.list(request_with_timeout(request, REQUEST_TIMEOUT)),
            ))?
            .map_err(status_error)?;
        response
            .into_inner()
            .try_into()
            .map_err(|error: protocol::ProtocolError| {
                ClientError::MalformedResponse(error.to_string())
            })
    }

    pub(crate) fn clear_history(&self) -> Result<crate::history::ClearHistoryResult, ClientError> {
        let mut history =
            wire::history_service_client::HistoryServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                history.clear(request_with_timeout(
                    wire::ClearHistoryRequest {},
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?
            .into_inner();
        Ok(crate::history::ClearHistoryResult {
            deleted_entries: response.deleted_entry_count,
            cleanup_pending: response.audio_cleanup_pending,
        })
    }

    pub(crate) fn retry_history(
        &self,
        id: u64,
        options: crate::history::RetryOptions,
    ) -> Result<crate::history::HistoryEntry, ClientError> {
        let mut history =
            wire::history_service_client::HistoryServiceClient::new(self.channel.clone());
        let request = wire::RegenerateHistoryRequest {
            id,
            engine_id: options.engine,
            language: options
                .language
                .map(|language| language.as_str().to_owned()),
        };
        let response = self
            .runtime
            .block_on(with_deadline(
                DICTATION_TIMEOUT,
                history.regenerate(request_with_timeout(request, DICTATION_TIMEOUT)),
            ))?
            .map_err(status_error)?;
        response
            .into_inner()
            .try_into()
            .map_err(|error: protocol::ProtocolError| {
                ClientError::MalformedResponse(error.to_string())
            })
    }

    pub(crate) fn vocabulary_status(
        &self,
        directory: Option<&crate::vocabulary::Directory>,
    ) -> Result<crate::vocabulary::Status, ClientError> {
        let mut vocabulary =
            wire::vocabulary_service_client::VocabularyServiceClient::new(self.channel.clone());
        let request = wire::VocabularyRequest {
            directory: directory.map(|path| path.as_str().to_owned()),
        };
        let timeout = REQUEST_TIMEOUT;
        let response = self
            .runtime
            .block_on(with_deadline(
                timeout,
                vocabulary.get_status(request_with_timeout(request, timeout)),
            ))?
            .map_err(status_error)?;
        response
            .into_inner()
            .try_into()
            .map_err(|error: crate::ipc::protocol::ProtocolError| {
                ClientError::MalformedResponse(error.to_string())
            })
    }

    pub(crate) fn vocabulary_refresh(
        &self,
        directory: Option<&crate::vocabulary::Directory>,
    ) -> Result<crate::vocabulary::Status, ClientError> {
        let mut vocabulary =
            wire::vocabulary_service_client::VocabularyServiceClient::new(self.channel.clone());
        let request = wire::VocabularyRequest {
            directory: directory.map(|path| path.as_str().to_owned()),
        };
        let timeout = Duration::from_secs(120);
        let response = self
            .runtime
            .block_on(with_deadline(
                timeout,
                vocabulary.refresh(request_with_timeout(request, timeout)),
            ))?
            .map_err(status_error)?;
        response
            .into_inner()
            .try_into()
            .map_err(|error: crate::ipc::protocol::ProtocolError| {
                ClientError::MalformedResponse(error.to_string())
            })
    }

    pub(crate) fn vocabulary_forget(
        &self,
        directory: Option<&crate::vocabulary::Directory>,
    ) -> Result<crate::vocabulary::Status, ClientError> {
        let mut vocabulary =
            wire::vocabulary_service_client::VocabularyServiceClient::new(self.channel.clone());
        let request = wire::VocabularyRequest {
            directory: directory.map(|path| path.as_str().to_owned()),
        };
        let timeout = REQUEST_TIMEOUT;
        let response = self
            .runtime
            .block_on(with_deadline(
                timeout,
                vocabulary.forget(request_with_timeout(request, timeout)),
            ))?
            .map_err(status_error)?;
        response
            .into_inner()
            .try_into()
            .map_err(|error: crate::ipc::protocol::ProtocolError| {
                ClientError::MalformedResponse(error.to_string())
            })
    }

    pub(crate) fn get_preferences(&self) -> Result<PreferenceSnapshot, ClientError> {
        let mut preferences =
            wire::preferences_service_client::PreferencesServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                preferences.get(request_with_timeout(
                    wire::GetPreferencesRequest {},
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;

        PreferenceSnapshot::try_from(response.into_inner())
            .map_err(|error| ClientError::MalformedResponse(error.to_string()))
    }

    pub(crate) fn update_preferences(
        &self,
        expected_revision: u64,
        patch: PreferencePatch,
    ) -> Result<UpdateResult, ClientError> {
        let mut preferences =
            wire::preferences_service_client::PreferencesServiceClient::new(self.channel.clone());
        let request = wire::UpdatePreferencesRequest::from((expected_revision, patch));
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                preferences.update(request_with_timeout(request, REQUEST_TIMEOUT)),
            ))?
            .map_err(status_error)?;

        UpdateResult::try_from(response.into_inner())
            .map_err(|error| ClientError::MalformedResponse(error.to_string()))
    }

    pub(crate) fn listen_start(
        &self,
        transcript_path: Option<String>,
    ) -> Result<wire::StartListenResponse, ClientError> {
        let mut listen =
            wire::listen_service_client::ListenServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                listen.start(request_with_timeout(
                    wire::StartListenRequest { transcript_path },
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;
        Ok(response.into_inner())
    }

    pub(crate) fn enroll(
        &self,
        request: EnrollmentRequest,
    ) -> Result<EnrollmentResult, ClientError> {
        let name = request.name.clone();
        let mut speaker =
            wire::speaker_service_client::SpeakerServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                ENROLL_TIMEOUT,
                speaker.enroll(request_with_timeout(request.into(), ENROLL_TIMEOUT)),
            ))?
            .map_err(status_error)?
            .into_inner();
        Ok(EnrollmentResult {
            name,
            role: protocol::role(response.role)
                .map_err(|error| ClientError::MalformedResponse(error.to_string()))?,
            consistency: response.consistency,
        })
    }

    pub(crate) fn list_speakers(&self) -> Result<Vec<EnrolledSpeaker>, ClientError> {
        let mut speaker =
            wire::speaker_service_client::SpeakerServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                speaker.list(request_with_timeout(
                    wire::ListSpeakersRequest {},
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?
            .into_inner();
        response
            .speakers
            .iter()
            .map(|speaker| {
                EnrolledSpeaker::try_from(speaker)
                    .map_err(|error| ClientError::MalformedResponse(error.to_string()))
            })
            .collect()
    }

    pub(crate) fn remove_speaker(&self, name: String) -> Result<(), ClientError> {
        let mut speaker =
            wire::speaker_service_client::SpeakerServiceClient::new(self.channel.clone());
        self.runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                speaker.remove(request_with_timeout(
                    wire::RemoveSpeakerRequest { name },
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;
        Ok(())
    }

    pub(crate) fn set_owner(&self, name: String) -> Result<(), ClientError> {
        let mut speaker =
            wire::speaker_service_client::SpeakerServiceClient::new(self.channel.clone());
        self.runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                speaker.set_owner(request_with_timeout(
                    wire::SetOwnerRequest { name },
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;
        Ok(())
    }

    pub(crate) fn listen_stop(&self) -> Result<wire::StopListenResponse, ClientError> {
        let mut listen =
            wire::listen_service_client::ListenServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                LISTEN_STOP_TIMEOUT,
                listen.stop(request_with_timeout(
                    wire::StopListenRequest {},
                    LISTEN_STOP_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;
        Ok(response.into_inner())
    }

    pub(crate) fn listen_status(&self) -> Result<wire::GetListenStatusResponse, ClientError> {
        let mut listen =
            wire::listen_service_client::ListenServiceClient::new(self.channel.clone());
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                listen.get_status(request_with_timeout(
                    wire::GetListenStatusRequest {},
                    REQUEST_TIMEOUT,
                )),
            ))?
            .map_err(status_error)?;
        Ok(response.into_inner())
    }

    /// The stream shares this client's runtime: the channel only makes
    /// progress on the runtime that built it.
    pub(crate) fn subscribe(&self) -> Result<EventStream, ClientError> {
        let mut events =
            wire::events_service_client::EventsServiceClient::new(self.channel.clone());
        // The deadline covers only establishing the stream. Events then
        // arrive whenever the user presses; patience is not an error.
        let response = self
            .runtime
            .block_on(with_deadline(
                REQUEST_TIMEOUT,
                events.subscribe(wire::SubscribeRequest {}),
            ))?
            .map_err(status_error)?;

        Ok(EventStream {
            stream: response.into_inner(),
            runtime: Arc::clone(&self.runtime),
        })
    }
}

/// The outcome is the first terminal message; a stage may still trail it.
/// The stream ending before one is the daemon going away mid-press.
fn outcome_from(
    messages: impl IntoIterator<Item = Result<wire::StartDictationResponse, ClientError>>,
    mut observe: impl FnMut(&DictationStage),
) -> Result<DictationOutcome, ClientError> {
    use wire::start_dictation_response::Body;
    let malformed =
        |error: protocol::ProtocolError| ClientError::MalformedResponse(error.to_string());
    for message in messages {
        match message?.body {
            Some(Body::Progress(stage)) => {
                observe(&DictationStage::try_from(&stage).map_err(malformed)?);
            }
            Some(Body::Result(result)) => {
                return Ok(DictationOutcome::Completed(
                    crate::dictation::DictationResult {
                        text: result.text,
                        own_voice: result.own_voice,
                    },
                ));
            }
            Some(Body::StopAccepted(_)) => return Ok(DictationOutcome::StopAccepted),
            None => {
                return Err(malformed(protocol::ProtocolError::MissingBody(
                    "dictation response",
                )));
            }
        }
    }
    Err(ClientError::MalformedResponse(
        "the press ended without an outcome".to_owned(),
    ))
}

/// A subscribed stream, yielding stages until the daemon closes it.
pub struct EventStream {
    // Declared before the runtime so it drops first.
    stream: tonic::Streaming<wire::Event>,
    runtime: Arc<ClientRuntime>,
}

impl Iterator for EventStream {
    type Item = Result<DaemonEvent, ClientError>;

    /// `None` is the daemon ending the stream; an error is a broken one.
    /// An event this build does not know is skipped, so a newer daemon's
    /// extra events do not end the stream.
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let event = match self.runtime.block_on(self.stream.message()) {
                Ok(Some(event)) => event,
                Ok(None) => return None,
                Err(status) => return Some(Err(status_error(status))),
            };
            if let Some(event) = decode_event(event) {
                return Some(event);
            }
        }
    }
}

/// One event from the daemon's stream, whichever capability announced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonEvent {
    Dictation(DictationStage),
    Chat(ChatStage),
    Speech(SpeechStage),
    Listen(ListenStage),
    Enrollment(EnrollmentStage),
}

/// A newer daemon's event kinds must not end the stream. An older daemon's
/// chat event must: without its assistant, the ask it answers would wait forever.
fn decode_event(event: wire::Event) -> Option<Result<DaemonEvent, ClientError>> {
    match event.payload {
        Some(
            wire::event::Payload::Caption(_)
            | wire::event::Payload::Ghost(_)
            | wire::event::Payload::Level(_)
            | wire::event::Payload::Hotkey(_),
        ) => None,
        Some(wire::event::Payload::Dictation(event)) => match DictationStage::try_from(&event) {
            Ok(stage) => Some(Ok(DaemonEvent::Dictation(stage))),
            Err(error) => {
                tracing::debug!(%error, "skipping an unrecognized stage");
                None
            }
        },
        Some(wire::event::Payload::Chat(event)) => match ChatStage::try_from(&event) {
            Ok(stage) => Some(Ok(DaemonEvent::Chat(stage))),
            Err(
                error @ protocol::ProtocolError::InvalidEnum {
                    field: "assistant_id" | "step",
                    ..
                },
            ) => Some(Err(ClientError::MalformedResponse(format!(
                "{error}; restart the daemon"
            )))),
            Err(error) => {
                tracing::debug!(%error, "skipping an unrecognized stage");
                None
            }
        },
        Some(wire::event::Payload::Speaker(event)) => match EnrollmentStage::try_from(&event) {
            Ok(stage) => Some(Ok(DaemonEvent::Enrollment(stage))),
            Err(error) => {
                tracing::debug!(%error, "skipping an unrecognized stage");
                None
            }
        },
        Some(wire::event::Payload::Speech(event)) => match SpeechStage::try_from(&event) {
            Ok(stage) => Some(Ok(DaemonEvent::Speech(stage))),
            Err(error) => {
                tracing::debug!(%error, "skipping an unrecognized stage");
                None
            }
        },
        Some(wire::event::Payload::Listen(event)) => match ListenStage::try_from(&event) {
            Ok(stage) => Some(Ok(DaemonEvent::Listen(stage))),
            Err(error) => {
                tracing::debug!(%error, "skipping an unrecognized stage");
                None
            }
        },
        None => {
            tracing::debug!("skipping an unrecognized event payload");
            None
        }
    }
}

/// Enforces the deadline locally; `request_with_timeout` announces it.
async fn with_deadline<T>(
    deadline: Duration,
    call: impl std::future::Future<Output = T>,
) -> Result<T, ClientError> {
    tokio::time::timeout(deadline, call)
        .await
        .map_err(|_elapsed| ClientError::Timeout(deadline))
}

fn request_with_timeout<T>(message: T, deadline: Duration) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.set_timeout(deadline);
    request
}

/// An Aria failure when the status details carry one, the raw status
/// otherwise.
fn status_error(status: tonic::Status) -> ClientError {
    match protocol::rpc_failure(&status) {
        Some(RpcFailure { code, message }) => ClientError::Rpc { code, message },
        None => ClientError::Status(status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions_do_not_become_cli_stages() {
        let event = wire::Event {
            payload: Some(wire::event::Payload::Caption(wire::CaptionSnapshot {
                recording_id: 42,
                committed: "hello".to_owned(),
                provisional: "world".to_owned(),
                ended: false,
            })),
        };
        assert!(decode_event(event).is_none());
    }

    #[test]
    fn a_readiness_rejection_is_not_transient() {
        let error = status_error(tonic::Status::from(RpcFailure::new(
            RpcErrorCode::InvalidRequest,
            "malformed request",
        )));

        assert!(!error.is_transient_readiness_failure());
    }

    #[test]
    fn an_unreachable_daemon_is_a_transient_readiness_failure() {
        let error = status_error(tonic::Status::unavailable("connection refused"));

        assert!(error.is_transient_readiness_failure());
        assert!(ClientError::NotServing.is_transient_readiness_failure());
        assert!(ClientError::Timeout(READINESS_TIMEOUT).is_transient_readiness_failure());
    }

    // An unknown payload decodes as empty; ending the stream on one would
    // kill every older `aria events` process.
    #[test]
    fn an_unknown_event_payload_is_skipped_not_fatal() {
        assert!(decode_event(wire::Event { payload: None }).is_none());
        assert!(matches!(
            decode_event(wire::Event::from(DictationStage::Recording(RecordingId(1)))),
            Some(Ok(DaemonEvent::Dictation(DictationStage::Recording(
                RecordingId(1)
            ))))
        ));
        assert!(matches!(
            decode_event(wire::Event::from(SpeechStage::Speaking)),
            Some(Ok(DaemonEvent::Speech(SpeechStage::Speaking)))
        ));
    }

    // Skipping an invalid chat event would leave its ask waiting forever.
    #[test]
    fn a_chat_event_without_an_assistant_ends_the_stream_loudly() {
        let stale = wire::Event {
            payload: Some(wire::event::Payload::Chat(wire::ChatEvent {
                stage: wire::ChatStage::Working as i32,
                step: wire::ChatStep::Thinking as i32,
                ticket: 1,
                ..Default::default()
            })),
        };

        assert!(matches!(
            decode_event(stale),
            Some(Err(ClientError::MalformedResponse(message))) if message.contains("restart")
        ));
    }

    #[test]
    fn a_press_reports_its_stages_then_its_outcome() {
        let messages = [
            Ok(wire::StartDictationResponse::from(
                DictationStage::Recording(RecordingId(3)),
            )),
            Ok(wire::StartDictationResponse::from(
                DictationStage::Transcribing,
            )),
            Ok(wire::StartDictationResponse::from(
                DictationOutcome::Completed(crate::dictation::DictationResult {
                    text: "heard".to_owned(),
                    own_voice: false,
                }),
            )),
            Ok(wire::StartDictationResponse::from(
                DictationStage::Delivered {
                    outcome: None,
                    words: 0,
                },
            )),
        ];
        let mut seen = Vec::new();

        let outcome = outcome_from(messages, |stage| seen.push(stage.clone()));

        assert_eq!(
            seen,
            [
                DictationStage::Recording(RecordingId(3)),
                DictationStage::Transcribing
            ]
        );
        assert!(
            matches!(outcome, Ok(DictationOutcome::Completed(result)) if result.text == "heard")
        );
    }

    #[test]
    fn a_press_that_ends_without_an_outcome_is_an_error() {
        let messages = [Ok(wire::StartDictationResponse::from(
            DictationStage::Recording(RecordingId(3)),
        ))];

        assert!(matches!(
            outcome_from(messages, |_stage| {}),
            Err(ClientError::MalformedResponse(..))
        ));
    }

    #[test]
    fn a_failed_press_surfaces_its_status() {
        let messages = [
            Ok(wire::StartDictationResponse::from(
                DictationStage::Recording(RecordingId(3)),
            )),
            Err(ClientError::NotServing),
        ];

        assert!(matches!(
            outcome_from(messages, |_stage| {}),
            Err(ClientError::NotServing)
        ));
    }

    #[test]
    fn delivery_target_change_is_preserved_for_clients() {
        let error = status_error(tonic::Status::from(RpcFailure::new(
            RpcErrorCode::DeliveryTargetChanged,
            "test RPC error",
        )));

        assert!(matches!(
            error,
            ClientError::Rpc {
                code: RpcErrorCode::DeliveryTargetChanged,
                ..
            }
        ));
    }

    #[test]
    fn an_rpc_failure_reads_as_its_message_without_its_code() {
        let named = ClientError::Rpc {
            code: RpcErrorCode::DeliveryTargetChanged,
            message: "the window changed".to_owned(),
        };
        let bare = ClientError::Rpc {
            code: RpcErrorCode::HistoryUnavailable,
            message: String::new(),
        };

        assert_eq!(named.to_string(), "the window changed");
        assert_eq!(bare.to_string(), "history unavailable");
        let newer = ClientError::Rpc {
            code: RpcErrorCode::Unknown(42),
            message: String::new(),
        };
        assert_eq!(newer.to_string(), "unknown (42)");
    }
}
