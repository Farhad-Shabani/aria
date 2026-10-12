//! The newline-JSON wire the daemon and the sidecar agree on, and the
//! lifecycle of one child speaking it.

use std::path::Path;

use serde::Deserialize;

use aria_kit::backend::BackendError;
use aria_kit::cancel::CancelToken;
use aria_kit::primitives::process::{Script, Sidecar, SidecarError};
use aria_kit::transcription::TranscriptionError;

pub(super) const BACKEND: &str = super::NAME;

const SIDECAR_SCRIPT: &str = include_str!("parakeet_mlx.py");
const PROTOCOL_VERSION: u32 = 1;

/// Cache and network settings the model download needs. Everything else about
/// the daemon's environment stays out of the child.
const INHERITED_VARIABLES: &[&str] = &[
    "HF_HOME",
    "HF_TOKEN",
    "HF_HUB_CACHE",
    "HUGGINGFACE_HUB_CACHE",
    "XDG_CACHE_HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
];

pub(super) fn start(
    interpreter: &Path,
    model: &str,
    cancel: &CancelToken,
) -> Result<Sidecar, TranscriptionError> {
    let mut sidecar = Sidecar::start(&Script {
        name: BACKEND,
        interpreter,
        source: SIDECAR_SCRIPT,
        argument: model,
        inherited_variables: INHERITED_VARIABLES,
    })
    .map_err(|error| Phase::Load { model }.transport(error))?;
    handshake(&mut sidecar, model, cancel)?;
    Ok(sidecar)
}

/// Waits for the model to load. A first run downloads it, so the caller's
/// cancellation is the only bound that makes sense here.
fn handshake(
    sidecar: &mut Sidecar,
    model: &str,
    cancel: &CancelToken,
) -> Result<(), TranscriptionError> {
    let phase = Phase::Load { model };
    match sidecar
        .receive(&|| cancel.is_cancelled())
        .map_err(|error| phase.transport(error))?
    {
        Reply::Ready { protocol, .. } if protocol == PROTOCOL_VERSION => {
            tracing::info!(backend = BACKEND, model, "transcription sidecar ready");
            Ok(())
        }
        Reply::Ready { protocol, .. } => Err(phase.unusable(format!(
            "speaks protocol {protocol}; this daemon speaks {PROTOCOL_VERSION}"
        ))),
        Reply::Failed { error, .. } => Err(phase.unusable(error)),
        Reply::Transcript { .. } => {
            Err(phase.unusable("sent a transcript before reporting readiness"))
        }
    }
}

pub(super) fn transcribe(
    sidecar: &mut Sidecar,
    audio_path: &str,
    chunk_samples: Option<usize>,
    cancel: &CancelToken,
) -> Result<String, TranscriptionError> {
    let phase = Phase::Inference;
    let request = Request {
        op: "transcribe",
        audio_path,
        chunk_samples,
    };
    sidecar
        .send(&request)
        .map_err(|error| phase.transport(error))?;

    match sidecar
        .receive(&|| cancel.is_cancelled())
        .map_err(|error| phase.transport(error))?
    {
        Reply::Transcript { text, .. } => Ok(text),
        Reply::Failed { error, .. } => Err(phase.unusable(error)),
        Reply::Ready { .. } => Err(phase.unusable("reported readiness twice")),
    }
}

/// Which half of the backend's life a failure belongs to. Bringing the child
/// up and asking it a question fail the same ways, but the pipeline treats a
/// backend that cannot load differently from a press that did not transcribe.
pub(super) enum Phase<'a> {
    Load { model: &'a str },
    Inference,
}

impl Phase<'_> {
    fn failed(&self, source: BackendError) -> TranscriptionError {
        match *self {
            Self::Load { model } => TranscriptionError::Load {
                backend: BACKEND,
                model: model.to_owned(),
                source,
            },
            Self::Inference => TranscriptionError::Inference {
                backend: BACKEND,
                source,
            },
        }
    }

    /// The transport gave out. Cancellation is its own outcome, never a
    /// failure of the child.
    pub(super) fn transport(&self, error: SidecarError) -> TranscriptionError {
        match error {
            SidecarError::Cancelled => TranscriptionError::Cancelled,
            transport @ (SidecarError::Interpreter(_)
            | SidecarError::Home(_)
            | SidecarError::Spawn(_)
            | SidecarError::Pipes
            | SidecarError::Encode(_)
            | SidecarError::Write(_)
            | SidecarError::Read(_)
            | SidecarError::Decode(_)
            | SidecarError::Closed) => self.failed(BackendError::engine(transport)),
        }
    }

    /// The child answered on its own terms, and the answer is no good.
    fn unusable(&self, detail: impl Into<String>) -> TranscriptionError {
        self.failed(BackendError::Unusable(detail.into()))
    }
}

/// A sidecar that answered is still serving. One that was killed, lost its
/// pipes, or has since exited is not, and the next press starts a new one.
pub(super) fn still_serving(
    sidecar: &mut Sidecar,
    outcome: &Result<String, TranscriptionError>,
) -> bool {
    match outcome {
        Ok(_)
        | Err(TranscriptionError::Processing(_))
        | Err(TranscriptionError::Inference {
            source: BackendError::Unusable(_),
            ..
        }) => sidecar.is_running(),
        Err(
            TranscriptionError::Model { .. }
            | TranscriptionError::Load { .. }
            | TranscriptionError::Capture(_)
            | TranscriptionError::Inference { .. }
            | TranscriptionError::UnsupportedLanguage { .. }
            | TranscriptionError::UnsupportedAudioInput { .. }
            | TranscriptionError::UnsupportedAudioPath { .. }
            | TranscriptionError::UnsupportedBackend { .. }
            | TranscriptionError::UnsupportedDevice { .. }
            | TranscriptionError::NoNativeStream
            | TranscriptionError::LoadPanicked
            | TranscriptionError::Cancelled
            | TranscriptionError::NoSpeech,
        ) => false,
    }
}

#[derive(serde::Serialize)]
struct Request<'a> {
    op: &'a str,
    audio_path: &'a str,
    /// Feed the recording to the model's streaming path in chunks of this
    /// many samples; absent means one whole-recording decode.
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk_samples: Option<usize>,
}

/// What the sidecar can say. `ok` discriminates, so a reply cannot claim both
/// a transcript and a failure.
#[derive(Deserialize)]
#[serde(untagged)]
enum Reply {
    Ready {
        #[serde(rename = "ok")]
        _ok: OkTrue,
        protocol: u32,
    },
    Transcript {
        #[serde(rename = "ok")]
        _ok: OkTrue,
        text: String,
    },
    Failed {
        #[serde(rename = "ok")]
        _ok: OkFalse,
        error: String,
    },
}

#[derive(Deserialize)]
#[serde(try_from = "bool")]
struct OkTrue;

impl TryFrom<bool> for OkTrue {
    type Error = &'static str;

    fn try_from(ok: bool) -> Result<Self, Self::Error> {
        ok.then_some(Self).ok_or("expected ok: true")
    }
}

#[derive(Deserialize)]
#[serde(try_from = "bool")]
struct OkFalse;

impl TryFrom<bool> for OkFalse {
    type Error = &'static str;

    fn try_from(ok: bool) -> Result<Self, Self::Error> {
        (!ok).then_some(Self).ok_or("expected ok: false")
    }
}

/// Shells speaking the protocol, so both this module and the engine can be
/// checked without an interpreter or a model.
#[cfg(test)]
pub(super) mod stubs {
    use super::*;

    pub(super) const READY: &str = r#"echo '{"ok":true,"ready":true,"protocol":1}'"#;

    pub(in crate::parakeet_mlx) fn shell(source: &str) -> Sidecar {
        Sidecar::start(&Script {
            name: "stub",
            interpreter: Path::new("/bin/sh"),
            source,
            argument: "unused",
            inherited_variables: &[],
        })
        .expect("the stub should start")
    }

    pub(in crate::parakeet_mlx) fn stub(script: &str) -> Sidecar {
        let mut sidecar = shell(&format!("{READY}; {script}"));
        handshake(&mut sidecar, "stub", &CancelToken::never())
            .expect("the stub should report readiness");
        sidecar
    }
}

#[cfg(test)]
mod tests {
    use super::stubs::{READY, shell, stub};
    use super::*;

    #[test]
    fn a_request_carries_the_operation_and_the_audio_path() {
        let line = serde_json::to_string(&Request {
            op: "transcribe",
            audio_path: "/tmp/recording.wav",
            chunk_samples: None,
        })
        .expect("a request should encode");

        assert_eq!(
            line,
            r#"{"op":"transcribe","audio_path":"/tmp/recording.wav"}"#
        );
    }

    #[test]
    fn a_streaming_request_names_its_chunk() {
        let line = serde_json::to_string(&Request {
            op: "transcribe",
            audio_path: "/tmp/recording.wav",
            chunk_samples: Some(12000),
        })
        .expect("a request should encode");

        assert_eq!(
            line,
            r#"{"op":"transcribe","audio_path":"/tmp/recording.wav","chunk_samples":12000}"#
        );
    }

    #[test]
    fn a_reply_is_read_as_exactly_one_kind() {
        assert!(matches!(
            serde_json::from_str(r#"{"ok":true,"ready":true,"protocol":1}"#),
            Ok(Reply::Ready { protocol: 1, .. })
        ));
        assert!(matches!(
            serde_json::from_str(r#"{"ok":true,"text":"hello there"}"#),
            Ok(Reply::Transcript { text, .. }) if text == "hello there"
        ));
        assert!(matches!(
            serde_json::from_str(r#"{"ok":false,"error":"boom"}"#),
            Ok(Reply::Failed { error, .. }) if error == "boom"
        ));
    }

    #[test]
    fn a_reply_that_contradicts_itself_is_refused() {
        for line in [
            r#"{"ok":true,"error":"boom"}"#,
            r#"{"ok":false,"text":"hello"}"#,
            r#"{"text":"no ok field"}"#,
            "not json at all",
        ] {
            assert!(
                serde_json::from_str::<Reply>(line).is_err(),
                "{line} must not decode"
            );
        }
    }

    #[test]
    fn giving_up_is_cancellation_in_either_phase() {
        assert!(matches!(
            Phase::Load { model: "m" }.transport(SidecarError::Cancelled),
            TranscriptionError::Cancelled
        ));
        assert!(matches!(
            Phase::Inference.transport(SidecarError::Cancelled),
            TranscriptionError::Cancelled
        ));
    }

    #[test]
    fn a_transport_failure_belongs_to_the_phase_that_hit_it() {
        assert!(matches!(
            Phase::Load { model: "m" }.transport(SidecarError::Closed),
            TranscriptionError::Load {
                source: BackendError::Engine(_),
                ..
            }
        ));
        assert!(matches!(
            Phase::Inference.transport(SidecarError::Closed),
            TranscriptionError::Inference {
                source: BackendError::Engine(_),
                ..
            }
        ));
    }

    #[test]
    fn a_sidecar_that_speaks_another_protocol_is_refused() {
        let mut sidecar = shell(r#"echo '{"ok":true,"ready":true,"protocol":99}'"#);

        let error = handshake(&mut sidecar, "stub", &CancelToken::never())
            .expect_err("a protocol mismatch must be loud");

        assert!(matches!(
            error,
            TranscriptionError::Load {
                source: BackendError::Unusable(detail),
                ..
            } if detail.contains("speaks protocol 99")
        ));
    }

    #[test]
    fn a_sidecar_that_fails_to_load_reports_its_reason() {
        let mut sidecar = shell(r#"echo '{"ok":false,"error":"no such model"}'"#);

        let error = handshake(&mut sidecar, "stub", &CancelToken::never())
            .expect_err("a failed load must be loud");

        assert!(matches!(
            error,
            TranscriptionError::Load {
                source: BackendError::Unusable(detail),
                ..
            } if detail == "no such model"
        ));
    }

    #[test]
    fn a_transcript_during_the_handshake_is_refused() {
        let mut sidecar = shell(r#"echo '{"ok":true,"text":"too early"}'"#);

        let error = handshake(&mut sidecar, "stub", &CancelToken::never())
            .expect_err("a transcript is not readiness");

        assert!(matches!(
            error,
            TranscriptionError::Load {
                source: BackendError::Unusable(detail),
                ..
            } if detail.contains("before reporting readiness")
        ));
    }

    #[test]
    fn a_transcript_is_read_back_from_the_sidecar() {
        let mut sidecar = stub(r#"read line; echo '{"ok":true,"text":"hello there"}'"#);

        let text = transcribe(
            &mut sidecar,
            "/tmp/recording.wav",
            None,
            &CancelToken::never(),
        )
        .expect("the stub should answer");

        assert_eq!(text, "hello there");
    }

    #[test]
    fn a_sidecar_that_reports_a_failure_keeps_serving() {
        let mut sidecar = stub(r#"while read line; do echo '{"ok":false,"error":"boom"}'; done"#);

        let outcome = transcribe(
            &mut sidecar,
            "/tmp/recording.wav",
            None,
            &CancelToken::never(),
        );

        assert!(matches!(
            &outcome,
            Err(TranscriptionError::Inference {
                source: BackendError::Unusable(detail),
                ..
            }) if detail == "boom"
        ));
        assert!(
            still_serving(&mut sidecar, &outcome),
            "it answered, so it is alive"
        );
    }

    #[test]
    fn a_cancelled_press_stops_the_sidecar_instead_of_waiting() {
        let mut sidecar = stub("sleep 600");

        let outcome = transcribe(&mut sidecar, "/tmp/recording.wav", None, &{
            let token = CancelToken::new();
            token.cancel();
            token
        });

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
        assert!(
            !still_serving(&mut sidecar, &outcome),
            "a cancelled sidecar was killed and must not be reused"
        );
    }

    #[test]
    fn a_second_readiness_report_is_refused() {
        let mut sidecar = stub(&format!("read line; {READY}"));

        let outcome = transcribe(
            &mut sidecar,
            "/tmp/recording.wav",
            None,
            &CancelToken::never(),
        );

        assert!(matches!(
            outcome,
            Err(TranscriptionError::Inference {
                source: BackendError::Unusable(detail),
                ..
            }) if detail == "reported readiness twice"
        ));
    }

    #[test]
    fn a_child_that_dies_mid_request_is_not_reused() {
        let mut sidecar = stub("read line; exit 1");

        let outcome = transcribe(
            &mut sidecar,
            "/tmp/recording.wav",
            None,
            &CancelToken::never(),
        );

        assert!(matches!(
            &outcome,
            Err(TranscriptionError::Inference {
                source: BackendError::Engine(_),
                ..
            })
        ));
        assert!(
            !still_serving(&mut sidecar, &outcome),
            "a dead child must not take the next request"
        );
    }
}
