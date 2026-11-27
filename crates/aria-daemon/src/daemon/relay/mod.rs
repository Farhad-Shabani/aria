//! One recording relayed off a phone or watch on the LAN.
//!
//! The client sends its audio as it records, into a press on the Mac. The
//! press owns the recording's clocks and its stop; what is left here is
//! validating the stream and the size cap.
//!
//! Everything below is driven by the caller's stream, so tests run it without
//! a server; only [`crate::daemon::service::RelayRpc`] knows about tonic.

mod advert;
mod code;
mod pairing;
mod settings;
mod stage;

use crate::ipc::protocol::{ProtocolError, failure_status, phone_samples};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use tonic::Status;

use crate::ipc::wire;
use aria_kit::audio::{AutoStop, canonical_samples};
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_voice::dictation::RecordingDevice;

pub use advert::SERVICE_TYPE;
pub(super) use advert::{Advert, instance_name};
pub(crate) use code::Invite;
pub(super) use code::{PairingCode, PairingCodes};
pub(crate) use pairing::PairedPhone;
pub(crate) use pairing::RELATIVE_PATH as PAIRING_RELATIVE_PATH;
pub(super) use pairing::{
    HardwareModel, Pairing, PairingError, Phones, fingerprint, fingerprint_from_hex,
    load_or_create as load_pairing, mint, presented, require_paired, to_hex,
};
pub(super) use settings::LanSettings;
pub(crate) use stage::{Offer, RelayStage};

/// Clients stop recording at 600 seconds. Recorder overshoot runs slightly
/// past it, so a full-length recording still fits.
const MAX_RECORDING: Duration = Duration::from_secs(601);
/// The longest recording's PCM, so an oversized recording stops as it
/// streams.
const MAX_AUDIO_BYTES: usize = canonical_samples(MAX_RECORDING) as usize * 2;
/// Maximum wait for the opening and the first audio, and the quiet limit
/// while auto-stop is off.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a recording may go without speech, or without audio arriving,
/// before the press ends it: the auto-stop time, so every device keeps one
/// rule.
pub(super) fn quiet_limit(auto_stop: Option<AutoStop>) -> Duration {
    auto_stop.map_or(IDLE_TIMEOUT, AutoStop::get)
}
/// How long the phone may take to ask for Return once the text landed.
const SUBMIT_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Opening {
    format: wire::AudioFormat,
    /// `None` from an older client.
    device: Option<RecordingDevice>,
}

impl Opening {
    fn accept(start: wire::RelayStart) -> Result<Self, RelayError> {
        let format = match start.format() {
            wire::AudioFormat::Unspecified => return Err(RelayError::UnsupportedFormat),
            format => format,
        };
        Ok(Self {
            format,
            device: crate::ipc::protocol::recording_device(start.device),
        })
    }

    pub(super) fn device(&self) -> Option<RecordingDevice> {
        self.device
    }
}

/// How a stream opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Opened {
    /// It starts a recording, which a later stream can continue when the
    /// client named it.
    Started(Opening, Option<RecordingId>),
    /// It continues the recording of this name, whose stream failed.
    Resumed(RecordingId),
}

/// The name a client gives its recording.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct RecordingId(Vec<u8>);

impl RecordingId {
    const MAX_BYTES: usize = 32;

    fn accept(id: Vec<u8>) -> Result<Self, RelayError> {
        if id.is_empty() || id.len() > Self::MAX_BYTES {
            return Err(RelayError::UnusableRecordingId);
        }
        Ok(Self(id))
    }
}

/// Whether the phone can still ask for Return once its audio is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReplyChannel {
    /// It ended with a `RelayStop`, so a `RelaySubmit` can follow.
    Open,
    /// It half-closed, so it sends nothing more.
    Closed,
}

/// How a recording's audio stopped arriving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PushEnd {
    /// The client ended it with a `RelayStop` or a half-close.
    ClientStopped(ReplyChannel),
    /// The press stopped reading, as the Mac stopped the recording.
    PressEnded,
}

/// What the phone said once the text landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reply {
    Submit,
    /// [`SUBMIT_WAIT`] passed without a word.
    Lapsed,
    /// The phone half-closed without asking.
    Closed,
}

/// Reads the opening message and validates it. A stream that opens with
/// anything else never gets as far as claiming the recorder.
pub(super) async fn open<S>(stream: &mut S) -> Result<Opened, RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    let first = tokio::time::timeout(IDLE_TIMEOUT, stream.next())
        .await
        .map_err(|_elapsed| RelayError::NotStarted)?;
    match first {
        Some(Ok(request)) => match request.body {
            Some(wire::relay_request::Body::Start(mut start)) => {
                let recording = start
                    .recording_id
                    .take()
                    .map(RecordingId::accept)
                    .transpose()?;
                Ok(Opened::Started(Opening::accept(start)?, recording))
            }
            Some(wire::relay_request::Body::Resume(resume)) => {
                RecordingId::accept(resume.recording_id).map(Opened::Resumed)
            }
            _other => Err(RelayError::NotStarted),
        },
        Some(Err(status)) => Err(RelayError::ClientStream(status)),
        None => Err(RelayError::NotStarted),
    }
}

/// Reads a recording up to its first audio. `None` when the stream
/// ended, or [`IDLE_TIMEOUT`] passed, before any arrived.
pub(super) async fn first_audio<S>(
    stream: &mut S,
    opening: Opening,
    shutdown: &CancellationToken,
) -> Result<Option<Vec<i16>>, RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    // One deadline for the whole wait, so messages without audio can't
    // hold the stream open.
    let deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
    loop {
        let next = tokio::select! {
            biased;
            () = shutdown.cancelled() => return Err(RelayError::ShuttingDown),
            next = tokio::time::timeout_at(deadline, stream.next()) => next,
        };
        let Ok(next) = next else {
            return Ok(None);
        };
        match next {
            Some(Ok(request)) => match request.body {
                Some(wire::relay_request::Body::Audio(chunk)) => {
                    return decode(opening, &chunk.chunk).map(Some);
                }
                Some(wire::relay_request::Body::Stop(_stop)) => return Ok(None),
                Some(
                    wire::relay_request::Body::Start(_) | wire::relay_request::Body::Resume(_),
                ) => {
                    return Err(RelayError::RestartedMidStream);
                }
                Some(wire::relay_request::Body::Submit(_submit)) => {
                    return Err(RelayError::SubmittedMidRecording);
                }
                Some(wire::relay_request::Body::Cancel(_cancel)) => {
                    return Err(RelayError::ClientCancelled);
                }
                None => {}
            },
            Some(Err(status)) => return Err(RelayError::ClientStream(status)),
            None => return Ok(None),
        }
    }
}

/// Forwards a recording's audio to its press until the recording
/// ends. `sent` counts the bytes already forwarded, toward the size cap.
/// Dropping `feed` on return ends the recording at the press. A client that
/// goes quiet is the press's to end: its capture stops once audio stalls.
pub(super) async fn forward<S>(
    stream: &mut S,
    opening: Opening,
    feed: SyncSender<Vec<i16>>,
    mut sent: usize,
) -> Result<PushEnd, RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(request)) => match request.body {
                Some(wire::relay_request::Body::Audio(chunk)) => {
                    sent += chunk.chunk.len();
                    if sent > MAX_AUDIO_BYTES {
                        return Err(RelayError::TooLarge);
                    }
                    let samples = decode(opening, &chunk.chunk)?;
                    if !feed_press(&feed, samples).await {
                        return Ok(PushEnd::PressEnded);
                    }
                }
                Some(wire::relay_request::Body::Stop(_stop)) => {
                    return Ok(PushEnd::ClientStopped(ReplyChannel::Open));
                }
                Some(
                    wire::relay_request::Body::Start(_) | wire::relay_request::Body::Resume(_),
                ) => {
                    return Err(RelayError::RestartedMidStream);
                }
                Some(wire::relay_request::Body::Submit(_submit)) => {
                    return Err(RelayError::SubmittedMidRecording);
                }
                Some(wire::relay_request::Body::Cancel(_cancel)) => {
                    return Err(RelayError::ClientCancelled);
                }
                None => {}
            },
            Some(Err(status)) => return Err(RelayError::ClientStream(status)),
            None => return Ok(PushEnd::ClientStopped(ReplyChannel::Closed)),
        }
    }
}

/// Hands `samples` to the press, waiting while its feed is full: a chunk
/// dropped there would leave a gap in the text, and Return could follow it.
/// Not reading meanwhile lets the stream's flow control slow the client.
/// False once the press stopped reading.
async fn feed_press(feed: &SyncSender<Vec<i16>>, samples: Vec<i16>) -> bool {
    let samples = match feed.try_send(samples) {
        Ok(()) => return true,
        Err(TrySendError::Disconnected(_)) => return false,
        Err(TrySendError::Full(samples)) => samples,
    };
    let feed = feed.clone();
    tokio::task::spawn_blocking(move || feed.send(samples).is_ok())
        .await
        .unwrap_or(false)
}

/// Waits for the phone's word after the text landed, up to
/// [`SUBMIT_WAIT`]. The daemon stopping ends the wait at once.
pub(super) async fn next_reply<S>(
    stream: &mut S,
    shutdown: &CancellationToken,
) -> Result<Reply, RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    tokio::select! {
        biased;
        () = shutdown.cancelled() => Err(RelayError::ShuttingDown),
        reply = tokio::time::timeout(SUBMIT_WAIT, read_reply(stream)) => {
            reply.unwrap_or(Ok(Reply::Lapsed))
        }
    }
}

async fn read_reply<S>(stream: &mut S) -> Result<Reply, RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    loop {
        let Some(request) = stream.next().await else {
            return Ok(Reply::Closed);
        };
        match request.map_err(RelayError::ClientStream)?.body {
            Some(wire::relay_request::Body::Submit(_submit)) => return Ok(Reply::Submit),
            Some(wire::relay_request::Body::Cancel(_cancel)) => {
                return Err(RelayError::ClientCancelled);
            }
            Some(
                wire::relay_request::Body::Start(_)
                | wire::relay_request::Body::Resume(_)
                | wire::relay_request::Body::Audio(_)
                | wire::relay_request::Body::Stop(_),
            ) => return Err(RelayError::RecordingEnded),
            // A newer client's field, skipped as during the recording.
            None => {}
        }
    }
}

/// What a client that stopped its recording says while the Mac transcribes
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterStop {
    /// It asked for Return ahead of COMPLETED.
    Submit,
    /// It half-closed, so it sends nothing more.
    Closed,
}

/// Reads a stopped recording's stream until the client says something, so
/// a `RelayCancel` reaches the press while it transcribes.
pub(super) async fn after_stop<S>(stream: &mut S) -> Result<AfterStop, RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    loop {
        let Some(request) = stream.next().await else {
            return Ok(AfterStop::Closed);
        };
        match request.map_err(RelayError::ClientStream)?.body {
            Some(wire::relay_request::Body::Submit(_submit)) => return Ok(AfterStop::Submit),
            Some(wire::relay_request::Body::Cancel(_cancel)) => {
                return Err(RelayError::ClientCancelled);
            }
            Some(
                wire::relay_request::Body::Start(_)
                | wire::relay_request::Body::Resume(_)
                | wire::relay_request::Body::Audio(_)
                | wire::relay_request::Body::Stop(_),
            ) => return Err(RelayError::RecordingEnded),
            None => {}
        }
    }
}

/// Turns a chunk's bytes into the canonical PCM the engine reads.
fn decode(opening: Opening, audio: &[u8]) -> Result<Vec<i16>, RelayError> {
    match opening.format {
        wire::AudioFormat::PcmS16le => phone_samples(audio).map_err(RelayError::Audio),
        // `Opening::accept` refuses it, so nothing reaches here.
        wire::AudioFormat::Unspecified => Err(RelayError::UnsupportedFormat),
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum RelayError {
    #[error("a relay stream must open with RelayStart or RelayResume")]
    NotStarted,

    #[error("a second RelayStart or RelayResume arrived mid-stream")]
    RestartedMidStream,

    #[error("a recording_id must be 1 to {} bytes", RecordingId::MAX_BYTES)]
    UnusableRecordingId,

    #[error("the recording_id names a recording still under way")]
    RecordingIdInUse,

    #[error("the recording to continue is over")]
    RecordingOver,

    #[error("a RelaySubmit arrived before the recording ended")]
    SubmittedMidRecording,

    #[error("the recording already ended with RelayStop")]
    RecordingEnded,

    #[error("the audio format is missing or unsupported; the relay takes PCM_S16LE")]
    UnsupportedFormat,

    #[error("the stream sent more than {MAX_AUDIO_BYTES} bytes of audio")]
    TooLarge,

    #[error("the daemon is stopping and will not finish this recording")]
    ShuttingDown,

    #[error(transparent)]
    Audio(ProtocolError),

    #[error("the relay client cancelled the recording")]
    ClientCancelled,

    #[error("the relay client's stream failed: {0}")]
    ClientStream(#[source] Status),
}

impl RelayError {
    /// The client's stream broke, as a lost connection does, rather than
    /// failing to decode.
    pub(super) fn is_lost_stream(&self) -> bool {
        matches!(self, Self::ClientStream(status) if is_lost(status))
    }
}

/// Whether a stream failed as a lost connection fails it: its keepalive
/// timed out (`Unavailable`), or its connection broke (`Unknown`). A
/// cancelled RPC reads as the stream ending. Anything else, such as an
/// oversized or undecodable message, is the client's fault.
pub(super) fn is_lost(status: &Status) -> bool {
    matches!(
        status.code(),
        tonic::Code::Unavailable | tonic::Code::Unknown
    )
}

impl From<&RelayError> for RpcErrorCode {
    fn from(error: &RelayError) -> Self {
        match error {
            RelayError::NotStarted
            | RelayError::RestartedMidStream
            | RelayError::UnusableRecordingId
            | RelayError::RecordingIdInUse
            | RelayError::SubmittedMidRecording
            | RelayError::RecordingEnded
            | RelayError::UnsupportedFormat
            | RelayError::Audio(_) => Self::InvalidRequest,
            RelayError::TooLarge => Self::PayloadTooLarge,
            RelayError::ShuttingDown => Self::ShuttingDown,
            RelayError::RecordingOver
            | RelayError::ClientCancelled
            | RelayError::ClientStream(_) => Self::Cancelled,
        }
    }
}

/// What a phone learns of a settings fault: that the Mac needs it fixed.
const SETTINGS_NEED_FIXING: &str = "Settings need fixing on your Mac";

/// A failure as it may cross the network. Messages can name paths and quote
/// files, so a code keeps its own only when every message under it is known
/// to be fixed text or the phone's own input. Every code is named, so a new
/// one can't cross until someone has checked its messages.
pub(in crate::daemon) fn redacted(status: Status) -> Status {
    let Some(failure) = crate::ipc::protocol::rpc_failure(&status) else {
        return failure_status(RpcFailure::new(
            RpcErrorCode::Internal,
            SOMETHING_WENT_WRONG,
        ));
    };
    let message = match failure.code {
        RpcErrorCode::InvalidRequest
        | RpcErrorCode::Unauthenticated
        | RpcErrorCode::PayloadTooLarge
        | RpcErrorCode::ApplicationExcluded => failure.message,
        RpcErrorCode::SettingsInvalid => SETTINGS_NEED_FIXING.to_owned(),
        RpcErrorCode::Busy => "Your Mac is busy".to_owned(),
        RpcErrorCode::PermissionDenied => "Your Mac refused this change".to_owned(),
        RpcErrorCode::Internal
        | RpcErrorCode::Cancelled
        | RpcErrorCode::Unsupported
        | RpcErrorCode::ShuttingDown
        | RpcErrorCode::DurabilityUnconfirmed
        | RpcErrorCode::OverallTimeout
        | RpcErrorCode::RecordingTimeout
        | RpcErrorCode::DeliveryTimeout
        | RpcErrorCode::CaptureFailed
        | RpcErrorCode::NoSpeech
        | RpcErrorCode::TranscriptionFailed
        | RpcErrorCode::DeliveryFailed
        | RpcErrorCode::DeliveryTargetChanged
        | RpcErrorCode::DeliveryInterrupted
        | RpcErrorCode::SynthesisFailed
        | RpcErrorCode::PlaybackFailed
        | RpcErrorCode::SpeechTimeout
        | RpcErrorCode::VoiceUnavailable
        | RpcErrorCode::AssistantUnavailable
        | RpcErrorCode::AssistantFailed
        | RpcErrorCode::AssistantTimeout
        | RpcErrorCode::AssistantSignedOut
        | RpcErrorCode::PromptUnreadable
        | RpcErrorCode::PromptMissing
        | RpcErrorCode::PromptsFull
        | RpcErrorCode::PromptWriteFailed
        | RpcErrorCode::ChatNameTaken
        | RpcErrorCode::SpeakerModelMissing
        | RpcErrorCode::SpeakerAlreadyEnrolled
        | RpcErrorCode::SpeakerNotEnrolled
        | RpcErrorCode::SpeakerInconsistentSamples
        | RpcErrorCode::SpeakerProfileWriteFailed
        | RpcErrorCode::SpeakerNoSpeech
        | RpcErrorCode::SystemTranscriptionUnsupportedOs
        | RpcErrorCode::SystemTranscriptionPermissionDenied
        | RpcErrorCode::SystemTranscriptionCaptureFailed
        | RpcErrorCode::UnsavedTranscriptMissing
        | RpcErrorCode::HistoryMissing
        | RpcErrorCode::HistoryAudioUnavailable
        | RpcErrorCode::HistoryUnavailable
        | RpcErrorCode::HistoryProjectUnavailable
        | RpcErrorCode::HistoryEntryEmpty
        | RpcErrorCode::HistoryTranscriptUnreadable
        | RpcErrorCode::HistoryFull
        | RpcErrorCode::Unknown(_) => SOMETHING_WENT_WRONG.to_owned(),
    };
    failure_status(RpcFailure::new(failure.code, message))
}

const SOMETHING_WENT_WRONG: &str = "Something went wrong on your Mac";

#[cfg(test)]
mod tests {
    use tokio_stream::iter;

    use super::*;

    fn start(format: wire::AudioFormat) -> wire::RelayRequest {
        wire::RelayRequest {
            body: Some(wire::relay_request::Body::Start(wire::RelayStart {
                format: format as i32,
                ..Default::default()
            })),
        }
    }

    fn chunk(bytes: &[u8]) -> wire::RelayRequest {
        wire::RelayRequest {
            body: Some(wire::relay_request::Body::Audio(wire::RelayAudio {
                chunk: bytes.to_vec(),
            })),
        }
    }

    fn stop() -> wire::RelayRequest {
        wire::RelayRequest {
            body: Some(wire::relay_request::Body::Stop(wire::RelayStop {})),
        }
    }

    fn requests(
        messages: Vec<wire::RelayRequest>,
    ) -> impl Stream<Item = Result<wire::RelayRequest, Status>> + Unpin {
        iter(messages.into_iter().map(Ok))
    }

    /// The recording a stream starts, and the name it gave it.
    async fn started<S>(stream: &mut S) -> (Opening, Option<RecordingId>)
    where
        S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
    {
        match open(stream).await.expect("a valid opening") {
            Opened::Started(opening, recording) => (opening, recording),
            Opened::Resumed(recording) => panic!("{recording:?} starts no recording"),
        }
    }

    async fn pcm_opening() -> Opening {
        started(&mut requests(vec![start(wire::AudioFormat::PcmS16le)]))
            .await
            .0
    }

    #[tokio::test]
    async fn audio_reaches_the_press_as_it_arrives() {
        let opening = pcm_opening().await;
        let mut stream = requests(vec![chunk(&[1, 0]), chunk(&[2, 0, 3, 0]), stop()]);
        let shutdown = CancellationToken::new();

        let first = first_audio(&mut stream, opening, &shutdown).await.unwrap();
        assert_eq!(first, Some(vec![1]));
        let (feed, press) = std::sync::mpsc::sync_channel(8);
        let ended = forward(&mut stream, opening, feed, 2).await.unwrap();

        assert_eq!(ended, PushEnd::ClientStopped(ReplyChannel::Open));
        assert_eq!(press.try_iter().collect::<Vec<_>>(), [vec![2, 3]]);
    }

    #[tokio::test]
    async fn a_recording_ends_once_the_press_stops_reading() {
        let opening = pcm_opening().await;
        let mut stream = requests(vec![chunk(&[2, 0]), chunk(&[3, 0])]);
        let (feed, press) = std::sync::mpsc::sync_channel(8);
        drop(press);

        let ended = forward(&mut stream, opening, feed, 0).await.unwrap();

        assert_eq!(ended, PushEnd::PressEnded);
    }

    #[tokio::test]
    async fn a_half_closed_recording_takes_no_return() {
        let opening = pcm_opening().await;
        let (feed, _press) = std::sync::mpsc::sync_channel(8);

        let ended = forward(&mut requests(vec![chunk(&[2, 0])]), opening, feed, 0)
            .await
            .unwrap();

        assert_eq!(ended, PushEnd::ClientStopped(ReplyChannel::Closed));
    }

    #[tokio::test]
    async fn audio_past_the_cap_is_refused() {
        let opening = pcm_opening().await;
        let (feed, _press) = std::sync::mpsc::sync_channel(8);

        let error = forward(
            &mut requests(vec![chunk(&[2, 0])]),
            opening,
            feed,
            MAX_AUDIO_BYTES,
        )
        .await
        .expect_err("the cap counts audio already sent");

        assert!(matches!(error, RelayError::TooLarge));
    }

    #[tokio::test(start_paused = true)]
    async fn messages_without_audio_cannot_hold_a_recording_open() {
        let opening = pcm_opening().await;
        let empty = || Ok(wire::RelayRequest { body: None });
        let mut stream = Box::pin(
            iter(std::iter::repeat_with(empty)).throttle(IDLE_TIMEOUT - Duration::from_secs(1)),
        );
        let shutdown = CancellationToken::new();
        let opened = tokio::time::Instant::now();

        let first = first_audio(&mut stream, opening, &shutdown).await.unwrap();

        assert_eq!(first, None);
        assert_eq!(opened.elapsed(), IDLE_TIMEOUT);
    }

    #[tokio::test]
    async fn a_recording_stopped_before_audio_heard_nothing() {
        let opening = pcm_opening().await;
        let shutdown = CancellationToken::new();

        let first = first_audio(&mut requests(vec![stop()]), opening, &shutdown)
            .await
            .unwrap();

        assert_eq!(first, None);
    }

    #[tokio::test]
    async fn a_stream_that_does_not_open_with_start_is_refused() {
        let mut stream = requests(vec![chunk(b"audio")]);

        let error = open(&mut stream)
            .await
            .expect_err("audio is not an opening");

        assert!(matches!(error, RelayError::NotStarted));
    }

    #[tokio::test]
    async fn pcm_opens_without_rate_metadata() {
        let mut stream = requests(vec![start(wire::AudioFormat::PcmS16le)]);

        let (opening, _recording) = started(&mut stream).await;

        assert_eq!(opening.format, wire::AudioFormat::PcmS16le);
    }

    #[tokio::test]
    async fn an_opening_keeps_its_device() {
        let opening = |device: wire::RecordingDevice| {
            requests(vec![wire::RelayRequest {
                body: Some(wire::relay_request::Body::Start(wire::RelayStart {
                    format: wire::AudioFormat::PcmS16le as i32,
                    device: device as i32,
                    recording_id: None,
                })),
            }])
        };

        let (watch, _) = started(&mut opening(wire::RecordingDevice::Watch)).await;
        let (older, _) = started(&mut opening(wire::RecordingDevice::Unspecified)).await;

        assert_eq!(watch.device(), Some(RecordingDevice::Watch));
        assert_eq!(older.device(), None);
    }

    fn named(recording_id: &[u8]) -> wire::RelayRequest {
        wire::RelayRequest {
            body: Some(wire::relay_request::Body::Start(wire::RelayStart {
                format: wire::AudioFormat::PcmS16le as i32,
                recording_id: Some(recording_id.to_vec()),
                ..Default::default()
            })),
        }
    }

    fn resume(recording_id: &[u8]) -> wire::RelayRequest {
        wire::RelayRequest {
            body: Some(wire::relay_request::Body::Resume(wire::RelayResume {
                recording_id: recording_id.to_vec(),
            })),
        }
    }

    #[tokio::test]
    async fn a_stream_continues_the_recording_its_resume_names() {
        let (_, recording) = started(&mut requests(vec![named(b"walk")])).await;
        let (_, unnamed) = started(&mut requests(vec![start(wire::AudioFormat::PcmS16le)])).await;

        let resumed = open(&mut requests(vec![resume(b"walk")]))
            .await
            .expect("a resume opens a stream");

        assert_eq!(unnamed, None);
        assert_eq!(
            resumed,
            Opened::Resumed(recording.expect("the recording named itself"))
        );
    }

    #[tokio::test]
    async fn a_recording_id_that_is_empty_or_too_long_is_refused() {
        for unusable in [vec![], vec![7; RecordingId::MAX_BYTES + 1]] {
            let starting = open(&mut requests(vec![named(&unusable)])).await;
            let resuming = open(&mut requests(vec![resume(&unusable)])).await;

            assert!(matches!(starting, Err(RelayError::UnusableRecordingId)));
            assert!(matches!(resuming, Err(RelayError::UnusableRecordingId)));
        }
    }

    #[tokio::test]
    async fn a_resume_after_the_opening_is_refused() {
        let opening = pcm_opening().await;
        let (feed, _press) = std::sync::mpsc::sync_channel(8);
        let shutdown = CancellationToken::new();

        let before_audio =
            first_audio(&mut requests(vec![resume(b"walk")]), opening, &shutdown).await;
        let mid_recording = forward(&mut requests(vec![resume(b"walk")]), opening, feed, 0).await;

        assert!(matches!(before_audio, Err(RelayError::RestartedMidStream)));
        assert!(matches!(mid_recording, Err(RelayError::RestartedMidStream)));
    }

    #[test]
    fn a_recording_goes_quiet_for_as_long_as_its_auto_stop() {
        let auto_stop = AutoStop::new(Duration::from_secs(4)).expect("in range");

        assert_eq!(quiet_limit(Some(auto_stop)), Duration::from_secs(4));
        assert_eq!(quiet_limit(None), IDLE_TIMEOUT);
    }

    #[tokio::test]
    async fn unspecified_and_unknown_formats_are_refused() {
        for format in [0, 2, i32::MAX] {
            let mut stream = requests(vec![wire::RelayRequest {
                body: Some(wire::relay_request::Body::Start(wire::RelayStart {
                    format,
                    ..Default::default()
                })),
            }]);

            let error = open(&mut stream)
                .await
                .expect_err("a known format is required");

            assert!(matches!(error, RelayError::UnsupportedFormat));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_opening_that_never_arrives_is_refused() {
        let mut stream = Box::pin(tokio_stream::pending());

        let error = open(&mut stream)
            .await
            .expect_err("Start has an idle deadline");

        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::InvalidRequest);
    }

    fn submit() -> wire::RelayRequest {
        wire::RelayRequest {
            body: Some(wire::relay_request::Body::Submit(wire::RelaySubmit {})),
        }
    }

    #[tokio::test]
    async fn a_stop_leaves_the_stream_open_for_a_submit() {
        let opening = pcm_opening().await;
        let mut stream = requests(vec![chunk(&[1, 0]), stop(), submit()]);
        first_audio(&mut stream, opening, &CancellationToken::new())
            .await
            .expect("the first audio");
        let (feed, _press) = std::sync::mpsc::sync_channel(8);
        forward(&mut stream, opening, feed, 2)
            .await
            .expect("the stop ends the recording");

        let reply = next_reply(&mut stream, &CancellationToken::new()).await;

        assert_eq!(reply.expect("a submit is a reply"), Reply::Submit);
    }

    #[tokio::test]
    async fn a_closed_stream_asks_for_nothing() {
        let mut stream = requests(vec![]);

        let reply = next_reply(&mut stream, &CancellationToken::new()).await;

        assert_eq!(reply.expect("a half-close is a reply"), Reply::Closed);
    }

    #[tokio::test]
    async fn a_submit_during_the_recording_is_refused() {
        let opening = pcm_opening().await;
        let shutdown = CancellationToken::new();
        let before_audio = first_audio(&mut requests(vec![submit()]), opening, &shutdown).await;
        let (feed, _press) = std::sync::mpsc::sync_channel(8);
        let mid_recording = forward(&mut requests(vec![submit(), stop()]), opening, feed, 0).await;

        for error in [before_audio.unwrap_err(), mid_recording.unwrap_err()] {
            assert!(matches!(error, RelayError::SubmittedMidRecording));
            assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::InvalidRequest);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_submit_wait_lapses_on_a_quiet_phone() {
        let mut stream = Box::pin(tokio_stream::pending());
        let started = tokio::time::Instant::now();

        let reply = next_reply(&mut stream, &CancellationToken::new()).await;

        assert_eq!(reply.expect("a lapse is a reply"), Reply::Lapsed);
        assert_eq!(started.elapsed(), SUBMIT_WAIT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_stop_ends_the_submit_wait_at_once() {
        let mut stream = Box::pin(tokio_stream::pending());
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let started = tokio::time::Instant::now();

        let error = next_reply(&mut stream, &shutdown)
            .await
            .expect_err("a stopping daemon presses nothing");

        assert!(matches!(error, RelayError::ShuttingDown));
        assert!(started.elapsed().is_zero());
    }

    #[tokio::test]
    async fn a_recording_message_after_the_stop_is_refused() {
        for late in [chunk(b"late"), stop(), start(wire::AudioFormat::PcmS16le)] {
            let mut stream = requests(vec![late, submit()]);

            let error = next_reply(&mut stream, &CancellationToken::new())
                .await
                .expect_err("the recording already ended");

            assert!(matches!(error, RelayError::RecordingEnded));
            assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::InvalidRequest);
        }
    }

    #[tokio::test]
    async fn an_unknown_body_after_the_stop_is_skipped() {
        let newer_field = wire::RelayRequest { body: None };
        let mut stream = requests(vec![newer_field, submit()]);

        let reply = next_reply(&mut stream, &CancellationToken::new()).await;

        assert_eq!(reply.expect("the unknown is skipped"), Reply::Submit);
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_stop_ends_the_stream_without_waiting_out_a_clock() {
        let opening = pcm_opening().await;
        let mut stream = Box::pin(tokio_stream::pending());
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let error = first_audio(&mut stream, opening, &shutdown)
            .await
            .expect_err("a stopping daemon takes no more audio");

        assert!(matches!(error, RelayError::ShuttingDown));
    }

    #[tokio::test]
    async fn audio_the_client_got_wrong_is_refused_as_its_fault() {
        let opening = pcm_opening().await;
        let shutdown = CancellationToken::new();
        let oversized = vec![0; crate::ipc::protocol::MAX_PHONE_CHUNK_BYTES + 2];
        for wrong in [chunk(&[]), chunk(&[1, 0, 2]), chunk(&oversized)] {
            let error = first_audio(&mut requests(vec![wrong]), opening, &shutdown)
                .await
                .expect_err("the chunk is refused");

            assert!(matches!(error, RelayError::Audio(_)));
            assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::InvalidRequest);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_press_that_falls_behind_still_gets_every_chunk() {
        let opening = pcm_opening().await;
        let (feed, press) = std::sync::mpsc::sync_channel(1);
        let mut stream = requests(vec![chunk(&[1, 0]), chunk(&[2, 0]), chunk(&[3, 0]), stop()]);
        let reading = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(50));
            press.iter().collect::<Vec<_>>()
        });

        let ended = forward(&mut stream, opening, feed, 0).await.unwrap();

        assert_eq!(ended, PushEnd::ClientStopped(ReplyChannel::Open));
        assert_eq!(reading.await.unwrap(), [vec![1], vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn a_second_start_mid_stream_is_refused() {
        let opening = pcm_opening().await;
        let mut stream = requests(vec![chunk(&[1, 0]), start(wire::AudioFormat::PcmS16le)]);
        let (feed, _press) = std::sync::mpsc::sync_channel(8);

        let error = forward(&mut stream, opening, feed, 0)
            .await
            .expect_err("one start per stream");

        assert!(matches!(error, RelayError::RestartedMidStream));
    }

    #[tokio::test]
    async fn an_unknown_body_is_skipped_rather_than_ending_the_stream() {
        let opening = pcm_opening().await;
        let newer_field = wire::RelayRequest { body: None };
        let mut stream = requests(vec![newer_field, chunk(&[1, 0]), stop()]);
        let (feed, press) = std::sync::mpsc::sync_channel(8);

        let ended = forward(&mut stream, opening, feed, 0)
            .await
            .expect("the unknown is skipped");

        assert_eq!(ended, PushEnd::ClientStopped(ReplyChannel::Open));
        assert_eq!(press.try_iter().collect::<Vec<_>>(), [vec![1]]);
    }

    #[test]
    fn only_a_code_known_to_be_safe_keeps_its_message() {
        let path = "/Users/you/.aria/settings.lock";
        for code in [
            RpcErrorCode::Busy,
            RpcErrorCode::Internal,
            RpcErrorCode::PermissionDenied,
            RpcErrorCode::SettingsInvalid,
            RpcErrorCode::TranscriptionFailed,
            RpcErrorCode::Unknown(999),
        ] {
            let status = redacted(failure_status(RpcFailure::new(code, path)));
            assert!(!status.message().contains(path), "{code:?}");
        }
        let kept = redacted(failure_status(RpcFailure::new(
            RpcErrorCode::InvalidRequest,
            "memory.enabled is refused",
        )));
        assert_eq!(kept.message(), "memory.enabled is refused");
        let bare = redacted(Status::internal(path));
        assert!(!bare.message().contains(path));
    }
}
