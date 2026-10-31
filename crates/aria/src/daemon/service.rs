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
mod listen;
mod pairing;
mod relay;
mod settings;
mod speaker;
mod speech;
mod vocabulary;

use tonic::Status;

use crate::primitives::error_chain;
use crate::rpc::{RpcErrorCode, RpcFailure};

pub(super) use chat::ChatRpc;
pub(super) use dictation::DictationRpc;
pub(super) use events::EventsService;
pub(super) use history::{HistoryRpc, HistoryWorker};
pub(super) use inference::{
    InferenceAdminRpc, InferencePairingRpc, InferenceRpc, authenticate as authenticate_inference,
};
pub(super) use listen::ListenRpc;
pub(super) use pairing::{InviteRpc, PairingRpc};
pub(super) use relay::RelayRpc;
pub(super) use settings::SettingsRpc;
pub(super) use speaker::SpeakerRpc;
pub(super) use speech::SpeechRpc;
pub(super) use vocabulary::VocabularyRpc;

fn invalid_request(method: &'static str, error: &dyn std::error::Error) -> Status {
    failed(
        method,
        RpcFailure::new(RpcErrorCode::InvalidRequest, error_chain(error)),
    )
}

fn failed(method: &'static str, failure: RpcFailure) -> Status {
    log_failure(method, &failure);
    failure.into()
}

/// A settings fault's message can quote the file, so only its place is
/// logged.
fn log_failure(method: &'static str, failure: &RpcFailure) {
    if failure.code != crate::rpc::RpcErrorCode::SettingsInvalid {
        tracing::info!(
            method,
            status = failure.code.label(),
            message = failure.message,
            "request failed"
        );
        return;
    }
    match failure.settings_position {
        Some(position) => tracing::info!(
            method,
            status = failure.code.label(),
            line = position.line,
            column = position.column,
            "request failed"
        ),
        None => tracing::info!(method, status = failure.code.label(), "request failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::rpc::RpcErrorCode;
    use crate::settings::Position;

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
}
