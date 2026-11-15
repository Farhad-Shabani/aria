//! A paired phone offers its microphone over the LAN.
//! [`crate::daemon::phone`] owns the attached phone.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

use crate::ipc::protocol::ProtocolError;
use crate::ipc::wire;
use aria_settings::Microphone;

use super::super::phone::{
    AdmitsPhone, Attachment, Detached, PhoneMicrophone, PhoneMicrophoneError, PhoneRequest,
};

const METHOD: &str = "PhoneMicrophoneService/Attach";

/// How long a phone may take to send its offer.
const OFFER_TIMEOUT: Duration = Duration::from_secs(10);

/// Room for the commands and the closing status, so the stream never waits
/// on a phone slow to read.
const OUT_CAPACITY: usize = 8;

type Outbound = mpsc::Sender<Result<wire::PhoneMicrophoneCommand, Status>>;

/// Served on the LAN only, behind the paired-phone check.
pub(in crate::daemon) struct PhoneMicrophoneRpc {
    phones: Arc<PhoneMicrophone>,
    admits: AdmitsPhone,
    preference: watch::Receiver<Microphone>,
    /// The daemon's stop, so an attached phone doesn't hold the drain open.
    shutdown: CancellationToken,
}

impl PhoneMicrophoneRpc {
    pub(in crate::daemon) fn new(
        phones: Arc<PhoneMicrophone>,
        admits: AdmitsPhone,
        preference: watch::Receiver<Microphone>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            phones,
            admits,
            preference,
            shutdown,
        }
    }
}

type CommandStream =
    Pin<Box<dyn Stream<Item = Result<wire::PhoneMicrophoneCommand, Status>> + Send>>;

#[tonic::async_trait]
impl wire::phone_microphone_service_server::PhoneMicrophoneService for PhoneMicrophoneRpc {
    type AttachStream = CommandStream;

    async fn attach(
        &self,
        request: Request<Streaming<wire::PhoneMicrophoneRequest>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        let commands = answer(
            Arc::clone(&self.phones),
            request.into_inner(),
            self.admits,
            &self.preference,
            self.shutdown.clone(),
        )
        .await?;
        Ok(Response::new(commands))
    }
}

/// Admits the phone before answering. The phone reads the response's
/// headers as acceptance, so a refused phone gets only the refusal.
async fn answer<S>(
    phones: Arc<PhoneMicrophone>,
    mut stream: S,
    admits: AdmitsPhone,
    preference: &watch::Receiver<Microphone>,
    shutdown: CancellationToken,
) -> Result<CommandStream, Status>
where
    S: Stream<Item = Result<wire::PhoneMicrophoneRequest, Status>> + Unpin + Send + 'static,
{
    open(&mut stream)
        .await
        .map_err(|error| super::failed(METHOD, &error))?;
    let admitted = admit(phones, admits, preference).await?;
    let (out, commands) = mpsc::channel(OUT_CAPACITY);
    tokio::spawn(async move {
        match serve(admitted, stream, &out, &shutdown).await {
            Ok(ended) => {
                tracing::info!(method = METHOD, status = ended.label(), "request served");
            }
            Err(status) => {
                // The phone may already be gone, with nobody to tell.
                let _ = out.send(Err(status)).await;
            }
        }
    });
    Ok(Box::pin(ReceiverStream::new(commands)))
}

/// How a phone's stream ended without a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// The phone closed its stream.
    Left,
    /// A newer phone offered.
    Replaced,
    /// It sent no audio after Start.
    Silent,
    /// It fell too far behind to take Stop.
    Behind,
}

impl Ended {
    fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Replaced => "replaced",
            Self::Silent => "silent",
            Self::Behind => "behind",
        }
    }
}

/// Reads the phone's offer. Anything else first ends the stream.
async fn open<S>(stream: &mut S) -> Result<(), PhoneMicrophoneError>
where
    S: Stream<Item = Result<wire::PhoneMicrophoneRequest, Status>> + Unpin,
{
    let first = tokio::time::timeout(OFFER_TIMEOUT, stream.next())
        .await
        .map_err(|_elapsed| PhoneMicrophoneError::NoOffer)?;
    match first {
        Some(Ok(request)) => match Option::<PhoneRequest>::try_from(request) {
            Ok(Some(PhoneRequest::Offer)) => Ok(()),
            Ok(_) | Err(_) => Err(PhoneMicrophoneError::NoOffer),
        },
        None => Err(PhoneMicrophoneError::NoOffer),
        Some(Err(status)) if is_oversized(&status) => Err(PhoneMicrophoneError::NoOffer),
        Some(Err(_status)) => Err(PhoneMicrophoneError::ClientGone),
    }
}

/// The attached phone, once a press would record on it. Dropping it
/// detaches the phone, so no exit path leaves one attached.
struct Admitted {
    phones: Arc<PhoneMicrophone>,
    attachment: Attachment,
}

impl Drop for Admitted {
    fn drop(&mut self) {
        self.phones.detach(self.attachment.generation);
        tracing::info!("the phone's microphone detached");
    }
}

/// Attaches the phone, then checks that a press would record on it.
async fn admit(
    phones: Arc<PhoneMicrophone>,
    admits: AdmitsPhone,
    preference: &watch::Receiver<Microphone>,
) -> Result<Admitted, Status> {
    let attachment = phones.attach();
    let admitted = Admitted { phones, attachment };
    tracing::info!("a phone offered its microphone");
    // Checked once attached, so the capture finds this phone when a later
    // change rules it out. The rule can list the audio devices.
    let preference = preference.borrow().clone();
    if super::blocking(METHOD, move || admits(&preference)).await? {
        Ok(admitted)
    } else {
        Err(super::failed(METHOD, &PhoneMicrophoneError::Unsupported))
    }
}

/// Serves an admitted phone until it leaves, a newer one attaches, the
/// capture dismisses it, or the daemon stops. Every command the capture
/// sends goes out on `out`.
async fn serve<S>(
    mut admitted: Admitted,
    stream: S,
    out: &Outbound,
    shutdown: &CancellationToken,
) -> Result<Ended, Status>
where
    S: Stream<Item = Result<wire::PhoneMicrophoneRequest, Status>> + Unpin,
{
    let Admitted { phones, attachment } = &mut admitted;
    pump(phones, attachment, stream, out, shutdown).await
}

/// Carries the capture's commands out and the phone's requests in.
async fn pump<S>(
    phones: &PhoneMicrophone,
    attachment: &mut Attachment,
    mut stream: S,
    out: &Outbound,
    shutdown: &CancellationToken,
) -> Result<Ended, Status>
where
    S: Stream<Item = Result<wire::PhoneMicrophoneRequest, Status>> + Unpin,
{
    let Attachment {
        generation,
        commands,
        ended,
    } = attachment;
    let generation = *generation;
    let failed = |error: PhoneMicrophoneError| Err(super::failed(METHOD, &error));
    loop {
        let stop_deadline = phones.stop_deadline(generation);
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return failed(PhoneMicrophoneError::ShuttingDown),
            why = &mut *ended => return match why {
                Ok(Detached::Dismissed) => failed(PhoneMicrophoneError::Unsupported),
                Ok(Detached::Replaced) => Ok(Ended::Replaced),
                Ok(Detached::Silent) => Ok(Ended::Silent),
                Ok(Detached::Behind) => Ok(Ended::Behind),
                // Unsent only when the registry itself went away.
                Err(_) => Ok(Ended::Left),
            },
            () = out.closed() => return Ok(Ended::Left),
            Some(command) = commands.recv() => {
                if out.send(Ok(command.into())).await.is_err() {
                    return Ok(Ended::Left);
                }
            }
            () = until(stop_deadline) => return failed(PhoneMicrophoneError::StopUnconfirmed),
            next = stream.next() => match next {
                Some(Ok(request)) => match Option::<PhoneRequest>::try_from(request) {
                    Ok(Some(request)) => {
                        if let Err(error) = phones.hear(generation, request) {
                            return failed(error);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => return Err(super::invalid_request(METHOD, &error)),
                },
                Some(Err(status)) if is_oversized(&status) => {
                    return Err(super::invalid_request(METHOD, &ProtocolError::AudioTooLarge));
                }
                Some(Err(_)) | None => return Ok(Ended::Left),
            },
        }
    }
}

/// Whether the transport refused a message over the LAN's size cap. Its own
/// text names the message's length, so it never goes further.
fn is_oversized(status: &Status) -> bool {
    status.code() == tonic::Code::OutOfRange
}

/// Never wakes without a deadline.
async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc::UnboundedSender;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    use crate::daemon::phone::{PhoneCommand, PhoneRecordingId};
    use crate::ipc::protocol::{MAX_PHONE_CHUNK_BYTES, rpc_failure};
    use aria_settings::rpc::RpcErrorCode;

    type Inbound = UnboundedReceiverStream<Result<wire::PhoneMicrophoneRequest, Status>>;

    fn request(body: wire::phone_microphone_request::Body) -> wire::PhoneMicrophoneRequest {
        wire::PhoneMicrophoneRequest { body: Some(body) }
    }

    fn offer() -> wire::PhoneMicrophoneRequest {
        request(wire::phone_microphone_request::Body::Offer(
            wire::PhoneMicrophoneOffer {},
        ))
    }

    fn audio(chunk: Vec<u8>) -> wire::PhoneMicrophoneRequest {
        request(wire::phone_microphone_request::Body::Audio(
            wire::PhoneMicrophoneAudio { chunk },
        ))
    }

    fn stopped(recording: PhoneRecordingId) -> wire::PhoneMicrophoneRequest {
        request(wire::phone_microphone_request::Body::Stopped(
            wire::PhoneMicrophoneStopped {
                recording: recording.into(),
            },
        ))
    }

    fn always(_preference: &Microphone) -> bool {
        true
    }

    fn never(_preference: &Microphone) -> bool {
        false
    }

    fn code(status: &Status) -> RpcErrorCode {
        rpc_failure(status).expect("an RpcError").code
    }

    fn command(sent: Option<Result<wire::PhoneMicrophoneCommand, Status>>) -> PhoneCommand {
        use wire::phone_microphone_command::Body;
        match sent.expect("a command").expect("not a failure").body {
            Some(Body::Start(start)) => PhoneCommand::Start(start.recording.into()),
            Some(Body::Stop(stop)) => PhoneCommand::Stop(stop.recording.into()),
            None => panic!("an empty command"),
        }
    }

    /// One phone's stream, served as the RPC serves it.
    struct PhoneStream {
        sends: UnboundedSender<Result<wire::PhoneMicrophoneRequest, Status>>,
        hears: mpsc::Receiver<Result<wire::PhoneMicrophoneCommand, Status>>,
        served: tokio::task::JoinHandle<Result<Ended, Status>>,
    }

    impl PhoneStream {
        fn serve(phones: &Arc<PhoneMicrophone>, admits: AdmitsPhone) -> Self {
            let (sends, inbound) = tokio::sync::mpsc::unbounded_channel();
            let inbound: Inbound = UnboundedReceiverStream::new(inbound);
            let (out, hears) = mpsc::channel(OUT_CAPACITY);
            let phones = Arc::clone(phones);
            let served = tokio::spawn(async move {
                let preference = watch::channel(Microphone::Automatic).1;
                let admitted = admit(phones, admits, &preference).await?;
                serve(admitted, inbound, &out, &CancellationToken::new()).await
            });
            Self {
                sends,
                hears,
                served,
            }
        }

        /// Served and admitted, so the capture can record on it.
        async fn attach(phones: &Arc<PhoneMicrophone>) -> Self {
            let stream = Self::serve(phones, always);
            while !phones.is_attached() {
                tokio::task::yield_now().await;
            }
            stream
        }

        fn send(&self, request: wire::PhoneMicrophoneRequest) {
            self.sends.send(Ok(request)).expect("the stream is open");
        }

        async fn outcome(self) -> Result<Ended, Status> {
            self.served.await.expect("serving doesn't panic")
        }
    }

    #[tokio::test]
    async fn a_stream_must_open_with_an_offer() {
        let mut opened = tokio_stream::iter(vec![Ok(stopped(PhoneRecordingId::from(1)))]);

        assert!(matches!(
            open(&mut opened).await,
            Err(PhoneMicrophoneError::NoOffer)
        ));
        assert!(
            open(&mut tokio_stream::iter(vec![Ok(offer())]))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_phone_a_press_wouldnt_use_is_refused_in_place_of_an_answer() {
        let phones = PhoneMicrophone::new();
        let offered = tokio_stream::iter(vec![Ok(offer())]);

        let answered = answer(
            Arc::clone(&phones),
            offered,
            never,
            &watch::channel(Microphone::Automatic).1,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(
            code(&answered.err().expect("refused")),
            RpcErrorCode::Unsupported
        );
        assert!(!phones.is_attached());
    }

    #[tokio::test]
    async fn a_dismissed_phone_is_told_a_press_wouldnt_use_it() {
        let phones = PhoneMicrophone::new();
        let stream = PhoneStream::attach(&phones).await;

        phones.dismiss();

        assert_eq!(
            code(&stream.outcome().await.expect_err("dismissed")),
            RpcErrorCode::Unsupported
        );
    }

    #[tokio::test]
    async fn each_quiet_ending_names_its_cause() {
        let phones = PhoneMicrophone::new();
        let silent = PhoneStream::attach(&phones).await;
        let recording = phones.start().expect("an idle phone records");
        phones.abandon(recording.generation);
        assert_eq!(silent.outcome().await.ok(), Some(Ended::Silent));

        let earlier = PhoneStream::attach(&phones).await;
        let _newer = PhoneStream::serve(&phones, always);
        assert_eq!(earlier.outcome().await.ok(), Some(Ended::Replaced));
    }

    #[tokio::test]
    async fn a_chunk_that_is_not_whole_samples_or_too_large_ends_the_stream() {
        for chunk in [vec![1, 0, 2], vec![0; MAX_PHONE_CHUNK_BYTES + 2]] {
            let phones = PhoneMicrophone::new();
            let mut stream = PhoneStream::attach(&phones).await;
            let _recording = phones.start().expect("an idle phone records");
            command(stream.hears.recv().await);

            let length = chunk.len().to_string();
            stream.send(audio(chunk));

            let status = stream.outcome().await.expect_err("the stream ends");
            assert_eq!(code(&status), RpcErrorCode::InvalidRequest);
            assert!(
                !status.message().contains(&length),
                "the chunk's length stays out of the message"
            );
        }
    }

    #[tokio::test]
    async fn a_message_over_the_transport_cap_ends_the_stream_as_invalid() {
        let phones = PhoneMicrophone::new();
        let stream = PhoneStream::attach(&phones).await;

        let refused = Status::out_of_range("found 70000 bytes");
        stream.sends.send(Err(refused)).expect("the stream is open");

        let status = stream.outcome().await.expect_err("the stream ends");
        assert_eq!(code(&status), RpcErrorCode::InvalidRequest);
        assert!(!status.message().contains("70000"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_phone_that_never_confirms_stop_is_dropped() {
        let phones = PhoneMicrophone::new();
        let mut stream = PhoneStream::attach(&phones).await;
        let recording = phones.start().expect("an idle phone records");
        command(stream.hears.recv().await);

        phones.stop(recording.id);
        assert_eq!(
            command(stream.hears.recv().await),
            PhoneCommand::Stop(recording.id)
        );

        assert_eq!(
            code(&stream.outcome().await.expect_err("dropped")),
            RpcErrorCode::InvalidRequest
        );
        assert!(!phones.is_attached());
    }
}
