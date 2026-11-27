//! Receives LAN audio and dictates it through the press pipeline.
//! [`crate::daemon::relay`] validates the stream and forwards its audio to
//! the press.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use aria_kit::audio::{FeedEnd, PushedFeed};
use aria_kit::cancel::CancelToken;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

use crate::ipc::wire;
use aria_platform::delivery::OutputMode;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_voice::dictation::{Delivered, DictationError, DictationSettings, ReturnOffer};
use aria_voice::microphone::Gate;

use super::super::capture::FEED_CHUNKS;
use super::super::dictation::{DictationRuntime, DictationRuntimeError, RelaySettings};
use super::super::relay::{
    self, Opened, Opening, Phones, PushEnd, RecordingId, RelayError, RelayStage, Reply,
    ReplyChannel,
};
use super::super::server::lan::MAX_LAN_STREAMS;

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
    phones: Arc<Phones>,
    resumable: Arc<Resumable<Streaming<wire::RelayRequest>>>,
}

impl RelayRpc {
    pub(in crate::daemon) fn new(
        dictation: Arc<DictationRuntime>,
        microphone: Arc<Gate>,
        phones: Arc<Phones>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            dictation,
            microphone,
            shutdown,
            phones,
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
            Opened::Continued(recording) => {
                self.resumable
                    .resume(&(phone, recording), requests, events)
                    .await
                    .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
            }
            Opened::Started(opening, recording) => {
                let (settings, return_policy) = self.session_settings()?;
                // A phone revoked since the call was admitted records nothing.
                let revoked = match &phone {
                    Some(phone) => self.phones.connection(phone).ok_or_else(relay::unpaired)?,
                    None => CancellationToken::new(),
                };
                let (client, continuations) = match recording {
                    Some(recording) => {
                        let client = Client::continuable(events, revoked, self.shutdown.clone());
                        let continuations = self
                            .resumable
                            .open((phone, recording), client.settled.clone())
                            .map_err(|error| relay::redacted(super::failed(METHOD, &error)))?;
                        (client, Some(continuations))
                    }
                    None => (Client::new(events, revoked, self.shutdown.clone()), None),
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

/// What the session tells the relay client, and whether a client still
/// hears it. The daemon stopping cancels the session, and so does revoking
/// its phone.
struct Client {
    listener: Arc<Mutex<Listener>>,
    /// Fires once no stream can take over the recording.
    settled: CancellationToken,
    revoked: CancellationToken,
    shutdown: CancellationToken,
}

enum Listener {
    /// The stream of a recording its client didn't name. No stream continues
    /// it, so no `RelayCancel` could reach it once this one is gone: its
    /// client not reading cancels it.
    Only(Events),
    /// A named recording's stream no other takes over from. Once its client
    /// stops reading, no Return follows the text.
    Last(Events),
    /// The stream that started a named recording the Mac still takes audio
    /// for, which another can take over from. Its client not reading ends
    /// nothing: it may come back.
    Replaceable(Events),
    /// A stream that took over such a recording, which another can take
    /// over from in turn. It opened with CONTINUED, so it hears no
    /// RECORDING.
    Continuing(Events),
    /// A named recording's stream failed, and no other continues it yet.
    Away,
    /// The stream failed, and none continues the recording any more.
    Gone,
}

/// The phone that started a recording, by its certificate, and the name it
/// gave the recording. Only that phone continues it.
type Named = (Option<[u8; 32]>, RecordingId);

struct Resumable<S>(Mutex<HashMap<Named, Offered<S>>>);

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
        // A stream that failed frees its place on the connection while its
        // recording still waits here, so the connection's cap doesn't bound
        // them.
        let held = recordings.keys().filter(|(phone, _)| *phone == named.0);
        if held.count() >= MAX_LAN_STREAMS {
            return Err(RelayError::TooManyRecordings);
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
        let quiet_limit = relay::quiet_limit(settings.auto_stop);
        let mut requests = std::pin::pin!(continued(requests, continuations, &client, quiet_limit));
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
        client.emit_recording(quiet_limit).await;
        let sent = first.len() * 2;
        let (feed, rest) = std::sync::mpsc::sync_channel(FEED_CHUNKS);
        let (capture_ended, capturing) = oneshot::channel();
        let feed_end = Arc::new(OnceLock::new());
        // Runs once the press's capture ends, as when the Mac stops it, so
        // the phone hears TRANSCRIBING before the text. A session that
        // already ended has no one to tell. It runs before the press reads
        // the end of its audio, so the Return decision sees `feed_end`.
        let samples = PushedFeed::new(first, rest, quiet_limit, {
            let feed_end = Arc::clone(&feed_end);
            move |end| {
                let _ = feed_end.set(end);
                let _ = capture_ended.send(());
            }
        });

        let abort = CancellationToken::new();
        let cancel = client.cancel_with(&abort);
        let device = opening.device();
        let job = tokio::task::spawn_blocking({
            let dictation = Arc::clone(&dictation);
            let listens = client.listens();
            move || {
                let pressing = cancel.clone();
                dictation.dictate_pushed(samples, device, settings, cancel, move |executed| {
                    let after = return_policy.after_text(
                        executed.return_offer,
                        feed_end.get().copied(),
                        listens,
                        &pressing,
                    )?;
                    Ok(Heard {
                        text: executed.result.text,
                        delivered: executed.delivered,
                        after,
                    })
                })
            }
        });
        let (stopped, pushed) = oneshot::channel();
        let (asked, asking) = oneshot::channel();
        let pushing = push(&mut requests, opening, feed, sent, stopped, asked);
        let Followed {
            ended,
            joined,
            early,
        } = follow_press(&client, capturing, job, pushed, pushing, asking, &abort).await;
        let channel = match ended {
            Ok(PushEnd::ClientStopped) if matches!(early, Ok(Some(AfterStop::Closed))) => {
                ReplyChannel::Closed
            }
            Ok(PushEnd::ClientStopped) => ReplyChannel::Open,
            // The client is still recording, or sends nothing more, so it
            // won't ask.
            Ok(PushEnd::PressEnded | PushEnd::ClientClosed) => ReplyChannel::Closed,
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
        complete(heard, channel, early, &microphone, &client, &mut requests).await;
    }
}

/// A recording's requests, from the stream that started it and from each
/// that continues it. A stream lost with its connection leaves a named
/// recording waiting for the next. The next can also arrive
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
    quiet_limit: Duration,
) -> impl Stream<Item = Result<wire::RelayRequest, Status>> + 'a
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin + 'a,
{
    let streams = Streams {
        current: stream,
        awaits_stop: continuations.is_some(),
        continuations,
        client,
        taken: 0,
        quiet_limit,
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
    /// The streams asking to take over, until the recording settles or its
    /// client stops it.
    continuations: Option<Continuations<S>>,
    /// A named recording ends with a stop, so its stream ending before one
    /// has failed.
    awaits_stop: bool,
    client: &'a Client,
    /// The audio bytes handed on, from every stream.
    taken: u64,
    /// Told again to each stream that takes over.
    quiet_limit: Duration,
}

impl<S> Streams<'_, S>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    async fn next(&mut self) -> Option<Result<wire::RelayRequest, Status>> {
        loop {
            let Some(continuations) = self.continuations.as_mut() else {
                return match self.current.next().await {
                    None if self.awaits_stop => Some(Err(relay::ended_before_stop())),
                    request => {
                        self.awaits_stop &= !is_stop(request.as_ref());
                        request
                    }
                };
            };
            let settled = &self.client.settled;
            let continuation = tokio::select! {
                biased;
                () = settled.cancelled() => None,
                // Ahead of a stream that takes over, so audio that already
                // arrived counts as taken before the next stream hears the
                // count.
                request = self.current.next() => {
                    let status = match request {
                        Some(Err(status)) if relay::is_lost(&status) => status,
                        // A dropped connection or a cancelled RPC ends a
                        // stream as cleanly as a half-close. A client that
                        // named its recording ends it with a stop, so this
                        // stream was lost.
                        None => relay::ended_before_stop(),
                        request => {
                            if let Some(Ok(wire::RelayRequest {
                                body: Some(wire::relay_request::Body::Audio(audio)),
                            })) = &request
                            {
                                self.taken += audio.chunk.len() as u64;
                            }
                            // Its client ended the recording, so no stream
                            // takes over, and a half-close now is no failure.
                            if is_stop(request.as_ref()) {
                                self.awaits_stop = false;
                                self.continuations = None;
                            }
                            return request;
                        }
                    };
                    if !self.client.leave() {
                        return Some(Err(status));
                    }
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
            tracing::info!(
                method = METHOD,
                taken_bytes = taken,
                "the relay client's recording continues on a new stream"
            );
            self.client
                .emit(Ok(RelayStage::Continued {
                    taken,
                    quiet_limit: self.quiet_limit,
                }
                .into()))
                .await;
            let _ = answer.send(());
        }
    }
}

fn is_stop(request: Option<&Result<wire::RelayRequest, Status>>) -> bool {
    matches!(
        request,
        Some(Ok(wire::RelayRequest {
            body: Some(wire::relay_request::Body::Stop(_)),
        }))
    )
}

/// Forwards a recording's audio to its press, and tells `stopped` how that
/// ended. It then reads on until the client half-closes, and tells `asked`
/// of a submit. A client that already half-closed, or whose stream failed,
/// says nothing more, so it never returns.
async fn push<S>(
    requests: &mut S,
    opening: Opening,
    feed: std::sync::mpsc::SyncSender<Vec<i16>>,
    mut sent: usize,
    stopped: oneshot::Sender<Result<PushEnd, RelayError>>,
    asked: oneshot::Sender<()>,
) -> Result<(), RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    let ended = relay::forward(requests, opening, feed, &mut sent).await;
    let reads_on = ended.as_ref().ok().copied();
    let _ = stopped.send(ended);
    match reads_on {
        Some(ended @ (PushEnd::ClientStopped | PushEnd::PressEnded)) => {
            relay::after_audio(requests, opening, ended, sent, asked).await
        }
        Some(PushEnd::ClientClosed) | None => std::future::pending().await,
    }
}

/// What a client that ended its recording said while the Mac transcribed
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterStop {
    /// It asked for Return ahead of COMPLETED.
    Submit,
    /// It half-closed without asking, so it sends nothing more.
    Closed,
}

/// What [`follow_press`] saw: what ended the pushing, the press's answer,
/// and what the client said after its stop while the press ran. A cancel
/// said then can land as the press answers, with its text already typed.
struct Followed<T> {
    ended: Result<PushEnd, RelayError>,
    joined: T,
    early: Result<Option<AfterStop>, RelayError>,
}

/// Runs until the press ends, and says what ended the pushing. The client
/// hears TRANSCRIBING once, as the press's capture ends, whichever side ends
/// it. A press refused before capture never says it, so the client hears
/// only the refusal.
///
/// `pushing` forwards the audio, tells `pushed` how that ended, and reads
/// on, so a cancel sent while the press transcribes reaches it. `asking`
/// hears a submit sent meanwhile.
///
/// The Mac takes audio only until the capture ends, the pushing ends, or the
/// press does. Each settles the client, so no stream continues the recording
/// after it. A recording still waiting for a stream then has no client to
/// hear its text or ask for Return, and one whose client leaves later has
/// none either: both type what the Mac heard, and press no Return.
async fn follow_press<J, P>(
    client: &Client,
    mut capturing: oneshot::Receiver<()>,
    job: J,
    mut pushed: oneshot::Receiver<Result<PushEnd, RelayError>>,
    pushing: P,
    mut asking: oneshot::Receiver<()>,
    abort: &CancellationToken,
) -> Followed<J::Output>
where
    J: Future,
    P: Future<Output = Result<(), RelayError>>,
{
    tokio::pin!(job);
    tokio::pin!(pushing);
    let mut is_listening = true;
    let mut ended = None;
    let mut is_closed = false;
    let mut refused = None;
    let mut is_reading = true;
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
            end = &mut pushed, if ended.is_none() => {
                ended = Some(pushing_ended(client, abort, end));
            }
            said = &mut pushing, if is_reading => {
                is_reading = false;
                match said {
                    Ok(()) => is_closed = true,
                    Err(error) if error.is_lost_stream() => client.lose(abort),
                    Err(error) => {
                        tracing::info!(method = METHOD, %error, "the relay client ended its recording as the Mac transcribed it");
                        abort.cancel();
                        refused = Some(error);
                    }
                }
            }
            joined = &mut job => {
                client.settle();
                // A capture can end between the poll above and this one.
                if is_listening && capturing.try_recv().is_ok() {
                    client.emit(Ok(RelayStage::Transcribing.into())).await;
                }
                // The pushing can end between its poll above and this one.
                if ended.is_none() && let Ok(end) = pushed.try_recv() {
                    ended = Some(pushing_ended(client, abort, Ok(end)));
                }
                let early = match refused {
                    Some(error) => Err(error),
                    None if asking.try_recv().is_ok() => Ok(Some(AfterStop::Submit)),
                    None => Ok(is_closed.then_some(AfterStop::Closed)),
                };
                // Still pushing: the client is recording, or gone.
                break Followed {
                    ended: ended.unwrap_or(Ok(PushEnd::PressEnded)),
                    joined,
                    early,
                };
            }
        }
    }
}

/// Settles the client once its pushing ended. A named recording's lost
/// stream leaves the press typing what arrived. Any other failure cancels
/// it.
fn pushing_ended(
    client: &Client,
    abort: &CancellationToken,
    end: Result<Result<PushEnd, RelayError>, oneshot::error::RecvError>,
) -> Result<PushEnd, RelayError> {
    // `push` sends before it can return, so the sender never drops unsent.
    let ended = end.unwrap_or(Ok(PushEnd::PressEnded));
    if let Ok(end) = &ended {
        tracing::info!(method = METHOD, ?end, "the relay client's audio ended");
    }
    client.settle();
    match &ended {
        Err(error) if error.is_lost_stream() => client.lose(abort),
        Err(_) => abort.cancel(),
        Ok(_) => {}
    }
    ended
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

    /// What follows the text. Nobody may be watching a recording whose
    /// audio stalled, or whose client is gone, so it takes no offer and its
    /// Return isn't pressed. `listens` is read again just before the key, as
    /// the client can go while the press checks focus.
    fn after_text(
        self,
        return_offer: Option<ReturnOffer>,
        feed_end: Option<FeedEnd>,
        listens: impl Fn() -> bool + Send + Sync + 'static,
        cancel: &CancelToken,
    ) -> Result<AfterText, DictationError> {
        let Some(return_offer) = return_offer else {
            return Ok(AfterText::Nothing);
        };
        if feed_end == Some(FeedEnd::Stalled) || !listens() {
            return Ok(AfterText::Nothing);
        }
        match self {
            Self::Never => Ok(AfterText::Nothing),
            Self::Offer => Ok(AfterText::Offered(return_offer)),
            Self::Press => {
                let pressing = cancel.clone();
                let withheld = CancelToken::from_fn(move || pressing.is_cancelled() || !listens());
                match return_offer.press(&withheld) {
                    Ok(()) => Ok(AfterText::Submitted),
                    Err(DictationError::Cancelled) if !cancel.is_cancelled() => {
                        Ok(AfterText::Nothing)
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }
}

/// `early` is what the client said after its stop while the press ran. A
/// cancel that landed as the press answered leaves its text typed, and
/// presses no Return. One that landed after the Return went changes
/// nothing: the client hears the Return, so it doesn't send the text again.
async fn complete<S>(
    heard: Heard,
    channel: ReplyChannel,
    early: Result<Option<AfterStop>, RelayError>,
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
    let submitted = match early {
        Ok(early) => early == Some(AfterStop::Submit),
        Err(error) => {
            client
                .emit(Err(relay::redacted(super::failed(METHOD, &error))))
                .await;
            return;
        }
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
            if let Some(event) =
                submit_on_reply(return_offer, submitted, microphone, client, requests).await
            {
                client.emit(event).await;
            }
        }
        None => super::served(METHOD),
    }
}

impl Client {
    /// For a recording no stream continues. `revoked` fires once its phone
    /// is.
    fn new(events: Events, revoked: CancellationToken, shutdown: CancellationToken) -> Self {
        let settled = CancellationToken::new();
        settled.cancel();
        Self {
            listener: Arc::new(Mutex::new(Listener::Only(events))),
            settled,
            revoked,
            shutdown,
        }
    }

    /// For a named recording, which another stream can continue until
    /// [`Client::settle`].
    fn continuable(
        events: Events,
        revoked: CancellationToken,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            listener: Arc::new(Mutex::new(Listener::Replaceable(events))),
            settled: CancellationToken::new(),
            revoked,
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
            Listener::Only(events)
            | Listener::Last(events)
            | Listener::Replaceable(events)
            | Listener::Continuing(events) => events.clone(),
            Listener::Away | Listener::Gone => return,
        };
        drop(events.send(event).await);
    }

    /// Says RECORDING to the stream that started the recording. One that
    /// took it over before its first audio opened with CONTINUED, which
    /// told it the same quiet limit.
    async fn emit_recording(&self, quiet_limit: Duration) {
        if matches!(*self.listener(), Listener::Continuing(_)) {
            return;
        }
        self.emit(Ok(RelayStage::Recording { quiet_limit }.into()))
            .await;
    }

    /// The recording's stream failed. True when another may continue it,
    /// which it then waits for.
    fn leave(&self) -> bool {
        let mut listener = self.listener();
        let waits = matches!(
            *listener,
            Listener::Replaceable(_) | Listener::Continuing(_)
        );
        if waits {
            *listener = Listener::Away;
        }
        waits
    }

    /// The stream that continues the recording reads the events from here
    /// on. False once the recording settled, which takes no stream.
    fn listen_on(&self, events: Events) -> bool {
        let mut listener = self.listener();
        let continues = matches!(
            *listener,
            Listener::Replaceable(_) | Listener::Continuing(_) | Listener::Away
        );
        if continues {
            *listener = Listener::Continuing(events);
        }
        continues
    }

    /// No stream continues the recording from here on: the Mac takes no more
    /// of its audio. A recording waiting for a stream is left with no client.
    fn settle(&self) {
        {
            let mut listener = self.listener();
            *listener = match std::mem::replace(&mut *listener, Listener::Gone) {
                Listener::Only(events) => Listener::Only(events),
                Listener::Last(events)
                | Listener::Replaceable(events)
                | Listener::Continuing(events) => Listener::Last(events),
                Listener::Away | Listener::Gone => Listener::Gone,
            };
        }
        self.settled.cancel();
    }

    /// The client's stream broke and no stream continues it. A recording
    /// its client named types what the Mac heard. One it didn't name is
    /// cancelled through `abort`.
    fn lose(&self, abort: &CancellationToken) {
        let mut listener = self.listener();
        if matches!(*listener, Listener::Only(_)) {
            abort.cancel();
        }
        *listener = Listener::Gone;
    }

    /// Whether a client still hears the recording, read when asked.
    fn listens(&self) -> impl Fn() -> bool + Send + Sync + 'static {
        let listener = Arc::clone(&self.listener);
        move || match &*listener.lock().unwrap_or_else(PoisonError::into_inner) {
            Listener::Only(events)
            | Listener::Last(events)
            | Listener::Replaceable(events)
            | Listener::Continuing(events) => !events.is_closed(),
            Listener::Away | Listener::Gone => false,
        }
    }

    /// A named recording's lost client doesn't cancel the press: it types
    /// what it heard. Cancelling the RPC cancels an unnamed one.
    fn cancel(&self) -> CancelToken {
        let shutdown = self.shutdown.clone();
        let revoked = self.revoked.clone();
        let listener = Arc::clone(&self.listener);
        CancelToken::from_fn(move || {
            shutdown.is_cancelled()
                || revoked.is_cancelled()
                || matches!(
                    &*listener.lock().unwrap_or_else(PoisonError::into_inner),
                    Listener::Only(events) if events.is_closed()
                )
        })
    }

    /// As [`Client::cancel`], and also once no client hears the recording:
    /// nobody would be watching the Return.
    fn withheld(&self) -> CancelToken {
        let cancel = self.cancel();
        let listens = self.listens();
        CancelToken::from_fn(move || cancel.is_cancelled() || !listens())
    }

    /// As [`Client::cancel`], and also once `abort` fires, as when the
    /// client sends a `RelayCancel` or breaks the protocol.
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
    submitted: bool,
    microphone: &Arc<Gate>,
    client: &Client,
    requests: &mut S,
) -> Option<Result<wire::RelayEvent, Status>>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    let reply = if submitted {
        Ok(Reply::Submit)
    } else {
        relay::next_reply(requests, &client.shutdown).await
    };
    match reply {
        Ok(Reply::Submit) => {}
        Ok(reply @ (Reply::Lapsed | Reply::Closed)) => {
            tracing::info!(method = METHOD, status = "ok", ?reply, "request served");
            return None;
        }
        Err(error) => return Some(Err(relay::redacted(super::failed(METHOD, &error)))),
    }
    let microphone = Arc::clone(microphone);
    let cancel = client.withheld();
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
        let client = Client::new(events, CancellationToken::new(), CancellationToken::new());
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
        requests: S,
    ) -> (Vec<Result<wire::RelayEvent, Status>>, usize)
    where
        S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
    {
        landed(policy, typed_into, channel, Ok(None), microphone, requests).await
    }

    /// As [`after_landing`], after the client said `early` while the press
    /// ran.
    async fn landed<S>(
        policy: ReturnPolicy,
        typed_into: &str,
        channel: ReplyChannel,
        early: Result<Option<AfterStop>, RelayError>,
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
            .after_text(
                Some(return_offer),
                Some(FeedEnd::Ended),
                || true,
                &CancelToken::new(),
            )
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

        complete(heard, channel, early, microphone, &client, &mut requests).await;
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
            Ok(None),
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

    /// Pushing that reads nothing after its end.
    fn forwarding_only(
        forwarding: impl Future<Output = Result<PushEnd, RelayError>>,
    ) -> (
        oneshot::Receiver<Result<PushEnd, RelayError>>,
        impl Future<Output = Result<(), RelayError>>,
    ) {
        let (stopped, pushed) = oneshot::channel();
        let pushing = async move {
            let _ = stopped.send(forwarding.await);
            std::future::pending().await
        };
        (pushed, pushing)
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
        let (pushed, pushing) = forwarding_only(forwarding);
        let (_asked, asking) = oneshot::channel();
        let Followed { ended, .. } = follow_press(
            &client,
            capturing,
            job,
            pushed,
            pushing,
            asking,
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
            std::future::ready(Ok(PushEnd::ClientStopped)),
        )
        .await;

        assert_eq!(events.len(), 1);
        assert_eq!(stage(&events[0]), (wire::RelayStage::Transcribing, false));
        assert!(matches!(ended, Ok(PushEnd::ClientStopped)));
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

    /// The cancel is read as the press answers, with a submit before it.
    #[tokio::test]
    async fn a_cancel_that_lands_as_the_press_answers_presses_no_return() {
        let (events, presses) = landed(
            ReturnPolicy::Offer,
            "Messages",
            ReplyChannel::Open,
            Err(RelayError::ClientCancelled),
            &Gate::new(),
            tokio_stream::empty(),
        )
        .await;

        assert_eq!(presses, 0);
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err(), "the stream ends as cancelled");
    }

    #[tokio::test]
    async fn a_cancel_that_lands_after_the_return_went_still_reports_the_return() {
        let (events, presses) = landed(
            ReturnPolicy::Press,
            "Messages",
            ReplyChannel::Open,
            Err(RelayError::ClientCancelled),
            &Gate::new(),
            tokio_stream::empty(),
        )
        .await;

        assert_eq!(presses, 1);
        assert_eq!(events.len(), 1);
        assert_eq!(stage(&events[0]), (wire::RelayStage::Completed, true));
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
    const QUIET: Duration = Duration::from_secs(10);

    /// Forwards a recording's audio from its first byte.
    async fn forwarded<S>(
        requests: &mut S,
        opening: Opening,
        feed: std::sync::mpsc::SyncSender<Vec<i16>>,
    ) -> Result<PushEnd, RelayError>
    where
        S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
    {
        relay::forward(requests, opening, feed, &mut 0).await
    }

    /// A recording its phone named `walk`, under way and open to streams.
    struct Walk {
        opening: Opening,
        named: Named,
        resumable: Arc<Resumable<Requests>>,
        continuations: Option<Continuations<Requests>>,
        client: Client,
        events: mpsc::Receiver<Result<wire::RelayEvent, Status>>,
    }

    /// The opening of a recording its phone named `name`.
    async fn opened(name: &[u8]) -> (Opening, RecordingId) {
        let (start, mut requests) = stream();
        start
            .send(Ok(wire::RelayRequest {
                body: Some(wire::relay_request::Body::Start(wire::RelayStart {
                    format: wire::AudioFormat::PcmS16le as i32,
                    recording_id: Some(name.to_vec()),
                    ..Default::default()
                })),
            }))
            .await
            .expect("the stream is open");
        let Ok(Opened::Started(opening, Some(recording))) = relay::open(&mut requests).await else {
            panic!("a named start opens a recording");
        };
        (opening, recording)
    }

    async fn walk() -> Walk {
        let (opening, recording) = opened(b"walk").await;
        let named = (PHONE, recording);
        let resumable = Arc::<Resumable<Requests>>::default();
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let client =
            Client::continuable(events, CancellationToken::new(), CancellationToken::new());
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
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));

        let (ended, resumed) = tokio::join!(
            forwarded(&mut requests, walk.opening, feed),
            walk.resumable.resume(&walk.named, continuing, events),
        );
        walk.client.emit(Ok(RelayStage::Transcribing.into())).await;

        assert!(resumed.is_ok());
        assert_eq!(
            ended.expect("the recording ends on its second stream"),
            PushEnd::ClientStopped
        );
        assert_eq!(press.try_iter().collect::<Vec<_>>(), [vec![1], vec![2]]);
        assert_eq!(
            stages(&mut continued_events),
            [wire::RelayStage::Continued, wire::RelayStage::Transcribing],
            "the new stream hears the rest"
        );
        assert!(walk.events.try_recv().is_err());
    }

    /// A dead connection's stream can end with no error.
    #[tokio::test]
    async fn a_named_stream_that_ends_before_its_stop_is_continued() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(audio(&[1, 0])).await.unwrap();
        drop(first);
        let (second, continuing) = stream();
        second.send(audio(&[2, 0])).await.unwrap();
        second.send(stop()).await.unwrap();
        let (events, _continued) = mpsc::channel(EVENT_CAPACITY);
        let (feed, press) = std::sync::mpsc::sync_channel(8);
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));

        let (ended, resumed) = tokio::time::timeout(WAIT, async {
            tokio::join!(
                forwarded(&mut requests, walk.opening, feed),
                walk.resumable.resume(&walk.named, continuing, events),
            )
        })
        .await
        .expect("the second stream takes over");

        assert!(resumed.is_ok());
        assert_eq!(
            ended.expect("the recording ends on its second stream"),
            PushEnd::ClientStopped
        );
        assert_eq!(press.try_iter().collect::<Vec<_>>(), [vec![1], vec![2]]);
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
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));

        let (ended, resumed) = tokio::join!(
            forwarded(&mut requests, walk.opening, feed),
            walk.resumable.resume(&walk.named, continuing, events),
        );

        assert!(resumed.is_ok());
        assert_eq!(
            ended.expect("the recording ends on its second stream"),
            PushEnd::ClientStopped
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
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));
        let resuming = walk.resumable.resume(&walk.named, continuing, events);
        tokio::pin!(resuming);

        let (ended, ()) = tokio::join!(forwarded(&mut requests, walk.opening, feed), async {
            let early =
                tokio::time::timeout(std::time::Duration::from_millis(50), &mut resuming).await;
            assert!(early.is_err(), "the press still takes the second chunk in");
            assert_eq!(press.recv().expect("the first chunk"), [1]);
            assert_eq!(press.recv().expect("the second chunk"), [2]);
            resuming.await.expect("the recording takes the stream");
        },);

        assert_eq!(
            ended.expect("the recording ends on its second stream"),
            PushEnd::ClientStopped
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
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));

        let (_ended, resumed) = tokio::join!(
            forwarded(&mut requests, walk.opening, feed),
            walk.resumable.resume(&walk.named, continuing, events),
        );

        let event = continued_events
            .try_recv()
            .expect("the stream hears first that it continues")
            .expect("an event");
        assert!(resumed.is_ok());
        assert_eq!(event.stage(), wire::RelayStage::Continued);
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
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));

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

    fn cancel() -> Result<wire::RelayRequest, Status> {
        Ok(wire::RelayRequest {
            body: Some(wire::relay_request::Body::Cancel(wire::RelayCancel {})),
        })
    }

    /// Whether the press still takes the recording's audio.
    #[derive(Clone, Copy, PartialEq)]
    enum Press {
        Taking,
        /// The Mac ended the recording.
        Ended,
    }

    /// Runs a named recording's press from `sent` until the press sees its
    /// abort. What ended the pushing, and whether the press was aborted.
    async fn press_after(
        walk: &mut Walk,
        sent: Vec<Result<wire::RelayRequest, Status>>,
    ) -> (Result<PushEnd, RelayError>, bool) {
        aborted_press(walk, sent, Press::Taking).await
    }

    async fn aborted_press(
        walk: &mut Walk,
        sent: Vec<Result<wire::RelayRequest, Status>>,
        press: Press,
    ) -> (Result<PushEnd, RelayError>, bool) {
        let (first, requests) = stream();
        for request in sent {
            first.send(request).await.unwrap();
        }
        let (feed, taking) = std::sync::mpsc::sync_channel(8);
        let _taking = (press == Press::Taking).then_some(taking);
        let abort = CancellationToken::new();
        let aborted = abort.clone();
        let (_capture_ended, capturing) = oneshot::channel();
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));
        let (stopped, pushed) = oneshot::channel();
        let (asked, asking) = oneshot::channel();
        let Followed { ended, .. } = tokio::time::timeout(
            WAIT,
            follow_press(
                &walk.client,
                capturing,
                async move { aborted.cancelled().await },
                pushed,
                push(&mut requests, walk.opening, feed, 0, stopped, asked),
                asking,
                &abort,
            ),
        )
        .await
        .expect("the press ends");
        drop(first);
        (ended, abort.is_cancelled())
    }

    /// A named recording whose stream sent `sent` and half-closed, as its
    /// press answers.
    async fn answered_after(
        walk: &mut Walk,
        sent: Vec<Result<wire::RelayRequest, Status>>,
    ) -> Followed<()> {
        let (first, requests) = stream();
        for request in sent {
            first.send(request).await.unwrap();
        }
        drop(first);
        let (feed, _taking) = std::sync::mpsc::sync_channel(8);
        let (_capture_ended, capturing) = oneshot::channel();
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));
        let (stopped, pushed) = oneshot::channel();
        let (asked, asking) = oneshot::channel();
        follow_press(
            &walk.client,
            capturing,
            std::future::ready(()),
            pushed,
            push(&mut requests, walk.opening, feed, 0, stopped, asked),
            asking,
            &CancellationToken::new(),
        )
        .await
    }

    #[tokio::test]
    async fn a_cancel_reaches_a_press_the_mac_ended() {
        let mut walk = walk().await;
        let sent = vec![audio(&[1, 0]), cancel()];

        let (ended, aborted) = aborted_press(&mut walk, sent, Press::Ended).await;

        assert!(matches!(ended, Ok(PushEnd::PressEnded)));
        assert!(aborted, "nothing more types");
    }

    #[tokio::test]
    async fn a_cancel_after_a_submit_ahead_of_completed_cancels_the_press() {
        let mut walk = walk().await;
        let sent = vec![audio(&[1, 0]), stop(), Ok(submit()), cancel()];

        let (ended, aborted) = press_after(&mut walk, sent).await;

        assert!(matches!(ended, Ok(PushEnd::ClientStopped)));
        assert!(aborted, "no Return follows");
    }

    #[tokio::test]
    async fn a_submit_ahead_of_completed_outlasts_the_half_close_after_it() {
        let mut walk = walk().await;
        let sent = vec![audio(&[1, 0]), stop(), Ok(submit())];

        let Followed { ended, early, .. } = answered_after(&mut walk, sent).await;

        assert!(matches!(ended, Ok(PushEnd::ClientStopped)));
        assert!(matches!(early, Ok(Some(AfterStop::Submit))));
    }

    #[tokio::test]
    async fn a_half_close_right_after_the_stop_is_no_lost_stream() {
        let mut walk = walk().await;

        let Followed { early, .. } = answered_after(&mut walk, vec![audio(&[1, 0]), stop()]).await;

        assert!(matches!(early, Ok(Some(AfterStop::Closed))));
        assert!(walk.client.listens()(), "its client hears COMPLETED");
    }

    #[tokio::test]
    async fn a_cancel_read_as_the_press_answers_is_kept_for_its_answer() {
        let mut walk = walk().await;
        let sent = vec![audio(&[1, 0]), stop(), Ok(submit()), cancel()];

        let Followed { early, .. } = answered_after(&mut walk, sent).await;

        assert!(matches!(early, Err(RelayError::ClientCancelled)));
    }

    /// The response side closes when the RPC ends, which the session can
    /// read before its request side fails.
    #[test]
    fn a_client_whose_stream_stopped_reading_does_not_listen() {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let named = Client::continuable(events, CancellationToken::new(), CancellationToken::new());

        drop(receiver);

        assert!(!named.listens()());
    }

    #[tokio::test]
    async fn a_stream_that_took_over_before_the_first_audio_hears_no_recording() {
        let mut walk = walk().await;
        let (events, mut continued) = mpsc::channel(EVENT_CAPACITY);

        assert!(walk.client.listen_on(events));
        walk.client.emit_recording(QUIET).await;

        assert!(stages(&mut continued).is_empty());
        assert!(stages(&mut walk.events).is_empty());
    }

    #[tokio::test]
    async fn the_stream_that_started_a_recording_hears_recording() {
        let mut walk = walk().await;

        walk.client.emit_recording(QUIET).await;

        assert_eq!(stages(&mut walk.events), [wire::RelayStage::Recording]);
    }

    #[tokio::test]
    async fn a_named_stream_that_ends_before_its_stop_failed_even_once_settled() {
        let mut walk = walk().await;
        walk.client.settle();
        let (first, requests) = stream();
        drop(first);
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));

        let ended = requests.next().await;

        assert!(matches!(ended, Some(Err(status)) if relay::is_lost(&status)));
    }

    #[tokio::test]
    async fn a_phone_holds_only_so_many_recordings_open() {
        let resumable = Arc::<Resumable<Requests>>::default();
        let open = |phone: u8, recording: RecordingId| {
            resumable.open((Some([phone; 32]), recording), CancellationToken::new())
        };
        let mut held = Vec::new();
        for name in 0..MAX_LAN_STREAMS as u8 {
            held.push(open(7, opened(&[name]).await.1));
        }

        let one_more = open(7, opened(b"one more").await.1);
        let another_phone = open(8, opened(b"one more").await.1);

        assert!(held.iter().all(Result::is_ok));
        assert!(matches!(one_more, Err(RelayError::TooManyRecordings)));
        assert!(another_phone.is_ok());
    }

    #[tokio::test]
    async fn a_cancel_during_the_recording_cancels_the_press() {
        let mut walk = walk().await;

        let (ended, aborted) = press_after(&mut walk, vec![audio(&[1, 0]), cancel()]).await;
        let (_second, continuing) = stream();
        let (events, _continued) = mpsc::channel(EVENT_CAPACITY);
        let late = walk.resumable.resume(&walk.named, continuing, events).await;

        assert!(matches!(ended, Err(RelayError::ClientCancelled)));
        assert!(aborted, "nothing more types");
        assert!(matches!(late, Err(RelayError::RecordingOver)));
    }

    #[tokio::test]
    async fn a_cancel_after_the_stop_cancels_the_press_as_it_transcribes() {
        let mut walk = walk().await;

        let (ended, aborted) = press_after(&mut walk, vec![audio(&[1, 0]), stop(), cancel()]).await;

        assert!(matches!(ended, Ok(PushEnd::ClientStopped)));
        assert!(aborted, "nothing more types");
    }

    #[tokio::test]
    async fn a_malformed_message_ends_a_named_recording_rather_than_waiting() {
        let mut walk = walk().await;

        let (ended, aborted) = press_after(
            &mut walk,
            vec![
                audio(&[1, 0]),
                Err(Status::out_of_range("a message over the size limit")),
            ],
        )
        .await;

        assert!(matches!(ended, Err(RelayError::ClientStream(_))));
        assert!(aborted, "nothing more types");
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
    fn a_client_listens_until_its_stream_is_gone_for_good() {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let named = Client::continuable(events, CancellationToken::new(), CancellationToken::new());
        let (events, unnamed_receiver) = mpsc::channel(EVENT_CAPACITY);
        let unnamed = Client::new(events, CancellationToken::new(), CancellationToken::new());
        drop(unnamed_receiver);
        let abort = CancellationToken::new();

        assert!(!unnamed.listens()());
        assert!(named.listens()());
        named.lose(&abort);
        assert!(!named.listens()());
        assert!(!named.cancel().is_cancelled(), "the press goes on");
        assert!(!abort.is_cancelled());
        drop(receiver);
    }

    /// No stream continues an unnamed recording, so no cancel could reach it
    /// once its stream is gone.
    #[test]
    fn an_unnamed_recording_is_cancelled_with_its_rpc_or_its_stream() {
        let (cancelled, receiver) = client();
        let (lost, _receiver) = client();
        let abort = CancellationToken::new();

        drop(receiver);
        lost.lose(&abort);

        assert!(cancelled.cancel().is_cancelled());
        assert!(abort.is_cancelled());
    }

    #[test]
    fn revoking_a_phone_cancels_its_recording() {
        let (events, _receiver) = mpsc::channel(EVENT_CAPACITY);
        let revoked = CancellationToken::new();
        let client = Client::continuable(events, revoked.clone(), CancellationToken::new());

        revoked.cancel();

        assert!(client.cancel().is_cancelled());
    }

    #[tokio::test]
    async fn a_return_asked_for_early_is_withheld_once_the_client_is_gone() {
        let (return_offer, pressed) = counted_return(|| {});
        let (client, _receiver) = client();
        client.lose(&CancellationToken::new());
        let mut requests = tokio_stream::empty();

        let answered =
            submit_on_reply(return_offer, true, &Gate::new(), &client, &mut requests).await;

        assert!(matches!(answered, Some(Err(_))));
        assert_eq!(pressed.load(Ordering::SeqCst), 0);
    }

    /// A Return offer that counts its presses, as the Mac's delivery runs
    /// it: refused once `cancel` fires at its last check.
    fn counted_return(
        before_the_key: impl Fn() + Send + 'static,
    ) -> (ReturnOffer, Arc<AtomicUsize>) {
        let pressed = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&pressed);
        let return_offer = testing::return_offer(None, move |cancel| {
            before_the_key();
            if cancel.is_cancelled() {
                return Err(aria_platform::delivery::DeliveryError::Cancelled);
            }
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        (return_offer, pressed)
    }

    #[test]
    fn a_client_that_is_gone_gets_no_return() {
        let (return_offer, pressed) = counted_return(|| {});

        let after = ReturnPolicy::Press
            .after_text(
                Some(return_offer),
                Some(FeedEnd::Ended),
                || false,
                &CancelToken::new(),
            )
            .expect("nothing fails");

        assert!(matches!(after, AfterText::Nothing));
        assert_eq!(pressed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_recording_whose_audio_stalled_gets_no_return() {
        let (return_offer, pressed) = counted_return(|| {});

        let after = ReturnPolicy::Press
            .after_text(
                Some(return_offer),
                Some(FeedEnd::Stalled),
                || true,
                &CancelToken::new(),
            )
            .expect("nothing fails");

        assert!(matches!(after, AfterText::Nothing));
        assert_eq!(pressed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_client_gone_by_the_last_check_before_the_key_gets_no_return() {
        let listening = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let leaves = Arc::clone(&listening);
        // The client goes while the press checks focus.
        let (return_offer, pressed) = counted_return(move || leaves.store(false, Ordering::SeqCst));

        let after = ReturnPolicy::Press
            .after_text(
                Some(return_offer),
                Some(FeedEnd::Ended),
                move || listening.load(Ordering::SeqCst),
                &CancelToken::new(),
            )
            .expect("a withheld Return is no failure");

        assert!(matches!(after, AfterText::Nothing));
        assert_eq!(pressed.load(Ordering::SeqCst), 0);
    }

    /// The Mac ended the recording and the phone left before it said its
    /// stop. The text still types, with no Return.
    #[tokio::test]
    async fn a_client_that_leaves_once_the_mac_ended_its_recording_still_gets_its_text_typed() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(audio(&[1, 0])).await.unwrap();
        let (feed, _press) = std::sync::mpsc::sync_channel(8);
        let (capture_ended, capturing) = oneshot::channel();
        capture_ended.send(()).expect("the session listens");
        let abort = CancellationToken::new();
        let delivering = walk.client.cancel_with(&abort);
        let (left, has_left) = oneshot::channel::<()>();
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));
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
            left.send(()).expect("the press waits");
        };

        let (stopped, pushed) = oneshot::channel();
        let (asked, asking) = oneshot::channel();
        let (Followed { ended, .. }, ()) = tokio::time::timeout(WAIT, async {
            tokio::join!(
                follow_press(
                    &walk.client,
                    capturing,
                    async move {
                        let _ = has_left.await;
                    },
                    pushed,
                    push(&mut requests, walk.opening, feed, 0, stopped, asked),
                    asking,
                    &abort,
                ),
                leaving,
            )
        })
        .await
        .expect("the press ends");
        let (_second, continuing) = stream();
        let (events, _continued) = mpsc::channel(EVENT_CAPACITY);
        let late = walk.resumable.resume(&walk.named, continuing, events).await;

        assert!(matches!(ended, Err(RelayError::ClientStream(_))));
        assert!(!delivering.is_cancelled(), "the text types");
        assert!(!walk.client.listens()(), "so no Return follows it");
        assert!(matches!(late, Err(RelayError::RecordingOver)));
    }

    /// A phone that never comes back leaves what the Mac heard, which still
    /// types.
    #[tokio::test]
    async fn a_recording_that_ends_while_its_stream_is_gone_types_what_it_heard() {
        let mut walk = walk().await;
        let (first, requests) = stream();
        first.send(dropped()).await.unwrap();
        let (feed, _press) = std::sync::mpsc::sync_channel(8);
        let (capture_ended, capturing) = oneshot::channel();
        let abort = CancellationToken::new();
        let delivering = walk.client.cancel_with(&abort);
        let (typed, has_typed) = oneshot::channel::<()>();
        let mut requests = std::pin::pin!(continued(
            requests,
            walk.continuations.take(),
            &walk.client,
            QUIET
        ));
        let ending = async {
            // Once the recording waits for a stream.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            capture_ended.send(()).expect("the session listens");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            typed.send(()).expect("the press waits");
        };

        let (stopped, pushed) = oneshot::channel();
        let (asked, asking) = oneshot::channel();
        let (Followed { ended, .. }, ()) = tokio::time::timeout(WAIT, async {
            tokio::join!(
                follow_press(
                    &walk.client,
                    capturing,
                    async move {
                        let _ = has_typed.await;
                    },
                    pushed,
                    push(&mut requests, walk.opening, feed, 0, stopped, asked),
                    asking,
                    &abort,
                ),
                ending,
            )
        })
        .await
        .expect("the press ends");

        assert!(matches!(ended, Err(RelayError::ClientStream(_))));
        assert!(!delivering.is_cancelled(), "the text types");
        assert!(!walk.client.listens()(), "with no Return");
        assert!(
            walk.events.try_recv().is_err(),
            "nobody is there to hear it"
        );
    }
}
