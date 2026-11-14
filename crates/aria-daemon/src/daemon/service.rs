//! The daemon's gRPC services, one per capability, translating wire requests
//! to domain operations.
//!
//! The domain stays synchronous: each handler submits to its capability's
//! actor, which runs the work on the blocking pool. Each logs one line: the
//! method and how it ended.

mod chat;
mod dictation;
mod events;
mod history;
mod inference;
mod microphone;
mod pairing;
mod relay;
mod settings;
mod speaker;
mod speech;
mod system_transcription;
mod vocabulary;

use crate::ipc::protocol::failure_status;
use tonic::Status;

use aria_primitives::error_chain;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};

pub(super) use chat::ChatRpc;
pub(super) use dictation::DictationRpc;
pub(super) use events::EventsService;
pub(super) use history::{HistoryRpc, HistoryWorker};
pub(super) use inference::{
    InferenceAdminRpc, InferencePairingRpc, InferenceRpc, authenticate as authenticate_inference,
};
pub(super) use microphone::MicrophoneRpc;
pub(super) use pairing::{PairingAdminRpc, PairingRpc};
pub(super) use relay::RelayRpc;
pub(super) use settings::SettingsRpc;
pub(super) use speaker::SpeakerRpc;
pub(super) use speech::SpeechRpc;
pub(super) use system_transcription::SystemTranscriptionRpc;
pub(super) use vocabulary::VocabularyRpc;

fn invalid_request(method: &'static str, error: &dyn std::error::Error) -> Status {
    failed(
        method,
        RpcFailure::new(RpcErrorCode::InvalidRequest, error_chain(error)),
    )
}

/// Runs domain work off the async pool. Domain code is synchronous and may
/// lock or join threads, so it never runs on a runtime worker. A panic inside
/// is the daemon's own fault and answers as Internal.
pub(super) async fn blocking<T: Send + 'static>(
    method: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_panicked| {
            failed(
                method,
                RpcFailure::new(RpcErrorCode::Internal, "the operation panicked"),
            )
        })
}

pub(in crate::daemon) fn served(method: &'static str) {
    tracing::info!(method, status = "ok", "request served");
}

/// Every failure a handler returns goes through here, so each is logged once
/// and encoded once. A domain error converts by the category it declares.
pub(in crate::daemon) fn failed(method: &'static str, failure: impl Into<RpcFailure>) -> Status {
    let failure = failure.into();
    log_failure(method, &failure);
    failure_status(failure)
}

/// One level rule. Unauthenticated is what a stranger on the LAN can cause,
/// so it stays quiet; Internal is the daemon's own fault, so it stands out.
/// A settings fault's message can quote the file, so only its place is
/// logged.
fn log_failure(method: &'static str, failure: &RpcFailure) {
    let status = failure.code.label();
    if failure.code == RpcErrorCode::Unauthenticated {
        tracing::debug!(method, status, "request failed");
    } else if failure.code == RpcErrorCode::Internal {
        tracing::warn!(method, status, message = failure.message, "request failed");
    } else if failure.code == RpcErrorCode::SettingsInvalid {
        match failure.settings_position {
            Some(position) => tracing::info!(
                method,
                status,
                line = position.line,
                column = position.column,
                "request failed"
            ),
            None => tracing::info!(method, status, "request failed"),
        }
    } else {
        tracing::info!(method, status, message = failure.message, "request failed");
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use super::super::operation::CallError;
    use super::*;
    use aria_settings::Position;
    use aria_settings::rpc::RpcErrorCode;

    /// A log sink the test reads back.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("the sink").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_settings_failure_logs_its_place_and_not_its_message() {
        let captured = Captured::default();
        let sink = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .finish();
        let failure = RpcFailure::new(
            RpcErrorCode::SettingsInvalid,
            "fix settings.toml first: tts.voice does not accept \"secret-value\"",
        )
        .with_settings_position(Some(Position { line: 4, column: 9 }));

        tracing::subscriber::with_default(subscriber, || log_failure("Method", &failure));

        let log = String::from_utf8(captured.0.lock().expect("the sink").clone()).expect("text");
        assert!(log.contains("line=4") && log.contains("column=9"), "{log}");
        assert!(
            !log.contains("secret-value") && !log.contains("tts.voice"),
            "{log}"
        );
    }

    /// An actor's own failures carry the daemon's category, whatever its
    /// payload declares; the chat actor's payload declares none.
    #[test]
    fn an_actor_failure_logs_the_daemons_category() {
        let captured = Captured::default();
        let sink = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .finish();

        let status = tracing::subscriber::with_default(subscriber, || {
            failed("Method", &CallError::<Infallible>::Unavailable)
        });

        let log = String::from_utf8(captured.0.lock().expect("the sink").clone()).expect("text");
        assert!(log.contains(r#"status="shutting_down""#), "{log}");
        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert_eq!(
            crate::ipc::protocol::rpc_failure(&status).map(|failure| failure.code),
            Some(RpcErrorCode::ShuttingDown)
        );
    }

    /// A settings fault keeps its place however deep it sits in the chain,
    /// so a press refused by an invalid file points at the line.
    #[test]
    fn a_settings_fault_keeps_its_place_through_a_runtime_error() {
        use super::super::dictation::DictationRuntimeError;

        let fault = aria_settings::Loaded::parse(
            "[memory]\nenabled = maybe\n",
            std::path::Path::new("/home"),
        )
        .expect_err("invalid");
        let error = DictationRuntimeError::Settings(aria_settings::StoreError::FileInvalid(fault));

        let failure = RpcFailure::from(&error);

        assert_eq!(failure.code, RpcErrorCode::SettingsInvalid);
        assert_eq!(
            failure.settings_position,
            Some(Position {
                line: 2,
                column: 11
            })
        );
    }

    /// Every handler encodes a failure through `failed`, so each one is
    /// logged once and the level rule holds everywhere.
    #[test]
    fn every_service_fails_through_failed() {
        let services = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon/service");
        for entry in std::fs::read_dir(services).expect("the service directory") {
            let path = entry.expect("an entry").path();
            let source = std::fs::read_to_string(&path).expect("readable");
            assert!(
                !source.contains("failure_status("),
                "{} encodes a failure outside `failed`",
                path.display()
            );
        }
    }
}
