//! Receives LAN audio, transcribes it, and delivers text using dictation preferences.
//! [`crate::daemon::relay`] validates and collects the upload.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use crate::audio::RecordingReader;
use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

use crate::delivery::{DeliveryError, DeliveryOutcome, PlatformTextDelivery, TextDelivery};
use crate::dictation::{
    DictationError, DictationSettings, DictationStage, TranscriptSink, Utterance,
};
use crate::history::HistorySource;
use crate::ipc::wire;
use crate::microphone::{Claim, Gate};
use crate::rpc::{RpcErrorCode, RpcFailure};

use super::super::dictation::DictationRuntime;
use super::super::relay::{self, Collected, Opening, RelayError};
use super::super::transcripts::History;

const METHOD: &str = "RelayService/Relay";

/// Room for every event one session emits, so the worker never waits on a
/// client that is slow to read.
const EVENT_CAPACITY: usize = 8;

/// Served only by persistent daemons with relay enabled.
pub(in crate::daemon) struct RelayRpc {
    dictation: Arc<DictationRuntime>,
    microphone: Arc<Gate>,
    delivery: Arc<PlatformTextDelivery>,
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
        delivery: PlatformTextDelivery,
        spool_dir: &Path,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            dictation,
            microphone,
            delivery: Arc::new(delivery),
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
            .map_err(|error| super::failed(METHOD, error.failure()))?;
        let claim = self
            .microphone
            .admit_relay()
            .ok_or_else(|| super::failed(METHOD, RelayError::Busy.failure()))?;

        let settings = self
            .dictation
            .relay_settings()
            .map_err(|error| super::failed(METHOD, failure(RpcErrorCode::Internal, &error)))?;
        if settings.application_excluded() {
            let excluded = DictationError::ApplicationExcluded;
            return Err(super::failed(
                METHOD,
                failure((&excluded).into(), &excluded),
            ));
        }
        let mut history = self.dictation.history();
        history.start(&settings, HistorySource::Relay);
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let session = Session {
            dictation: Arc::clone(&self.dictation),
            delivery: Arc::clone(&self.delivery),
            spool_dir: Arc::clone(&self.spool_dir),
            shutdown: self.shutdown.clone(),
            opening,
            _claim: claim,
            settings,
            history,
            events,
        };
        tokio::spawn(session.run(requests));
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

/// Every failure here reads the same way: a code, and the error's own chain
/// as the message a client sees.
fn failure(code: RpcErrorCode, error: &dyn std::error::Error) -> RpcFailure {
    RpcFailure::new(code, crate::primitives::error_chain(error))
}

/// One relayed recording, from the first chunk to the pasted text. Owns the
/// claim for its whole life: dropping this — the client vanished, the daemon
/// stopped — releases the recorder.
struct Session {
    dictation: Arc<DictationRuntime>,
    delivery: Arc<PlatformTextDelivery>,
    spool_dir: Arc<Path>,
    shutdown: CancellationToken,
    opening: Opening,
    _claim: Claim,
    settings: DictationSettings,
    history: History,
    events: mpsc::Sender<Result<wire::RelayEvent, Status>>,
}

impl Session {
    async fn run(self, mut requests: Streaming<wire::RelayRequest>) {
        let audio = match relay::collect(&mut requests, &self.shutdown).await {
            Ok(Collected::Audio(audio)) => audio,
            Ok(Collected::Silent) => {
                tracing::info!(method = METHOD, status = "no_speech", "request served");
                self.emit(Ok(stage(wire::RelayStage::NoSpeech))).await;
                return;
            }
            Err(error) => {
                let failure = error.failure();
                tracing::info!(
                    method = METHOD,
                    status = failure.code.label(),
                    message = failure.message,
                    "request failed"
                );
                self.emit(Err(Status::from(failure))).await;
                return;
            }
        };

        self.emit(Ok(stage(wire::RelayStage::Transcribing))).await;
        self.dictation.announce(DictationStage::Transcribing);

        let announcer = Arc::clone(&self.dictation);
        let events = self.events.clone();
        let observer = events.clone();
        let shutdown = self.shutdown.clone();
        let cancel = CancelToken::from_fn(move || shutdown.is_cancelled() || observer.is_closed());
        let heard = tokio::task::spawn_blocking(move || hear(self, audio, &cancel)).await;

        // Every outcome ends the Mac's dictation status.
        let event = match heard {
            Ok(Ok(Some((text, delivered)))) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                announcer.announce(DictationStage::delivered(Some(delivered), &text));
                Ok(wire::RelayEvent {
                    stage: wire::RelayStage::Completed as i32,
                    text,
                })
            }
            Ok(Ok(None)) => {
                tracing::info!(method = METHOD, status = "no_speech", "request served");
                announcer.announce(DictationStage::Failed(RpcErrorCode::NoSpeech));
                Ok(stage(wire::RelayStage::NoSpeech))
            }
            Ok(Err(failure)) => {
                tracing::info!(
                    method = METHOD,
                    status = failure.code.label(),
                    message = failure.message,
                    "request failed"
                );
                announcer.announce(DictationStage::ended_by(failure.code));
                Err(Status::from(failure))
            }
            Err(_join) => {
                tracing::error!(method = METHOD, "the relay job panicked");
                announcer.announce(DictationStage::Failed(RpcErrorCode::Internal));
                Err(Status::from(RpcFailure::new(
                    RpcErrorCode::Internal,
                    "the relay operation ended unexpectedly",
                )))
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

fn stage(stage: wire::RelayStage) -> wire::RelayEvent {
    wire::RelayEvent {
        stage: stage as i32,
        text: String::new(),
    }
}

/// Decodes, transcribes and delivers. `None` is a recording nobody spoke in,
/// which is an outcome rather than a failure.
fn hear(
    session: Session,
    audio: Vec<u8>,
    cancel: &CancelToken,
) -> Result<Option<(String, DeliveryOutcome)>, RpcFailure> {
    let Session {
        dictation,
        delivery,
        spool_dir,
        opening,
        settings,
        mut history,
        _claim,
        ..
    } = session;
    let samples = relay::decode(opening, &audio).map_err(|error| error.failure())?;
    // A full recording is about 19 MB as bytes and again as samples, and
    // transcription reads neither.
    drop(audio);

    // File-based engines need a WAV. The recording owns its temporary file.
    let mut recording = RecordingReader::from_pcm16_in(&spool_dir, &samples)
        .map_err(|error| failure(RpcErrorCode::CaptureFailed, &error))?;
    drop(samples);

    let text = match dictation.transcribe(&mut recording, &settings, cancel) {
        Ok(transcribed) => {
            history.serving_engine(transcribed.engine);
            transcribed.transcript.into_text()
        }
        Err(error) if is_silence(&error) => return Ok(None),
        Err(error) => {
            match &error {
                super::super::dictation::DictationRuntimeError::Execution(error) => {
                    history.recording_finished(&mut recording, Err(error));
                }
                _ => history.transcription_failed(&mut recording),
            }
            return Err(failure((&error).into(), &error));
        }
    };
    history
        .completed(Utterance::anonymous(&text))
        .map_err(|error| failure(RpcErrorCode::Internal, &error))?;

    // Honor the same output preference as local dictation.
    dictation.announce(DictationStage::Delivering);
    let delivered = deliver(&*delivery, &settings, &text, cancel).map_err(DictationError::from);
    history.recording_finished(&mut recording, delivered.as_ref().map(|_| ()));
    let delivered = delivered.map_err(|error| failure(RpcErrorCode::from(&error), &error))?;
    Ok(Some((text, delivered)))
}

/// Types `text` as a local press would: an insertion that asks to submit
/// presses Return once it lands.
fn deliver<D: TextDelivery>(
    delivery: &D,
    settings: &DictationSettings,
    text: &str,
    cancel: &CancelToken,
) -> Result<DeliveryOutcome, DeliveryError> {
    let should_cancel = || cancel.is_cancelled();
    settings.target.confirm_front()?;
    let prepared = delivery.prepare(settings.output_mode, should_cancel)?;
    settings.target.confirm_front()?;
    let text = delivery.caret(&prepared).insert(text);
    let outcome = delivery.deliver(&prepared, &text, should_cancel)?;
    if outcome == DeliveryOutcome::Inserted {
        delivery.finish(&prepared, should_cancel)?;
    }
    Ok(outcome)
}

/// Whether the engine heard nothing. A recording of a quiet room is a fact
/// about the audio, not a fault in the daemon.
fn is_silence(error: &super::super::dictation::DictationRuntimeError) -> bool {
    use crate::dictation::DictationError;

    matches!(
        error,
        super::super::dictation::DictationRuntimeError::Execution(
            // The engine heard nothing, or the own-voice gate cut everything
            // it did hear. To the person holding the watch these are one
            // outcome: nobody this Mac knows said anything.
            DictationError::NoSpeech
        )
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::delivery::{Caret, OutputMode};

    /// Inserts everything at `caret` and writes down each step it was
    /// asked for.
    #[derive(Default)]
    struct Steps {
        log: Mutex<Vec<String>>,
        caret: Caret,
    }

    impl TextDelivery for Steps {
        type Prepared = OutputMode;

        const DEFAULT_MODE: OutputMode = OutputMode::Insert;
        const OUTLIVES_PROCESS: bool = true;

        fn prepare(
            &self,
            mode: OutputMode,
            _should_cancel: impl Fn() -> bool,
        ) -> Result<OutputMode, DeliveryError> {
            Ok(mode)
        }

        fn finish(
            &self,
            prepared: &OutputMode,
            _should_cancel: impl Fn() -> bool,
        ) -> Result<(), DeliveryError> {
            if *prepared == OutputMode::InsertAndSubmit {
                self.log.lock().unwrap().push("submit".to_owned());
            }
            Ok(())
        }

        fn deliver(
            &self,
            _prepared: &OutputMode,
            text: &str,
            _should_cancel: impl Fn() -> bool,
        ) -> Result<DeliveryOutcome, DeliveryError> {
            self.log.lock().unwrap().push(format!("deliver:{text}"));
            Ok(DeliveryOutcome::Inserted)
        }

        fn caret(&self, _prepared: &OutputMode) -> Caret {
            self.caret
        }
    }

    #[test]
    fn an_inserted_submit_is_finished() {
        let steps = Steps::default();

        let outcome = deliver(
            &steps,
            &settings(OutputMode::InsertAndSubmit),
            "ship it",
            &CancelToken::never(),
        )
        .expect("delivers");

        assert_eq!(outcome, DeliveryOutcome::Inserted);
        assert_eq!(*steps.log.lock().unwrap(), ["deliver:ship it", "submit"]);
    }

    #[test]
    fn a_phone_dictation_is_spaced_by_the_text_at_the_caret() {
        let steps = Steps {
            caret: Caret::between("Done.", ""),
            ..Steps::default()
        };

        deliver(
            &steps,
            &settings(OutputMode::Insert),
            "Ship it",
            &CancelToken::never(),
        )
        .expect("delivers");

        assert_eq!(*steps.log.lock().unwrap(), ["deliver: Ship it"]);
    }

    #[test]
    fn settings_chosen_for_another_app_are_not_delivered() {
        let steps = Steps::default();
        let settings = DictationSettings {
            target: crate::delivery::TargetPolicy {
                overridden_for: Some("com.example.not-in-front".into()),
                excluded: Vec::new(),
            },
            ..settings(OutputMode::InsertAndSubmit)
        };

        let error = deliver(&steps, &settings, "ship it", &CancelToken::never())
            .expect_err("another app is in front");

        assert!(matches!(error, DeliveryError::TargetChanged));
        assert!(steps.log.lock().unwrap().is_empty());
    }

    fn settings(output_mode: OutputMode) -> DictationSettings {
        crate::dictation::testing::settings(crate::dictation::DeliveryMode::Complete, output_mode)
    }
}
