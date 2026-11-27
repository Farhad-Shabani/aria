//! Receives LAN audio and dictates it through the press pipeline.
//! [`crate::daemon::relay`] validates the stream and forwards its audio to
//! the press.

use std::pin::Pin;
use std::sync::Arc;

use aria_kit::audio::PushedFeed;
use aria_kit::cancel::CancelToken;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

use crate::ipc::wire;
use aria_platform::delivery::OutputMode;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_voice::dictation::{Delivered, DictationError, DictationSettings, ReturnOffer};
use aria_voice::microphone::Gate;

use super::super::capture::FEED_CHUNKS;
use super::super::dictation::{DictationRuntime, DictationRuntimeError, RelaySettings};
use super::super::relay::{self, Opening, PushEnd, RelayError, RelayStage, Reply, ReplyChannel};

const METHOD: &str = "RelayService/Relay";

/// Room for every event one session emits, so the worker never waits on a
/// client that is slow to read.
const EVENT_CAPACITY: usize = 8;

/// Served only by persistent daemons with relay enabled.
pub(in crate::daemon) struct RelayRpc {
    dictation: Arc<DictationRuntime>,
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
            dictation,
            microphone,
            shutdown,
        }
    }

    /// The settings a relay reads at admission, and what its Return may do.
    fn session_settings(&self) -> Result<(DictationSettings, ReturnPolicy), Status> {
        let RelaySettings {
            dictation: mut settings,
            phone_submits,
        } = self
            .dictation
            .relay_settings()
            .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
        let return_policy = ReturnPolicy::new(phone_submits, settings.output_mode);
        settings.output_mode = return_policy.output_mode(settings.output_mode);
        if settings.application_excluded() {
            return Err(relay::redacted(super::failed(
                METHOD,
                &DictationError::ApplicationExcluded,
            )));
        }
        Ok((settings, return_policy))
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

        // Before the stream opens, so a client that opened badly learns it
        // from the call rather than from an event it has to interpret.
        let opening = relay::open(&mut requests)
            .await
            .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
        let (settings, return_policy) = self.session_settings()?;
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let session = Session {
            client: Client {
                events,
                shutdown: self.shutdown.clone(),
            },
            microphone: Arc::clone(&self.microphone),
            dictation: Arc::clone(&self.dictation),
            opening,
            return_policy,
            settings,
        };
        // The session's press claims the recorder once the first audio
        // arrives.
        tokio::spawn(session.run(requests));
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

/// What the session tells the relay client, and what ends the session
/// early: the daemon stopping, or the client no longer reading.
struct Client {
    events: mpsc::Sender<Result<wire::RelayEvent, Status>>,
    shutdown: CancellationToken,
}

/// One relayed recording: its audio forwarded to a press on the Mac as it
/// arrives, then the text, then the phone's ask for Return after it.
struct Session {
    client: Client,
    /// Taken for any Return pressed after the text, so no recording types
    /// under it.
    microphone: Arc<Gate>,
    dictation: Arc<DictationRuntime>,
    opening: Opening,
    return_policy: ReturnPolicy,
    settings: DictationSettings,
}

impl Session {
    async fn run(self, mut requests: Streaming<wire::RelayRequest>) {
        let Self {
            client,
            microphone,
            dictation,
            opening,
            return_policy,
            settings,
        } = self;
        let first = match relay::first_audio(&mut requests, opening, &client.shutdown).await {
            Ok(Some(first)) => first,
            Ok(None) => {
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
        let sent = first.len() * 2;
        let (feed, rest) = std::sync::mpsc::sync_channel(FEED_CHUNKS);
        let (capture_ended, capturing) = oneshot::channel();
        // Runs once the press's capture ends, as when the Mac stops it, so
        // the phone hears TRANSCRIBING before the text. A session that
        // already ended has no one to tell.
        let samples = PushedFeed::new(first, rest, relay::IDLE_TIMEOUT, move || {
            let _ = capture_ended.send(());
        });

        let abort = CancellationToken::new();
        let cancel = client.cancel_with(&abort);
        let device = opening.device();
        let job = tokio::task::spawn_blocking({
            let dictation = Arc::clone(&dictation);
            move || {
                let pressing = cancel.clone();
                dictation.dictate_pushed(samples, device, settings, cancel, move |executed| {
                    let after = return_policy.after_text(executed.return_offer, &pressing)?;
                    Ok(Heard {
                        text: executed.result.text,
                        delivered: executed.delivered,
                        after,
                    })
                })
            }
        });
        let forwarding = relay::forward(&mut requests, opening, feed, sent);
        let (ended, joined) = follow_press(&client, capturing, job, forwarding, &abort).await;
        let channel = match ended {
            Ok(PushEnd::ClientStopped(channel)) => channel,
            // The client is still recording, or gone, so it won't ask.
            Ok(PushEnd::PressEnded) => ReplyChannel::Closed,
            Err(error) => {
                client
                    .emit(Err(relay::redacted(super::failed(METHOD, &error))))
                    .await;
                return;
            }
        };
        let heard = match joined {
            Ok(Ok(heard)) => heard,
            Ok(Err(error)) if is_silence(&error) => {
                tracing::info!(method = METHOD, status = "no_speech", "request served");
                client.emit(Ok(RelayStage::NoSpeech.into())).await;
                return;
            }
            Ok(Err(_error)) if client.shutdown.is_cancelled() => {
                client
                    .emit(Err(relay::redacted(super::failed(
                        METHOD,
                        &RelayError::ShuttingDown,
                    ))))
                    .await;
                return;
            }
            Ok(Err(error)) => {
                client
                    .emit(Err(relay::redacted(super::failed(
                        METHOD,
                        RpcFailure::from(&error),
                    ))))
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

/// Runs until the press ends, and says what ended the pushing. The client
/// hears TRANSCRIBING once, as the press's capture ends, whichever side ends
/// it. A press refused before capture never says it, so the client hears
/// only the refusal.
async fn follow_press<J, F>(
    client: &Client,
    mut capturing: oneshot::Receiver<()>,
    job: J,
    forwarding: F,
    abort: &CancellationToken,
) -> (Result<PushEnd, RelayError>, J::Output)
where
    J: Future,
    F: Future<Output = Result<PushEnd, RelayError>>,
{
    tokio::pin!(job);
    tokio::pin!(forwarding);
    let mut is_listening = true;
    let mut pushed = None;
    loop {
        tokio::select! {
            // A capture ends before its press can, so polling it first tells
            // TRANSCRIBING ahead of the press's answer.
            biased;
            told = &mut capturing, if is_listening => {
                is_listening = false;
                if told.is_ok() {
                    client.emit(Ok(RelayStage::Transcribing.into())).await;
                }
            }
            // Ahead of the press, so a stop that arrived keeps its channel.
            ended = &mut forwarding, if pushed.is_none() => {
                if ended.is_err() {
                    abort.cancel();
                }
                pushed = Some(ended);
            }
            joined = &mut job => {
                // A capture can end between the poll above and this one.
                if is_listening && capturing.try_recv().is_ok() {
                    client.emit(Ok(RelayStage::Transcribing.into())).await;
                }
                // Still pushing: the client is recording, or gone.
                break (pushed.unwrap_or(Ok(PushEnd::PressEnded)), joined);
            }
        }
    }
}

/// What the phone's Return may do after this recording's text. Decided at
/// admission, so a settings change applies from the next recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnPolicy {
    /// The Mac doesn't let the phone press Return. The phone may write the
    /// output mode, so insert and press Return only types.
    Never,
    Offer,
    /// As soon as the text lands, under the recording's claim.
    Press,
}

impl ReturnPolicy {
    fn new(permitted: bool, output_mode: OutputMode) -> Self {
        match (permitted, output_mode) {
            (false, _) => Self::Never,
            (true, OutputMode::InsertAndSubmit) => Self::Press,
            (true, _) => Self::Offer,
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
                .emit(Ok(RelayStage::CompletedAndSubmitted {
                    text: heard.text,
                    delivered: heard.delivered,
                }
                .into()))
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
            delivered: heard.delivered,
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

    /// As [`Client::cancel`], and also once `abort` fires, as when the
    /// client's stream breaks mid-recording.
    fn cancel_with(&self, abort: &CancellationToken) -> CancelToken {
        let client = self.cancel();
        let abort = abort.clone();
        CancelToken::from_fn(move || client.is_cancelled() || abort.is_cancelled())
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
        let _claim = microphone
            .admit_return_press()
            .ok_or(DictationError::Busy)?;
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
    delivered: Option<Delivered>,
    after: AfterText,
}

enum AfterText {
    Offered(ReturnOffer),
    Submitted,
    Nothing,
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

    use aria_voice::dictation::DeliveryOutcome;

    use super::*;

    fn client() -> (Client, mpsc::Receiver<Result<wire::RelayEvent, Status>>) {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let client = Client {
            events,
            shutdown: CancellationToken::new(),
        };
        (client, receiver)
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
            delivered: Some(Delivered {
                outcome: DeliveryOutcome::Inserted,
                words: None,
            }),
            after,
        };
        let (client, mut receiver) = client();

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
    async fn completed_says_how_many_words_were_not_typed() {
        let heard = Heard {
            text: "hello over there".to_owned(),
            delivered: Some(Delivered {
                outcome: DeliveryOutcome::NotTyped,
                words: Some(2),
            }),
            after: AfterText::Nothing,
        };
        let (client, mut receiver) = client();

        let mut requests = tokio_stream::empty();
        complete(
            heard,
            ReplyChannel::Closed,
            &Gate::new(),
            &client,
            &mut requests,
        )
        .await;

        let event = receiver.recv().await.expect("COMPLETED").expect("an event");
        assert_eq!(event.stage(), wire::RelayStage::Completed);
        assert_eq!(event.delivery(), wire::DeliveryOutcome::NotTyped);
        assert_eq!(event.word_count, 2);
    }

    /// The events `follow_press` emitted, and what it says ended the pushing.
    async fn followed<J: Future>(
        capturing: oneshot::Receiver<()>,
        job: J,
        forwarding: impl Future<Output = Result<PushEnd, RelayError>>,
    ) -> (
        Vec<Result<wire::RelayEvent, Status>>,
        Result<PushEnd, RelayError>,
    ) {
        let (client, mut receiver) = client();
        let (ended, _joined) = follow_press(
            &client,
            capturing,
            job,
            forwarding,
            &CancellationToken::new(),
        )
        .await;
        drop(client);
        let mut emitted = Vec::new();
        while let Some(event) = receiver.recv().await {
            emitted.push(event);
        }
        (emitted, ended)
    }

    #[tokio::test]
    async fn a_press_refused_before_capture_never_says_transcribing() {
        let (refused_feed, capturing) = oneshot::channel::<()>();
        drop(refused_feed);

        let (events, ended) = followed(
            capturing,
            std::future::ready(Err::<(), _>(DictationError::Busy)),
            std::future::ready(Ok(PushEnd::PressEnded)),
        )
        .await;

        assert!(events.is_empty());
        assert!(matches!(ended, Ok(PushEnd::PressEnded)));
    }

    #[tokio::test]
    async fn an_ended_capture_says_transcribing_once_before_the_press_answers() {
        let (capture_ended, capturing) = oneshot::channel();
        capture_ended.send(()).expect("the session listens");

        let (events, ended) = followed(
            capturing,
            std::future::ready(()),
            std::future::ready(Ok(PushEnd::ClientStopped(ReplyChannel::Open))),
        )
        .await;

        assert_eq!(events.len(), 1);
        assert_eq!(stage(&events[0]), (wire::RelayStage::Transcribing, false));
        assert!(matches!(
            ended,
            Ok(PushEnd::ClientStopped(ReplyChannel::Open))
        ));
    }

    #[tokio::test]
    async fn a_capture_that_ends_as_the_press_does_still_says_transcribing() {
        let (capture_ended, capturing) = oneshot::channel();
        let mut capture_ended = Some(capture_ended);
        // Capture ends after its notice was polled, and the press with it.
        let job = std::future::poll_fn(move |_| {
            if let Some(ended) = capture_ended.take() {
                ended.send(()).expect("the session listens");
            }
            std::task::Poll::Ready(())
        });

        let (events, _ended) = followed(capturing, job, std::future::pending()).await;

        assert_eq!(events.len(), 1);
        assert_eq!(stage(&events[0]), (wire::RelayStage::Transcribing, false));
    }

    #[test]
    fn the_permission_decides_what_the_phones_return_may_do() {
        use OutputMode::{Copy, Insert, InsertAndSubmit, InsertOrCopy};
        for (permitted, chosen, policy, output_mode) in [
            (false, Insert, ReturnPolicy::Never, Insert),
            (false, InsertAndSubmit, ReturnPolicy::Never, Insert),
            (false, InsertOrCopy, ReturnPolicy::Never, InsertOrCopy),
            (true, Insert, ReturnPolicy::Offer, Insert),
            (true, InsertOrCopy, ReturnPolicy::Offer, InsertOrCopy),
            (true, InsertAndSubmit, ReturnPolicy::Press, Insert),
            (true, Copy, ReturnPolicy::Offer, Copy),
        ] {
            let decided = ReturnPolicy::new(permitted, chosen);

            let case = format!("{permitted} {chosen:?}");
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
        let _recording = microphone
            .admit_return_press()
            .expect("the microphone is free");
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
