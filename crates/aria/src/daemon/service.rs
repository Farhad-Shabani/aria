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
mod preferences;
mod relay;
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
pub(super) use preferences::PreferencesRpc;
pub(super) use relay::RelayRpc;
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
    tracing::info!(
        method,
        status = failure.code.label(),
        message = failure.message,
        "request failed"
    );
    failure.into()
}
