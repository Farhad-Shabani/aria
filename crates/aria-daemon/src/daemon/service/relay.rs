//! Receives LAN audio and dictates it through the press pipeline.
//! [`crate::daemon::relay`] validates and collects the upload.

use std::pin::Pin;
use std::sync::Arc;

use aria_kit::audio::RecordingReader;
use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

use crate::ipc::wire;
use aria_platform::delivery::OutputMode;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_voice::dictation::{DictationError, DictationSettings, RecordingId, ReturnOffer};
use aria_voice::microphone::{Claim, Gate};

use super::super::dictation::{DictationRuntime, DictationRuntimeError, RelaySettings};
use super::super::relay::{
    self, Announcements, Collected, Opening, RelayError, RelayStage, Reply, ReplyChannel, Showing,
};

const METHOD: &str = "RelayService/Relay";
const START_REMOTE_RECORDING: &str = "RelayService/StartRemoteRecording";
const CANCEL_REMOTE_RECORDING: &str = "RelayService/CancelRemoteRecording";

/// Room for every event one session emits, so the worker never waits on a
/// client that is slow to read.
const EVENT_CAPACITY: usize = 8;

/// Served only by persistent daemons with relay enabled.
pub(in crate::daemon) struct RelayRpc {
    dictation: Arc<DictationRuntime>,
    /// Every phone's and watch's, so one shows at a time.
    announcements: Arc<Announcements>,
    microphone: Arc<Gate>,
    /// The daemon's stop, so a session in flight ends with it rather than
    /// holding the drain open until one of its own clocks runs out.
    shutdown: CancellationToken,
}

impl RelayRpc {
    pub(in crate::daemon) fn new(
        dictation: Arc<DictationRuntime>,
        microphone: Arc<Gate>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            announcements: Announcements::new(Arc::clone(&dictation)),
            dictation,
            microphone,
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
        // Taken first, so every refusal below ends the announcement too.
        let showing = self.announcements.showing(&opening);
        let claim = self
            .microphone
            .admit_relay()
            .ok_or_else(|| relay::redacted(super::failed(METHOD, &RelayError::Busy)))?;

        let RelaySettings {
            dictation: mut settings,
            phone_submits,
        } = self
            .dictation
            .relay_settings()
            .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
        let return_policy = ReturnPolicy::new(phone_submits, opening, settings.output_mode);
        settings.output_mode = return_policy.output_mode(settings.output_mode);
        if settings.application_excluded() {
            return Err(relay::redacted(super::failed(
                METHOD,
                &DictationError::ApplicationExcluded,
            )));
        }
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let session = Session {
            client: Client {
                events,
                shutdown: self.shutdown.clone(),
            },
            microphone: Arc::clone(&self.microphone),
            upload: Upload {
                dictation: Arc::clone(&self.dictation),
                opening,
                showing,
                return_policy,
                _claim: claim,
                settings,
            },
        };
        tokio::spawn(session.run(requests));
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn start_remote_recording(
        &self,
        request: Request<wire::StartRemoteRecordingRequest>,
    ) -> Result<Response<wire::StartRemoteRecordingResponse>, Status> {
        let device = crate::ipc::protocol::recording_device(request.into_inner().device)
            .ok_or_else(|| {
                relay::redacted(super::failed(
                    START_REMOTE_RECORDING,
                    &RelayError::DeviceUnspecified,
                ))
            })?;
        let id = self
            .announcements
            .start(device)
            .map_err(|error| relay::redacted(super::failed(START_REMOTE_RECORDING, &error)))?;
        super::served(START_REMOTE_RECORDING);
        Ok(Response::new(wire::StartRemoteRecordingResponse {
            recording_id: id.0,
        }))
    }

    async fn cancel_remote_recording(
        &self,
        request: Request<wire::CancelRemoteRecordingRequest>,
    ) -> Result<Response<wire::CancelRemoteRecordingResponse>, Status> {
        self.announcements
            .cancel(RecordingId(request.into_inner().recording_id));
        super::served(CANCEL_REMOTE_RECORDING);
        Ok(Response::new(wire::CancelRemoteRecordingResponse {}))
    }
}

/// One relayed recording, from the first chunk to the pasted text, then the
/// phone's ask for Return after it.
struct Session {
    client: Client,
    /// Taken again for the offered Return, so no recording types under it.
    microphone: Arc<Gate>,
    upload: Upload,
}

/// What the session tells the relay client, and what ends the session
/// early: the daemon stopping, or the client no longer reading.
struct Client {
    events: mpsc::Sender<Result<wire::RelayEvent, Status>>,
    shutdown: CancellationToken,
}

/// What hearing the upload takes. Owns the claim until the text lands, and
/// any Return pressed with it: dropping it there — or earlier, as the client
/// vanished or the daemon stopped — frees the recorder before the phone gets
/// COMPLETED. Dropped before the dictation, it ends the announcement it
/// continues.
struct Upload {
    dictation: Arc<DictationRuntime>,
    opening: Opening,
    showing: Showing,
    return_policy: ReturnPolicy,
    _claim: Claim,
    settings: DictationSettings,
}

impl Session {
    async fn run(self, mut requests: Streaming<wire::RelayRequest>) {
        let Self {
            client,
            microphone,
            upload,
        } = self;
        let (audio, channel) = match relay::collect(&mut requests, &client.shutdown).await {
            Ok(Collected::Audio { audio, channel }) => (audio, channel),
            Ok(Collected::Silent) => {
                tracing::info!(method = METHOD, status = "no_speech", "request served");
                client.emit(Ok(RelayStage::NoSpeech.into())).await;
                return;
            }
            Err(error) => {
                client
                    .emit(Err(relay::redacted(super::failed(METHOD, &error))))
                    .await;
                return;
            }
        };

        client.emit(Ok(RelayStage::Transcribing.into())).await;

        let dictation = Arc::clone(&upload.dictation);
        let cancel = client.cancel();
        let heard = tokio::task::spawn_blocking(move || hear(upload, audio, cancel)).await;

        // The pipeline announces every stage of the dictation itself; only
        // a job that never ran to its end has nothing announced.
        let heard = match heard {
            Ok(Ok(Some(heard))) => heard,
            Ok(Ok(None)) => {
                tracing::info!(method = METHOD, status = "no_speech", "request served");
                client.emit(Ok(RelayStage::NoSpeech.into())).await;
                return;
            }
            Ok(Err(failure)) => {
                client
                    .emit(Err(relay::redacted(super::failed(METHOD, failure))))
                    .await;
                return;
            }
            Err(_join) => {
                dictation.announce_failure(RpcErrorCode::Internal);
                client
                    .emit(Err(super::failed(
                        METHOD,
                        RpcFailure::new(
                            RpcErrorCode::Internal,
                            "the relay operation ended unexpectedly",
                        ),
                    )))
                    .await;
                return;
            }
        };

        complete(heard, channel, &microphone, &client, &mut requests).await;
    }
}

/// What the phone's Return may do after this upload's text. Decided at
/// admission, so a settings change applies from the next upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnPolicy {
    /// The Mac doesn't let the phone press Return. The phone may write the
    /// output mode, so insert and press Return only types.
    Never,
    Offer,
    /// As soon as the text lands, under the upload's claim.
    Press,
}

impl ReturnPolicy {
    fn new(permitted: bool, opening: Opening, output_mode: OutputMode) -> Self {
        if !permitted {
            return Self::Never;
        }
        let asked = opening.submits() || output_mode == OutputMode::InsertAndSubmit;
        if asked && output_mode != OutputMode::Copy {
            Self::Press
        } else {
            Self::Offer
        }
    }

    /// A press types as plain insertion and presses Return through the Return
    /// offer, so the phone hears whether Return went.
    fn output_mode(self, chosen: OutputMode) -> OutputMode {
        match self {
            Self::Press => OutputMode::Insert,
            Self::Never if chosen == OutputMode::InsertAndSubmit => OutputMode::Insert,
            Self::Never | Self::Offer => chosen,
        }
    }

    fn after_text(
        self,
        return_offer: Option<ReturnOffer>,
        cancel: &CancelToken,
    ) -> Result<AfterText, DictationError> {
        let Some(return_offer) = return_offer else {
            return Ok(AfterText::Nothing);
        };
        match self {
            Self::Never => Ok(AfterText::Nothing),
            Self::Offer => Ok(AfterText::Offered(return_offer)),
            Self::Press => {
                return_offer.press(cancel)?;
                Ok(AfterText::Submitted)
            }
        }
    }
}

async fn complete<S>(
    heard: Heard,
    channel: ReplyChannel,
    microphone: &Arc<Gate>,
    client: &Client,
    requests: &mut S,
) where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    let return_offer = match heard.after {
        AfterText::Submitted => {
            tracing::info!(method = METHOD, status = "submitted", "request served");
            client
                .emit(Ok(RelayStage::CompletedAndSubmitted(heard.text).into()))
                .await;
            return;
        }
        // A half-close says the client won't ask, so nothing is offered.
        AfterText::Offered(return_offer) if channel == ReplyChannel::Open => Some(return_offer),
        AfterText::Offered(_) | AfterText::Nothing => None,
    };
    let offered = return_offer.as_ref().map(|return_offer| relay::Offer {
        application_name: return_offer.application().map(|app| app.name.clone()),
    });
    client
        .emit(Ok(RelayStage::Completed {
            text: heard.text,
            return_offer: offered,
        }
        .into()))
        .await;
    match return_offer {
        Some(return_offer) => {
            if let Some(event) = submit_on_reply(return_offer, microphone, client, requests).await {
                client.emit(event).await;
            }
        }
        None => super::served(METHOD),
    }
}

impl Client {
    /// A client that stopped reading is not a failure worth reporting: the
    /// recording is already delivered, or already lost.
    async fn emit(&self, event: Result<wire::RelayEvent, Status>) {
        drop(self.events.send(event).await);
    }

    fn cancel(&self) -> CancelToken {
        let shutdown = self.shutdown.clone();
        let events = self.events.clone();
        CancelToken::from_fn(move || shutdown.is_cancelled() || events.is_closed())
    }
}

/// Presses the offered Return if the client asks before the offer lapses. `None`
/// when it asked nothing, so the stream ends after COMPLETED.
async fn submit_on_reply<S>(
    return_offer: ReturnOffer,
    microphone: &Arc<Gate>,
    client: &Client,
    requests: &mut S,
) -> Option<Result<wire::RelayEvent, Status>>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    match relay::next_reply(requests, &client.shutdown).await {
        Ok(Reply::Submit) => {}
        Ok(reply @ (Reply::Lapsed | Reply::Closed)) => {
            tracing::info!(method = METHOD, status = "ok", ?reply, "request served");
            return None;
        }
        Err(error) => return Some(Err(relay::redacted(super::failed(METHOD, &error)))),
    }
    let microphone = Arc::clone(microphone);
    let cancel = client.cancel();
    let pressed = super::blocking(METHOD, move || {
        let _claim = microphone.admit_relay().ok_or(DictationError::Busy)?;
        return_offer.press(&cancel)
    })
    .await;
    Some(match pressed {
        Ok(Ok(())) => {
            tracing::info!(method = METHOD, status = "submitted", "request served");
            Ok(RelayStage::OfferTaken.into())
        }
        Ok(Err(error)) => Err(relay::redacted(super::failed(METHOD, &error))),
        Err(status) => Err(relay::redacted(status)),
    })
}

struct Heard {
    text: String,
    after: AfterText,
}

enum AfterText {
    Offered(ReturnOffer),
    Submitted,
    Nothing,
}

/// Decodes the upload and dictates it. `None` is a recording nobody spoke
/// in, which is an outcome rather than a failure.
fn hear(upload: Upload, audio: Vec<u8>, cancel: CancelToken) -> Result<Option<Heard>, RpcFailure> {
    let Upload {
        dictation,
        opening,
        showing,
        return_policy,
        settings,
        _claim,
    } = upload;
    let samples = relay::decode(opening, &audio).map_err(|error| RpcFailure::from(&error))?;
    // A full recording is about 19 MB as bytes and again as samples, and
    // transcription reads neither.
    drop(audio);

    let mut recording = RecordingReader::from_pcm16(&samples);
    drop(samples);

    let delivered =
        match dictation.dictate_recording(&mut recording, showing, settings, cancel.clone()) {
            Ok(delivered) => delivered,
            Err(error) if is_silence(&error) => return Ok(None),
            Err(error) => return Err(RpcFailure::from(&error)),
        };
    let after = return_policy
        .after_text(delivered.return_offer, &cancel)
        .map_err(|error| RpcFailure::from(&error))?;
    Ok(Some(Heard {
        text: delivered.text,
        after,
    }))
}

/// Whether the engine heard nothing. A recording of a quiet room is a fact
/// about the audio, not a fault in the daemon.
fn is_silence(error: &DictationRuntimeError) -> bool {
    matches!(
        error,
        DictationRuntimeError::Execution(DictationError::NoSpeech)
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aria_voice::dictation::testing;

    use super::*;

    async fn opening(submit: bool) -> Opening {
        let start = wire::RelayRequest {
            body: Some(wire::relay_request::Body::Start(wire::RelayStart {
                format: wire::AudioFormat::PcmS16le as i32,
                submit,
                ..Default::default()
            })),
        };
        let mut stream = tokio_stream::iter([Ok(start)]);
        relay::open(&mut stream).await.expect("a valid opening")
    }

    fn submit() -> wire::RelayRequest {
        wire::RelayRequest {
            body: Some(wire::relay_request::Body::Submit(wire::RelaySubmit {})),
        }
    }

    /// The session from the landed text on, with a Return offer that counts
    /// its presses.
    async fn after_landing<S>(
        policy: ReturnPolicy,
        typed_into: &str,
        channel: ReplyChannel,
        microphone: &Arc<Gate>,
        mut requests: S,
    ) -> (Vec<Result<wire::RelayEvent, Status>>, usize)
    where
        S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
    {
        let presses = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&presses);
        let application = aria_platform::delivery::Application {
            name: typed_into.to_owned(),
            bundle_id: None,
        };
        let return_offer = testing::return_offer(Some(application), move |_cancel| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let after = policy
            .after_text(Some(return_offer), &CancelToken::new())
            .expect("the press succeeds");
        let heard = Heard {
            text: "ship it".to_owned(),
            after,
        };
        let (events, mut receiver) = mpsc::channel(EVENT_CAPACITY);
        let client = Client {
            events,
            shutdown: CancellationToken::new(),
        };

        complete(heard, channel, microphone, &client, &mut requests).await;
        drop(client);

        let mut emitted = Vec::new();
        while let Some(event) = receiver.recv().await {
            emitted.push(event);
        }
        (emitted, presses.load(Ordering::SeqCst))
    }

    fn stage(event: &Result<wire::RelayEvent, Status>) -> (wire::RelayStage, bool) {
        let event = event.as_ref().expect("an event, not a failure");
        (event.stage(), event.submitted)
    }

    /// The app COMPLETED offers Return in, or `None` when it offers none.
    fn offered_in(event: &Result<wire::RelayEvent, Status>) -> Option<Option<String>> {
        let event = event.as_ref().expect("an event, not a failure");
        event
            .return_offer
            .as_ref()
            .map(|return_offer| return_offer.application_name.clone())
    }

    #[tokio::test]
    async fn the_permission_decides_what_the_phones_return_may_do() {
        use OutputMode::{Copy, Insert, InsertAndSubmit, InsertOrCopy};
        for (permitted, submit, chosen, policy, output_mode) in [
            (false, true, Insert, ReturnPolicy::Never, Insert),
            (false, false, InsertAndSubmit, ReturnPolicy::Never, Insert),
            (
                false,
                false,
                InsertOrCopy,
                ReturnPolicy::Never,
                InsertOrCopy,
            ),
            (true, false, Insert, ReturnPolicy::Offer, Insert),
            (true, false, InsertOrCopy, ReturnPolicy::Offer, InsertOrCopy),
            (true, true, Insert, ReturnPolicy::Press, Insert),
            (true, true, InsertOrCopy, ReturnPolicy::Press, Insert),
            (true, false, InsertAndSubmit, ReturnPolicy::Press, Insert),
            (true, true, Copy, ReturnPolicy::Offer, Copy),
        ] {
            let decided = ReturnPolicy::new(permitted, opening(submit).await, chosen);

            let case = format!("{permitted} {submit} {chosen:?}");
            assert_eq!(decided, policy, "{case}");
            assert_eq!(decided.output_mode(chosen), output_mode, "{case}");
        }
    }

    #[tokio::test]
    async fn a_pressed_return_completes_as_submitted_and_takes_no_reply() {
        let requests = tokio_stream::iter([Ok(submit())]);

        let (events, presses) = after_landing(
            ReturnPolicy::Press,
            "Messages",
            ReplyChannel::Open,
            &Gate::new(),
            requests,
        )
        .await;

        assert_eq!(presses, 1);
        assert_eq!(events.len(), 1);
        assert_eq!(stage(&events[0]), (wire::RelayStage::Completed, true));
        assert_eq!(offered_in(&events[0]), None);
    }

    #[tokio::test]
    async fn an_offered_return_is_pressed_when_the_phone_asks() {
        let requests = tokio_stream::iter([Ok(submit())]);

        let (events, presses) = after_landing(
            ReturnPolicy::Offer,
            "Messages",
            ReplyChannel::Open,
            &Gate::new(),
            requests,
        )
        .await;

        assert_eq!(presses, 1);
        assert_eq!(events.len(), 2);
        assert_eq!(stage(&events[0]), (wire::RelayStage::Completed, false));
        assert_eq!(offered_in(&events[0]), Some(Some("Messages".to_owned())));
        assert_eq!(stage(&events[1]), (wire::RelayStage::OfferTaken, false));
    }

    #[tokio::test(start_paused = true)]
    async fn a_half_close_ends_the_session_at_completed_without_waiting() {
        let started = tokio::time::Instant::now();

        let (events, presses) = after_landing(
            ReturnPolicy::Offer,
            "Messages",
            ReplyChannel::Closed,
            &Gate::new(),
            Box::pin(tokio_stream::pending()),
        )
        .await;

        assert_eq!(presses, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(stage(&events[0]), (wire::RelayStage::Completed, false));
        assert_eq!(offered_in(&events[0]), None, "nobody is left to ask");
        assert!(started.elapsed().is_zero());
    }

    #[tokio::test]
    async fn an_offered_return_is_refused_while_a_recording_runs() {
        let microphone = Gate::new();
        let _recording = microphone.admit_relay().expect("the microphone is free");
        let requests = tokio_stream::iter([Ok(submit())]);

        let (events, presses) = after_landing(
            ReturnPolicy::Offer,
            "Messages",
            ReplyChannel::Open,
            &microphone,
            requests,
        )
        .await;

        assert_eq!(presses, 0);
        assert_eq!(events.len(), 2);
        assert_eq!(stage(&events[0]), (wire::RelayStage::Completed, false));
        let refused = events[1].as_ref().expect_err("the press is refused");
        let failure = crate::ipc::protocol::rpc_failure(refused).expect("an Aria failure");
        assert_eq!(failure.code, RpcErrorCode::Busy);
    }

    #[tokio::test]
    async fn a_remote_recording_must_name_its_device() {
        use wire::relay_service_server::RelayService;
        let directory = tempfile::tempdir().unwrap();
        let settings = Arc::new(
            aria_settings::SettingsStore::open(directory.path().join("settings.toml")).unwrap(),
        );
        let rpc = RelayRpc::new(
            Arc::new(DictationRuntime::detached(settings)),
            Gate::new(),
            CancellationToken::new(),
        );

        let refused = rpc
            .start_remote_recording(Request::new(wire::StartRemoteRecordingRequest {
                device: wire::RecordingDevice::Unspecified as i32,
            }))
            .await
            .expect_err("an unspecified device is refused");

        let failure = crate::ipc::protocol::rpc_failure(&refused).expect("an Aria failure");
        assert_eq!(failure.code, RpcErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn without_the_permission_a_submit_presses_nothing() {
        let requests = tokio_stream::iter([Ok(submit())]);

        let (events, presses) = after_landing(
            ReturnPolicy::Never,
            "Messages",
            ReplyChannel::Open,
            &Gate::new(),
            requests,
        )
        .await;

        assert_eq!(presses, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(stage(&events[0]), (wire::RelayStage::Completed, false));
        assert_eq!(offered_in(&events[0]), None);
    }
}
