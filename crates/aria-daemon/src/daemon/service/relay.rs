//! Receives LAN audio and dictates it through the press pipeline.
//! [`crate::daemon::relay`] validates the stream and forwards its audio to
//! the press.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use aria_kit::audio::PushedFeed;
use aria_kit::cancel::CancelToken;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request, Response, Status, Streaming};

use crate::ipc::wire;
use aria_platform::delivery::OutputMode;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_voice::dictation::{Delivered, DictationError, DictationSettings, ReturnOffer};
use aria_voice::microphone::Gate;

use super::super::capture::FEED_CHUNKS;
use super::super::dictation::{DictationRuntime, DictationRuntimeError, RelaySettings};
use super::super::relay::{
    self, Opened, Opening, PushEnd, RecordingId, RelayError, RelayStage, Reply, ReplyChannel,
};

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
    resumable: Arc<Resumable<Streaming<wire::RelayRequest>>>,
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
            resumable: Arc::default(),
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
type Events = mpsc::Sender<Result<wire::RelayEvent, Status>>;

#[tonic::async_trait]
impl wire::relay_service_server::RelayService for RelayRpc {
    type RelayStream = EventStream;

    async fn relay(
        &self,
        request: Request<Streaming<wire::RelayRequest>>,
    ) -> Result<Response<Self::RelayStream>, Status> {
        let phone = relay::presented(&request);
        let mut requests = request.into_inner();

        // Before the stream opens, so a client that opened badly learns it
        // from the call rather than from an event it has to interpret.
        let opened = relay::open(&mut requests)
            .await
            .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        match opened {
            Opened::Resumed(recording) => {
                self.resumable
                    .resume(&(phone, recording), requests, events)
                    .await
                    .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
            }
            Opened::Started(opening, recording) => {
                let (settings, return_policy) = self.session_settings()?;
                let (client, continuations) = match recording {
                    Some(recording) => {
                        let client = Client::continuable(events, self.shutdown.clone());
                        let continuations = self
                            .resumable
                            .open((phone, recording), client.settled.clone())
                            .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
                        (client, Some(continuations))
                    }
                    None => (Client::new(events, self.shutdown.clone()), None),
                };
                let session = Session {
                    client,
                    microphone: Arc::clone(&self.microphone),
                    dictation: Arc::clone(&self.dictation),
                    opening,
                    return_policy,
                    settings,
                    continuations,
                };
                // The session's press claims the recorder once the first
                // audio arrives.
                tokio::spawn(session.run(requests));
            }
        }
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

/// What the session tells the relay client, and what ends the session
/// early: the daemon stopping, or the client no longer reading.
struct Client {
    listener: Arc<Mutex<Listener>>,
    /// Fires once no stream can take over the recording.
    settled: CancellationToken,
    shutdown: CancellationToken,
}

/// Who reads the session's events.
enum Listener {
    /// A stream no other takes over from. Its client not reading ends the
    /// session.
    Last(Events),
    /// The stream of a named recording the Mac still takes audio for, which
    /// another can take over from. Its client not reading ends nothing: it
    /// may come back.
    Replaceable(Events),
    /// A named recording's stream failed, and no other continues it yet.
    Away,
    /// The stream failed, and none continues the recording any more.
    Gone,
}

/// The phone that started a recording, by its certificate, and the name it
/// gave the recording. Only that phone continues it.
type Named = (Option<[u8; 32]>, RecordingId);

/// The named recordings under way, which a new stream can continue.
struct Resumable<S>(Mutex<HashMap<Named, Offered<S>>>);

/// Where a stream asks to continue one recording.
struct Offered<S> {
    streams: mpsc::Sender<Continuation<S>>,
    /// Fires once the recording takes no more streams.
    settled: CancellationToken,
}

impl<S> Clone for Offered<S> {
    fn clone(&self) -> Self {
        Self {
            streams: self.streams.clone(),
            settled: self.settled.clone(),
        }
    }
}

impl<S> Default for Resumable<S> {
    fn default() -> Self {
        Self(Mutex::default())
    }
}

/// A stream asking to continue a recording.
struct Continuation<S> {
    requests: S,
    events: Events,
    /// Hears that the recording took the stream.
    taken: oneshot::Sender<()>,
}

impl<S> Resumable<S> {
    /// Lets streams ask to continue `named` until `settled` fires. The name
    /// stays taken until the returned value drops.
    fn open(
        self: &Arc<Self>,
        named: Named,
        settled: CancellationToken,
    ) -> Result<Continuations<S>, RelayError> {
        // One at a time: the session answers each before it reads the next.
        let (sender, streams) = mpsc::channel(1);
        let mut recordings = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if recordings.contains_key(&named) {
            return Err(RelayError::RecordingIdInUse);
        }
        let offered = Offered {
            streams: sender,
            settled,
        };
        recordings.insert(named.clone(), offered);
        Ok(Continuations {
            resumable: Arc::clone(self),
            named,
            streams,
        })
    }

    /// Hands a stream to the recording `named`, and returns once it took it.
    /// A recording that takes no more streams refuses it at once.
    async fn resume(&self, named: &Named, requests: S, events: Events) -> Result<(), RelayError> {
        let recording = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(named)
            .cloned()
            .ok_or(RelayError::RecordingOver)?;
        let (taken, answer) = oneshot::channel();
        let continuation = Continuation {
            requests,
            events,
            taken,
        };
        let taking = async {
            recording
                .streams
                .send(continuation)
                .await
                .map_err(|_over| RelayError::RecordingOver)?;
            answer.await.map_err(|_over| RelayError::RecordingOver)
        };
        tokio::select! {
            // The answer first: a stream the recording took just before it
            // settled is its stream.
            biased;
            taken = taking => taken,
            () = recording.settled.cancelled() => Err(RelayError::RecordingOver),
        }
    }
}

/// The streams asking to continue one recording, while it can be continued.
struct Continuations<S> {
    resumable: Arc<Resumable<S>>,
    named: Named,
    streams: mpsc::Receiver<Continuation<S>>,
}

impl<S> Continuations<S> {
    async fn next(&mut self) -> Continuation<S> {
        match self.streams.recv().await {
            Some(continuation) => continuation,
            // `resumable` holds a sender until this drops.
            None => std::future::pending().await,
        }
    }
}

impl<S> Drop for Continuations<S> {
    fn drop(&mut self) {
        self.resumable
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.named);
    }
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
    /// For a recording its client named.
    continuations: Option<Continuations<Streaming<wire::RelayRequest>>>,
}

impl Session {
    async fn run(self, requests: Streaming<wire::RelayRequest>) {
        let Self {
            client,
            microphone,
            dictation,
            opening,
            return_policy,
            settings,
            continuations,
        } = self;
        let mut requests = std::pin::pin!(continued(requests, continuations, &client));
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
        let samples = PushedFeed::new(first, rest, opening.stall(), move || {
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

/// A recording's requests, from the stream that started it and from each
/// that continues it. A stream that fails, unless its client cancelled it,
/// leaves a named recording waiting for the next. The next can also arrive
/// before this Mac sees the failure. It hears first how much audio the Mac
/// took, and sends on from there.
///
/// Streams change only between requests: the caller asks for the next once
/// it is done with the last, so every chunk counted as taken has reached the
/// press, in order. Once `client` is settled no stream takes over, and a
/// failure is the caller's to read.
fn continued<'a, S>(
    stream: S,
    continuations: Option<Continuations<S>>,
    client: &'a Client,
) -> impl Stream<Item = Result<wire::RelayRequest, Status>> + 'a
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin + 'a,
{
    let streams = Streams {
        current: stream,
        continuations,
        client,
        taken: 0,
    };
    // Fused: the session reads on after a half-close ended one stream.
    futures_util::stream::unfold(streams, |mut streams| async move {
        let request = streams.next().await?;
        Some((request, streams))
    })
    .fuse()
}

/// The streams of one recording, one at a time.
struct Streams<'a, S> {
    current: S,
    /// The streams asking to take over, until the recording settles.
    continuations: Option<Continuations<S>>,
    client: &'a Client,
    /// The audio bytes handed on, from every stream.
    taken: u64,
}

impl<S> Streams<'_, S>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    async fn next(&mut self) -> Option<Result<wire::RelayRequest, Status>> {
        loop {
            let Some(continuations) = self.continuations.as_mut() else {
                return self.current.next().await;
            };
            let settled = &self.client.settled;
            let continuation = tokio::select! {
                biased;
                () = settled.cancelled() => None,
                // Ahead of a stream that takes over, so audio that already
                // arrived counts as taken before the next stream hears the
                // count.
                request = self.current.next() => match request {
                    Some(Err(status))
                        if status.code() != Code::Cancelled && self.client.leave() =>
                    {
                        tracing::info!(
                            method = METHOD,
                            "the relay client's stream failed; its recording waits for another"
                        );
                        tokio::select! {
                            biased;
                            () = settled.cancelled() => {
                                self.continuations = None;
                                return Some(Err(status));
                            }
                            continuation = continuations.next() => Some(continuation),
                        }
                    }
                    request => {
                        if let Some(Ok(wire::RelayRequest {
                            body: Some(wire::relay_request::Body::Audio(audio)),
                        })) = &request
                        {
                            self.taken += audio.chunk.len() as u64;
                        }
                        return request;
                    }
                },
                continuation = continuations.next() => Some(continuation),
            };
            let Some(Continuation {
                requests,
                events,
                taken: answer,
            }) = continuation
            else {
                self.continuations = None;
                continue;
            };
            // A stream that asked as the recording settled is refused: its
            // answer drops with it.
            if !self.client.listen_on(events) {
                self.continuations = None;
                continue;
            }
            self.current = requests;
            let taken = self.taken;
            self.client
                .emit(Ok(RelayStage::Resumed { taken }.into()))
                .await;
            let _ = answer.send(());
        }
    }
}

/// Runs until the press ends, and says what ended the pushing. The client
/// hears TRANSCRIBING once, as the press's capture ends, whichever side ends
/// it. A press refused before capture never says it, so the client hears
/// only the refusal.
///
/// The Mac takes audio only until the capture ends, the pushing ends, or the
/// press does. Each settles the client, so no stream continues the recording
/// after it. A recording still waiting for a stream then has no client to
/// hear its text or ask for Return, and one whose client leaves later has
/// none either: both cancel the press.
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
                client.settle();
                if told.is_ok() {
                    client.emit(Ok(RelayStage::Transcribing.into())).await;
                }
            }
            // Ahead of the press, so a stop that arrived keeps its channel.
            ended = &mut forwarding, if pushed.is_none() => {
                client.settle();
                if ended.is_err() {
                    abort.cancel();
                }
                pushed = Some(ended);
            }
            joined = &mut job => {
                client.settle();
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
    /// For a recording no stream continues.
    fn new(events: Events, shutdown: CancellationToken) -> Self {
        let settled = CancellationToken::new();
        settled.cancel();
        Self {
            listener: Arc::new(Mutex::new(Listener::Last(events))),
            settled,
            shutdown,
        }
    }

    /// For a named recording, which another stream can continue until
    /// [`Client::settle`].
    fn continuable(events: Events, shutdown: CancellationToken) -> Self {
        Self {
            listener: Arc::new(Mutex::new(Listener::Replaceable(events))),
            settled: CancellationToken::new(),
            shutdown,
        }
    }

    fn listener(&self) -> std::sync::MutexGuard<'_, Listener> {
        self.listener.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A client that stopped reading is not a failure worth reporting: the
    /// recording is already delivered, or already lost.
    async fn emit(&self, event: Result<wire::RelayEvent, Status>) {
        let events = match &*self.listener() {
            Listener::Last(events) | Listener::Replaceable(events) => events.clone(),
            Listener::Away | Listener::Gone => return,
        };
        drop(events.send(event).await);
    }

    /// The recording's stream failed. True when another may continue it,
    /// which it then waits for.
    fn leave(&self) -> bool {
        let mut listener = self.listener();
        let waits = matches!(*listener, Listener::Replaceable(_));
        if waits {
            *listener = Listener::Away;
        }
        waits
    }

    /// The stream that continues the recording reads the events from here
    /// on. False once the recording settled, which takes no stream.
    fn listen_on(&self, events: Events) -> bool {
        let mut listener = self.listener();
        let continues = matches!(*listener, Listener::Replaceable(_) | Listener::Away);
        if continues {
            *listener = Listener::Replaceable(events);
        }
        continues
    }

    /// No stream continues the recording from here on: the Mac takes no more
    /// of its audio. A recording waiting for a stream is left with no client.
    fn settle(&self) {
        {
            let mut listener = self.listener();
            *listener = match std::mem::replace(&mut *listener, Listener::Gone) {
                Listener::Last(events) | Listener::Replaceable(events) => Listener::Last(events),
                Listener::Away | Listener::Gone => Listener::Gone,
            };
        }
        self.settled.cancel();
    }

    fn cancel(&self) -> CancelToken {
        let shutdown = self.shutdown.clone();
        let listener = Arc::clone(&self.listener);
        CancelToken::from_fn(move || {
            shutdown.is_cancelled()
                || match &*listener.lock().unwrap_or_else(PoisonError::into_inner) {
                    Listener::Last(events) => events.is_closed(),
                    Listener::Replaceable(_) | Listener::Away => false,
                    Listener::Gone => true,
                }
        })
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
        let client = Client::new(events, CancellationToken::new());
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

    type Requests = ReceiverStream<Result<wire::RelayRequest, Status>>;

    /// A client's stream, and the end the test writes it from.
    fn stream() -> (mpsc::Sender<Result<wire::RelayRequest, Status>>, Requests) {
        let (sender, receiver) = mpsc::channel(8);
        (sender, ReceiverStream::new(receiver))
    }

    fn audio(bytes: &[u8]) -> Result<wire::RelayRequest, Status> {
        Ok(wire::RelayRequest {
            body: Some(wire::relay_request::Body::Audio(wire::RelayAudio {
                chunk: bytes.to_vec(),
            })),
        })
    }

    fn stop() -> Result<wire::RelayRequest, Status> {
        Ok(wire::RelayRequest {
            body: Some(wire::relay_request::Body::Stop(wire::RelayStop {})),
        })
    }

    fn dropped() -> Result<wire::RelayRequest, Status> {
        Err(Status::unknown("the connection dropped"))
    }

    /// The stages a client has heard so far.
    fn stages(
        events: &mut mpsc::Receiver<Result<wire::RelayEvent, Status>>,
    ) -> Vec<wire::RelayStage> {
        std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| event.expect("an event").stage())
            .collect()
    }

    const PHONE: Option<[u8; 32]> = Some([7; 32]);

    /// Long enough for what a test waits on, so a wait that would never end
    /// fails.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    /// A recording its phone named `walk`, under way and open to streams.
    struct Walk {
        opening: Opening,
        named: Named,
        resumable: Arc<Resumable<Requests>>,
        continuations: Option<Continuations<Requests>>,
        client: Client,
        events: mpsc::Receiver<Result<wire::RelayEvent, Status>>,
    }

    async fn walk() -> Walk {
        let (start, mut requests) = stream();
        start
            .send(Ok(wire::RelayRequest {
                body: Some(wire::relay_request::Body::Start(wire::RelayStart {
                    format: wire::AudioFormat::PcmS16le as i32,
                    recording_id: Some(b"walk".to_vec()),
                    ..Default::default()
                })),
            }))
            .await
            .expect("the stream is open");
        let Ok(Opened::Started(opening, Some(recording))) = relay::open(&mut requests).await else {
            panic!("a named start opens a recording");
        };
        let named = (PHONE, recording);
        let resumable = Arc::<Resumable<Requests>>::default();
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let client = Client::continuable(events, CancellationToken::new());
        let continuations = resumable
            .open(named.clone(), client.settled.clone())
            .expect("the name is free");
        Walk {
            opening,
            named,
            resumable,
            continuations: Some(continuations),
            client,
            events: receiver,
        }
    }

    #[tokio::test]
    async fn a_recording_whose_stream_failed_continues_on_the_next() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(audio(&[1, 0])).await.unwrap();
        first.send(dropped()).await.unwrap();
        let (second, continuing) = stream();
        second.send(audio(&[2, 0])).await.unwrap();
        second.send(stop()).await.unwrap();
        let (events, mut continued_events) = mpsc::channel(EVENT_CAPACITY);
        let (feed, press) = std::sync::mpsc::sync_channel(8);
        let mut requests =
            std::pin::pin!(continued(requests, walk.continuations.take(), &walk.client));

        let (ended, resumed) = tokio::join!(
            relay::forward(&mut requests, walk.opening, feed, 0),
            walk.resumable.resume(&walk.named, continuing, events),
        );
        walk.client.emit(Ok(RelayStage::Transcribing.into())).await;

        assert!(resumed.is_ok());
        assert_eq!(
            ended.expect("the recording ends on its second stream"),
            PushEnd::ClientStopped(ReplyChannel::Open)
        );
        assert_eq!(press.try_iter().collect::<Vec<_>>(), [vec![1], vec![2]]);
        assert_eq!(
            stages(&mut continued_events),
            [wire::RelayStage::Resumed, wire::RelayStage::Transcribing],
            "the new stream hears the rest"
        );
        assert!(walk.events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_stream_can_continue_a_recording_before_the_mac_sees_the_first_fail() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(audio(&[1, 0])).await.unwrap();
        let (second, continuing) = stream();
        second.send(audio(&[2, 0])).await.unwrap();
        second.send(stop()).await.unwrap();
        let (events, _continued) = mpsc::channel(EVENT_CAPACITY);
        let (feed, press) = std::sync::mpsc::sync_channel(8);
        let mut requests =
            std::pin::pin!(continued(requests, walk.continuations.take(), &walk.client));

        let (ended, resumed) = tokio::join!(
            relay::forward(&mut requests, walk.opening, feed, 0),
            walk.resumable.resume(&walk.named, continuing, events),
        );

        assert!(resumed.is_ok());
        assert_eq!(
            ended.expect("the recording ends on its second stream"),
            PushEnd::ClientStopped(ReplyChannel::Open)
        );
        assert_eq!(press.try_iter().collect::<Vec<_>>(), [vec![1], vec![2]]);
        drop(first);
    }

    /// A stream taking over while the press still takes a chunk in would
    /// hear that chunk counted, and could get its own in ahead of it.
    #[tokio::test]
    async fn a_stream_takes_over_only_once_the_press_has_every_chunk_counted() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(audio(&[1, 0])).await.unwrap();
        first.send(audio(&[2, 0])).await.unwrap();
        let (second, continuing) = stream();
        second.send(audio(&[3, 0])).await.unwrap();
        second.send(stop()).await.unwrap();
        let (events, mut continued_events) = mpsc::channel(EVENT_CAPACITY);
        // Room for one chunk, so the second waits on the press.
        let (feed, press) = std::sync::mpsc::sync_channel(1);
        let mut requests =
            std::pin::pin!(continued(requests, walk.continuations.take(), &walk.client));
        let resuming = walk.resumable.resume(&walk.named, continuing, events);
        tokio::pin!(resuming);

        let (ended, ()) = tokio::join!(
            relay::forward(&mut requests, walk.opening, feed, 0),
            async {
                let early =
                    tokio::time::timeout(std::time::Duration::from_millis(50), &mut resuming).await;
                assert!(early.is_err(), "the press still takes the second chunk in");
                assert_eq!(press.recv().expect("the first chunk"), [1]);
                assert_eq!(press.recv().expect("the second chunk"), [2]);
                resuming.await.expect("the recording takes the stream");
            },
        );

        assert_eq!(
            ended.expect("the recording ends on its second stream"),
            PushEnd::ClientStopped(ReplyChannel::Open)
        );
        assert_eq!(press.try_iter().collect::<Vec<_>>(), [vec![3]]);
        let resumed = continued_events
            .try_recv()
            .expect("the stream hears first that it continues")
            .expect("an event");
        assert_eq!(resumed.taken_bytes, 4);
    }

    /// Audio written as the first stream failed never arrived, so the
    /// client has to hear where the Mac's copy ends.
    #[tokio::test]
    async fn a_stream_that_continues_a_recording_hears_what_the_mac_took() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(audio(&[1, 0])).await.unwrap();
        first.send(dropped()).await.unwrap();
        let (second, continuing) = stream();
        second.send(stop()).await.unwrap();
        let (events, mut continued_events) = mpsc::channel(EVENT_CAPACITY);
        let (feed, _press) = std::sync::mpsc::sync_channel(8);
        let mut requests =
            std::pin::pin!(continued(requests, walk.continuations.take(), &walk.client));

        let (_ended, resumed) = tokio::join!(
            relay::forward(&mut requests, walk.opening, feed, 0),
            walk.resumable.resume(&walk.named, continuing, events),
        );

        let event = continued_events
            .try_recv()
            .expect("the stream hears first that it continues")
            .expect("an event");
        assert!(resumed.is_ok());
        assert_eq!(event.stage(), wire::RelayStage::Resumed);
        assert_eq!(event.taken_bytes, 2);
    }

    /// The connection can drop before any audio crossed it, and the phone
    /// still holds every chunk.
    #[tokio::test]
    async fn a_stream_that_failed_before_its_first_audio_is_continued_from_the_start() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(dropped()).await.unwrap();
        let (second, continuing) = stream();
        second.send(audio(&[1, 0])).await.unwrap();
        let (events, mut continued_events) = mpsc::channel(EVENT_CAPACITY);
        let shutdown = CancellationToken::new();
        let mut requests =
            std::pin::pin!(continued(requests, walk.continuations.take(), &walk.client));

        let (first_audio, resumed) = tokio::time::timeout(WAIT, async {
            tokio::join!(
                relay::first_audio(&mut requests, walk.opening, &shutdown),
                walk.resumable.resume(&walk.named, continuing, events),
            )
        })
        .await
        .expect("the recording takes the stream");

        let event = continued_events
            .try_recv()
            .expect("the stream hears first that it continues")
            .expect("an event");
        assert!(resumed.is_ok());
        assert_eq!(event.taken_bytes, 0);
        assert_eq!(first_audio.expect("the audio arrives"), Some(vec![1]));
    }

    #[tokio::test]
    async fn a_stream_its_client_cancelled_ends_a_named_recording() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first
            .send(Err(Status::cancelled("the client cancelled")))
            .await
            .unwrap();
        let (feed, _press) = std::sync::mpsc::sync_channel(8);
        let abort = CancellationToken::new();
        let cancelled = abort.clone();
        let (_capture_ended, capturing) = oneshot::channel();
        let mut requests =
            std::pin::pin!(continued(requests, walk.continuations.take(), &walk.client));

        let (ended, ()) = follow_press(
            &walk.client,
            capturing,
            async move { cancelled.cancelled().await },
            relay::forward(&mut requests, walk.opening, feed, 0),
            &abort,
        )
        .await;
        let (_second, continuing) = stream();
        let (events, _continued) = mpsc::channel(EVENT_CAPACITY);
        let late = walk.resumable.resume(&walk.named, continuing, events).await;

        assert!(matches!(ended, Err(RelayError::ClientStream(_))));
        assert!(matches!(late, Err(RelayError::RecordingOver)));
    }

    #[tokio::test]
    async fn only_the_phone_that_started_a_recording_continues_it() {
        let walk = walk().await;
        let (_other, continuing) = stream();
        let (events, _continued) = mpsc::channel(EVENT_CAPACITY);
        let other_phone = (Some([8; 32]), walk.named.1.clone());

        let resumed = walk
            .resumable
            .resume(&other_phone, continuing, events)
            .await;
        let again = walk
            .resumable
            .open(walk.named.clone(), CancellationToken::new());

        assert!(matches!(resumed, Err(RelayError::RecordingOver)));
        assert!(matches!(again, Err(RelayError::RecordingIdInUse)));
    }

    #[tokio::test]
    async fn a_recording_that_ended_frees_its_name() {
        let mut walk = walk().await;

        drop(walk.continuations.take());

        let again = walk
            .resumable
            .open(walk.named.clone(), CancellationToken::new());
        assert!(again.is_ok());
    }

    #[test]
    fn a_named_recording_outlives_its_stream_until_its_audio_is_in() {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let named = Client::continuable(events, CancellationToken::new());
        let (events, unnamed_receiver) = mpsc::channel(EVENT_CAPACITY);
        let unnamed = Client::new(events, CancellationToken::new());
        drop((receiver, unnamed_receiver));

        assert!(unnamed.cancel().is_cancelled());
        assert!(!named.cancel().is_cancelled());
        named.settle();
        assert!(named.cancel().is_cancelled());
    }

    /// The Mac ended the recording and the phone left before it said its
    /// stop. Nothing may type, or press Return, with nobody there.
    #[tokio::test]
    async fn a_client_that_leaves_once_the_mac_ended_its_recording_gets_nothing_typed() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(audio(&[1, 0])).await.unwrap();
        let (feed, _press) = std::sync::mpsc::sync_channel(8);
        let (capture_ended, capturing) = oneshot::channel();
        capture_ended.send(()).expect("the session listens");
        let abort = CancellationToken::new();
        let delivering = walk.client.cancel_with(&abort);
        let cancelled = abort.clone();
        let mut requests =
            std::pin::pin!(continued(requests, walk.continuations.take(), &walk.client));
        let leaving = async {
            // Once the phone heard the recording end.
            while walk
                .events
                .recv()
                .await
                .expect("an event")
                .expect("a stage")
                .stage()
                != wire::RelayStage::Transcribing
            {}
            first.send(dropped()).await.unwrap();
        };

        let ((ended, ()), ()) = tokio::time::timeout(WAIT, async {
            tokio::join!(
                follow_press(
                    &walk.client,
                    capturing,
                    async move { cancelled.cancelled().await },
                    relay::forward(&mut requests, walk.opening, feed, 0),
                    &abort,
                ),
                leaving,
            )
        })
        .await
        .expect("the press is cancelled");
        let (_second, continuing) = stream();
        let (events, _continued) = mpsc::channel(EVENT_CAPACITY);
        let late = walk.resumable.resume(&walk.named, continuing, events).await;

        assert!(matches!(ended, Err(RelayError::ClientStream(_))));
        assert!(delivering.is_cancelled(), "nothing is delivered");
        assert!(matches!(late, Err(RelayError::RecordingOver)));
    }

    /// A phone that never comes back must not leave its press running, and
    /// then typing, with nobody there.
    #[tokio::test]
    async fn a_recording_that_ends_while_its_stream_is_gone_is_cancelled() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(dropped()).await.unwrap();
        let (feed, _press) = std::sync::mpsc::sync_channel(8);
        let (capture_ended, capturing) = oneshot::channel();
        let abort = CancellationToken::new();
        let delivering = walk.client.cancel_with(&abort);
        let cancelled = abort.clone();
        let mut requests =
            std::pin::pin!(continued(requests, walk.continuations.take(), &walk.client));
        let ending = async {
            // Once the recording waits for a stream.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            capture_ended.send(()).expect("the session listens");
        };

        let ((ended, ()), ()) = tokio::time::timeout(WAIT, async {
            tokio::join!(
                follow_press(
                    &walk.client,
                    capturing,
                    async move { cancelled.cancelled().await },
                    relay::forward(&mut requests, walk.opening, feed, 0),
                    &abort,
                ),
                ending,
            )
        })
        .await
        .expect("the press is cancelled");

        assert!(matches!(ended, Err(RelayError::ClientStream(_))));
        assert!(delivering.is_cancelled());
        assert!(
            walk.events.try_recv().is_err(),
            "nobody is there to hear it"
        );
    }
}
