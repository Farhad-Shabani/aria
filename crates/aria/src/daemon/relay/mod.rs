//! One recording relayed off a phone or watch on the LAN.
//!
//! The client records, then uploads: by the time the first byte reaches this
//! daemon the utterance is already over. So there is no voice-activity gate
//! and no silence stop here — the recorder on the wrist decided both. What is
//! left is the size cap and the timeouts.
//!
//! Upload limits prevent a stalled client from holding the recorder indefinitely.
//!
//! Everything below is driven by the caller's stream, so tests run it without
//! a server; only [`crate::daemon::service::RelayRpc`] knows about tonic.

mod advert;
mod code;
mod decode;
mod pairing;
mod preferences;

use std::time::Duration;

use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use tonic::Status;

use crate::ipc::wire;
use crate::rpc::{RpcErrorCode, RpcFailure};

pub(crate) use advert::SERVICE_TYPE;
pub(super) use advert::{Advert, instance_name};
pub(super) use code::{PairingCode, PairingCodes};
pub(crate) use pairing::RELATIVE_PATH as PAIRING_RELATIVE_PATH;
pub(super) use pairing::{
    Pairing, PairingError, Phones, fingerprint, fingerprint_from_hex,
    load_or_create as load_pairing, mint, presented, require_paired, to_hex,
};
pub(super) use preferences::LanPreferences;

/// The longest recording's PCM, so an oversized upload stops as it streams.
const MAX_AUDIO_BYTES: usize = decode::MAX_PCM_BYTES;
/// Maximum wait between upload messages; the client sends a completed recording.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Caps total upload time even when messages arrive within the idle timeout.
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Opening {
    format: wire::AudioFormat,
}

impl Opening {
    fn accept(start: wire::RelayStart) -> Result<Self, RelayError> {
        let format = match start.format() {
            wire::AudioFormat::Unspecified => return Err(RelayError::UnsupportedFormat),
            format => format,
        };
        Ok(Self { format })
    }
}

/// How one relay stream ended.
#[derive(Debug)]
pub(super) enum Collected {
    /// Audio to decode and hear.
    Audio(Vec<u8>),
    /// Nothing arrived before the idle timeout, or the stream closed empty.
    Silent,
}

/// Reads the opening message and validates it. A stream that opens with
/// anything else never gets as far as claiming the recorder.
pub(super) async fn open<S>(stream: &mut S) -> Result<Opening, RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    let first = tokio::time::timeout(IDLE_TIMEOUT, stream.next())
        .await
        .map_err(|_elapsed| RelayError::NotStarted)?;
    match first {
        Some(Ok(request)) => match request.body {
            Some(wire::relay_request::Body::Start(start)) => Opening::accept(start),
            _other => Err(RelayError::NotStarted),
        },
        Some(Err(status)) => Err(RelayError::ClientStream(status)),
        None => Err(RelayError::NotStarted),
    }
}

/// Reads the audio after the opening message, up to whatever ends the
/// stream: a `RelayStop`, a half-close, a gap longer than [`IDLE_TIMEOUT`],
/// the session cap, or the daemon stopping.
pub(super) async fn collect<S>(
    stream: &mut S,
    shutdown: &CancellationToken,
) -> Result<Collected, RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    // The daemon stopping outranks both clocks. Without this the session is
    // the one piece of daemon work nothing can interrupt, and a stop waits
    // out whichever timeout fires first.
    tokio::select! {
        biased;
        () = shutdown.cancelled() => Err(RelayError::ShuttingDown),
        collected = tokio::time::timeout(SESSION_TIMEOUT, accumulate(stream)) => {
            collected.map_err(|_elapsed| RelayError::SessionTimeout)?
        }
    }
}

async fn accumulate<S>(stream: &mut S) -> Result<Collected, RelayError>
where
    S: Stream<Item = Result<wire::RelayRequest, Status>> + Unpin,
{
    let mut audio: Vec<u8> = Vec::new();
    loop {
        // A gap this long ends the stream wherever it falls. A silent TCP
        // death — the phone out of range, no FIN — looks exactly like a
        // client thinking, and only the clock tells them apart.
        let next = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(next) => next,
            Err(_elapsed) if audio.is_empty() => return Ok(Collected::Silent),
            Err(_elapsed) => return Err(RelayError::UploadStalled),
        };

        match next {
            Some(Ok(request)) => match request.body {
                Some(wire::relay_request::Body::Audio(chunk)) => {
                    if audio.len() + chunk.chunk.len() > MAX_AUDIO_BYTES {
                        return Err(RelayError::TooLarge);
                    }
                    audio.extend_from_slice(&chunk.chunk);
                }
                Some(wire::relay_request::Body::Stop(_stop)) => break,
                Some(wire::relay_request::Body::Start(_start)) => {
                    return Err(RelayError::RestartedMidStream);
                }
                // An unknown body is a newer client's field. Skipping it
                // beats ending a recording over something additive.
                None => continue,
            },
            Some(Err(status)) => return Err(RelayError::ClientStream(status)),
            // A half-close ends the recording, the same as a `RelayStop`.
            None => break,
        }
    }

    Ok(if audio.is_empty() {
        Collected::Silent
    } else {
        Collected::Audio(audio)
    })
}

/// Turns collected bytes into the canonical PCM the engine reads.
pub(super) fn decode(opening: Opening, audio: &[u8]) -> Result<Vec<i16>, RelayError> {
    let samples = match opening.format {
        wire::AudioFormat::PcmS16le => decode::pcm16_from_bytes(audio),
        // `Opening::accept` refuses it, so nothing reaches here.
        wire::AudioFormat::Unspecified => return Err(RelayError::UnsupportedFormat),
    };
    samples.map_err(RelayError::Decode)
}

#[derive(Debug, thiserror::Error)]
pub(super) enum RelayError {
    #[error("a relay stream must open with RelayStart")]
    NotStarted,

    #[error("a second RelayStart arrived mid-stream")]
    RestartedMidStream,

    #[error("the audio format is missing or unsupported; the relay takes PCM_S16LE")]
    UnsupportedFormat,

    #[error("the stream sent more than {MAX_AUDIO_BYTES} bytes of audio")]
    TooLarge,

    #[error("the relay session ran past its cap of {SESSION_TIMEOUT:?}")]
    SessionTimeout,

    #[error("the upload went quiet for {IDLE_TIMEOUT:?} with the recording unfinished")]
    UploadStalled,

    #[error("the daemon is stopping and will not finish this recording")]
    ShuttingDown,

    #[error("a recording is already running on this Mac")]
    Busy,

    #[error(transparent)]
    Decode(#[from] decode::DecodeError),

    #[error("the relay client's stream failed: {0}")]
    ClientStream(#[source] Status),
}

impl RelayError {
    pub(super) fn failure(&self) -> RpcFailure {
        RpcFailure::new(self.code(), crate::primitives::error_chain(self))
    }

    fn code(&self) -> RpcErrorCode {
        use decode::DecodeError;
        match self {
            Self::NotStarted
            | Self::RestartedMidStream
            | Self::UnsupportedFormat
            | Self::Decode(DecodeError::Empty | DecodeError::OddPcmLength { .. }) => {
                RpcErrorCode::InvalidRequest
            }
            Self::TooLarge | Self::Decode(DecodeError::TooLong) => RpcErrorCode::PayloadTooLarge,
            Self::Busy => RpcErrorCode::Busy,
            Self::SessionTimeout | Self::UploadStalled => RpcErrorCode::OverallTimeout,
            Self::ShuttingDown => RpcErrorCode::ShuttingDown,
            Self::ClientStream(_) => RpcErrorCode::Cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio_stream::iter;

    use super::*;

    fn start(format: wire::AudioFormat) -> wire::RelayRequest {
        wire::RelayRequest {
            body: Some(wire::relay_request::Body::Start(wire::RelayStart {
                format: format as i32,
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

        let opening = open(&mut stream)
            .await
            .expect("the format defines its rate");

        assert_eq!(opening.format, wire::AudioFormat::PcmS16le);
    }

    #[tokio::test]
    async fn unspecified_and_unknown_formats_are_refused() {
        for format in [0, 2, i32::MAX] {
            let mut stream = requests(vec![wire::RelayRequest {
                body: Some(wire::relay_request::Body::Start(wire::RelayStart {
                    format,
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

        assert_eq!(error.failure().code, RpcErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn a_client_stop_ends_the_recording() {
        let mut stream = requests(vec![chunk(b"one"), chunk(b"two"), stop(), chunk(b"late")]);

        let collected = collect(&mut stream, &CancellationToken::new())
            .await
            .expect("the stop ends it cleanly");

        assert!(matches!(collected, Collected::Audio(audio) if audio == b"onetwo"));
    }

    #[tokio::test]
    async fn a_half_close_ends_the_recording_like_a_stop() {
        let mut stream = requests(vec![chunk(b"one")]);

        let collected = collect(&mut stream, &CancellationToken::new())
            .await
            .expect("a half-close ends it");

        assert!(matches!(collected, Collected::Audio(audio) if audio == b"one"));
    }

    #[tokio::test]
    async fn a_stream_that_sends_no_audio_reads_as_silence() {
        let mut stream = requests(vec![stop()]);

        let collected = collect(&mut stream, &CancellationToken::new())
            .await
            .expect("silence is not a failure");

        assert!(matches!(collected, Collected::Silent));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_says_nothing_gives_up_on_the_idle_timeout() {
        let mut stream = Box::pin(tokio_stream::pending());

        let collected = collect(&mut stream, &CancellationToken::new())
            .await
            .expect("an idle stream is silent");

        assert!(matches!(collected, Collected::Silent));
    }

    /// A phone that walks out of range sends no FIN, so the socket just goes
    /// quiet mid-upload. The recorder is claimed for the whole stream, so
    /// this is what keeps the Mac's own hotkey from being refused until the
    /// session cap runs out.
    #[tokio::test(start_paused = true)]
    async fn an_upload_that_goes_quiet_mid_stream_is_given_up_on() {
        let mut stream = Box::pin(iter(vec![Ok(chunk(b"audio"))]).chain(tokio_stream::pending()));

        let error = collect(&mut stream, &CancellationToken::new())
            .await
            .expect_err("a stalled upload does not hold the recorder");

        assert!(matches!(error, RelayError::UploadStalled));
    }

    /// A client that keeps its gaps just inside the idle timeout, forever.
    #[tokio::test(start_paused = true)]
    async fn a_stream_that_never_stops_is_cut_at_the_session_cap() {
        let mut stream = Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new({
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                while sender.send(Ok(chunk(b"audio"))).is_ok() {
                    tokio::time::sleep(IDLE_TIMEOUT / 2).await;
                }
            });
            receiver
        }));

        let error = collect(&mut stream, &CancellationToken::new())
            .await
            .expect_err("the cap ends it");

        assert!(matches!(error, RelayError::SessionTimeout));
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_stop_ends_the_stream_without_waiting_out_a_clock() {
        let mut stream = Box::pin(tokio_stream::pending());
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let error = collect(&mut stream, &shutdown)
            .await
            .expect_err("a stopping daemon takes no more audio");

        assert!(matches!(error, RelayError::ShuttingDown));
    }

    #[tokio::test]
    async fn audio_past_the_size_cap_is_refused() {
        let oversized = vec![0u8; MAX_AUDIO_BYTES + 1];
        let mut stream = requests(vec![chunk(&oversized)]);

        let error = collect(&mut stream, &CancellationToken::new())
            .await
            .expect_err("the cap holds");

        assert!(matches!(error, RelayError::TooLarge));
        assert_eq!(error.failure().code, RpcErrorCode::PayloadTooLarge);
    }

    #[test]
    fn audio_the_client_got_wrong_is_not_reported_as_a_daemon_fault() {
        let code = |error| RelayError::Decode(error).failure().code;

        assert_eq!(
            code(decode::DecodeError::TooLong),
            RpcErrorCode::PayloadTooLarge
        );
        assert_eq!(
            code(decode::DecodeError::Empty),
            RpcErrorCode::InvalidRequest
        );
        assert_eq!(
            code(decode::DecodeError::OddPcmLength { bytes: 3 }),
            RpcErrorCode::InvalidRequest
        );
    }

    #[tokio::test]
    async fn a_second_start_mid_stream_is_refused() {
        let mut stream = requests(vec![chunk(b"one"), start(wire::AudioFormat::PcmS16le)]);

        let error = collect(&mut stream, &CancellationToken::new())
            .await
            .expect_err("one start per stream");

        assert!(matches!(error, RelayError::RestartedMidStream));
    }

    #[tokio::test]
    async fn an_unknown_body_is_skipped_rather_than_ending_the_stream() {
        let newer_field = wire::RelayRequest { body: None };
        let mut stream = requests(vec![newer_field, chunk(b"one"), stop()]);

        let collected = collect(&mut stream, &CancellationToken::new())
            .await
            .expect("the unknown is skipped");

        assert!(matches!(collected, Collected::Audio(audio) if audio == b"one"));
    }
}
