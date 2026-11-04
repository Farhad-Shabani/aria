//! Receives LAN audio and dictates it through the press pipeline.
//! [`crate::daemon::relay`] validates and collects the upload.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use aria_kit::audio::RecordingReader;
use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

use crate::ipc::wire;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_voice::dictation::{DictationError, DictationSettings};
use aria_voice::microphone::{Claim, Gate};

use super::super::dictation::{DictationRuntime, DictationRuntimeError};
use super::super::relay::{self, Collected, Opening, RelayError, RelayOutcome};

const METHOD: &str = "RelayService/Relay";

/// Room for every event one session emits, so the worker never waits on a
/// client that is slow to read.
const EVENT_CAPACITY: usize = 8;

/// Served only by persistent daemons with relay enabled.
pub(in crate::daemon) struct RelayRpc {
    dictation: Arc<DictationRuntime>,
    microphone: Arc<Gate>,
    /// Temporary WAV directory for engines that read recordings from files.
    spool_dir: Arc<Path>,
    /// The daemon's stop, so a session in flight ends with it rather than
    /// holding the drain open until one of its own clocks runs out.
    shutdown: CancellationToken,
}

impl RelayRpc {
    pub(in crate::daemon) fn new(
        dictation: Arc<DictationRuntime>,
        microphone: Arc<Gate>,
        spool_dir: &Path,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            dictation,
            microphone,
            spool_dir: Arc::from(spool_dir),
            shutdown,
        }
    }
}

type EventStream = Pin<Box<dyn Stream<Item = Result<wire::RelayEvent, Status>> + Send>>;

#[tonic::async_trait]
impl wire::relay_service_server::RelayService for RelayRpc {
    type RelayStream = EventStream;

    async fn relay(
        &self,
        request: Request<Streaming<wire::RelayRequest>>,
    ) -> Result<Response<Self::RelayStream>, Status> {
        let mut requests = request.into_inner();

        // Both before the stream opens, so a client that opened badly or
        // arrived while a press was running learns it from the call rather
        // than from an event it has to interpret.
        let opening = relay::open(&mut requests)
            .await
            .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
        let claim = self
            .microphone
            .admit_relay()
            .ok_or_else(|| relay::redacted(super::failed(METHOD, &RelayError::Busy)))?;

        let settings = self
            .dictation
            .relay_settings()
            .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
        if settings.application_excluded() {
            return Err(relay::redacted(super::failed(
                METHOD,
                &DictationError::ApplicationExcluded,
            )));
        }
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let session = Session {
            dictation: Arc::clone(&self.dictation),
            spool_dir: Arc::clone(&self.spool_dir),
            shutdown: self.shutdown.clone(),
            opening,
            _claim: claim,
            settings,
            events,
        };
        tokio::spawn(session.run(requests));
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

/// One relayed recording, from the first chunk to the pasted text. Owns the
/// claim for its whole life: dropping this — the client vanished, the daemon
/// stopped — releases the recorder.
struct Session {
    dictation: Arc<DictationRuntime>,
    spool_dir: Arc<Path>,
    shutdown: CancellationToken,
    opening: Opening,
    _claim: Claim,
    settings: DictationSettings,
    events: mpsc::Sender<Result<wire::RelayEvent, Status>>,
}

impl Session {
    async fn run(self, mut requests: Streaming<wire::RelayRequest>) {
        let audio = match relay::collect(&mut requests, &self.shutdown).await {
            Ok(Collected::Audio(audio)) => audio,
            Ok(Collected::Silent) => {
                tracing::info!(method = METHOD, status = "no_speech", "request served");
                self.emit(Ok(RelayOutcome::NoSpeech.into())).await;
                return;
            }
            Err(error) => {
                self.emit(Err(relay::redacted(super::failed(METHOD, &error))))
                    .await;
                return;
            }
        };

        self.emit(Ok(RelayOutcome::Transcribing.into())).await;

        let dictation = Arc::clone(&self.dictation);
        let events = self.events.clone();
        let observer = events.clone();
        let shutdown = self.shutdown.clone();
        let cancel = CancelToken::from_fn(move || shutdown.is_cancelled() || observer.is_closed());
        let heard = tokio::task::spawn_blocking(move || hear(self, audio, cancel)).await;

        // The pipeline announces every stage of the dictation itself; only
        // a job that never ran to its end has nothing announced.
        let event = match heard {
            Ok(Ok(Some(text))) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(RelayOutcome::Completed(text).into())
            }
            Ok(Ok(None)) => {
                tracing::info!(method = METHOD, status = "no_speech", "request served");
                Ok(RelayOutcome::NoSpeech.into())
            }
            Ok(Err(failure)) => Err(relay::redacted(super::failed(METHOD, failure))),
            Err(_join) => {
                dictation.announce_failure(RpcErrorCode::Internal);
                Err(super::failed(
                    METHOD,
                    RpcFailure::new(
                        RpcErrorCode::Internal,
                        "the relay operation ended unexpectedly",
                    ),
                ))
            }
        };
        drop(events.send(event).await);
    }

    /// A client that stopped reading is not a failure worth reporting: the
    /// recording is already delivered, or already lost.
    async fn emit(&self, event: Result<wire::RelayEvent, Status>) {
        drop(self.events.send(event).await);
    }
}

/// Decodes the upload and dictates it. `None` is a recording nobody spoke
/// in, which is an outcome rather than a failure.
fn hear(
    session: Session,
    audio: Vec<u8>,
    cancel: CancelToken,
) -> Result<Option<String>, RpcFailure> {
    let Session {
        dictation,
        spool_dir,
        opening,
        settings,
        _claim,
        ..
    } = session;
    let samples = relay::decode(opening, &audio).map_err(|error| RpcFailure::from(&error))?;
    // A full recording is about 19 MB as bytes and again as samples, and
    // transcription reads neither.
    drop(audio);

    // File-based engines need a WAV. The recording owns its temporary file.
    let mut recording = RecordingReader::from_pcm16_in(&spool_dir, &samples)
        .map_err(|error| RpcFailure::from(&DictationError::from(error)))?;
    drop(samples);

    match dictation.dictate_recording(&mut recording, settings, cancel) {
        Ok(text) => Ok(Some(text)),
        Err(error) if is_silence(&error) => Ok(None),
        Err(error) => Err(RpcFailure::from(&error)),
    }
}

/// Whether the engine heard nothing. A recording of a quiet room is a fact
/// about the audio, not a fault in the daemon.
fn is_silence(error: &DictationRuntimeError) -> bool {
    // The engine heard nothing, or the own-voice gate cut everything it did
    // hear. To the person holding the watch these are one outcome: nobody
    // this Mac knows said anything.
    matches!(
        error,
        DictationRuntimeError::Execution(DictationError::NoSpeech)
    )
}
