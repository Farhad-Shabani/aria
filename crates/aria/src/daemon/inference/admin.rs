//! A running server's administration over its same-user socket.

use std::path::Path;

use aria_kit::cancel::CancelToken;
use tonic::transport::{Channel, Endpoint};

use super::Paths;
use super::client::{ADMIN_TIMEOUT, RemoteClientError, bounded, rpc_error, run};
use super::identity::MAX_DOCUMENT_BYTES;
use crate::config::ServerEndpoint;
use crate::daemon::relay::fingerprint_from_hex;
use crate::ipc::{protocol, wire};
use crate::rpc::RpcErrorCode;

pub fn remote_invite(endpoint: &str) -> Result<String, RemoteClientError> {
    let endpoint = ServerEndpoint::parse(endpoint).ok_or(RemoteClientError::InvalidEndpoint)?;
    let home = crate::config::home_directory().map_err(RemoteClientError::Config)?;
    run(bounded(&CancelToken::never(), ADMIN_TIMEOUT, async {
        let channel = admin_channel(&home).await?;
        let response =
            wire::inference_admin_service_client::InferenceAdminServiceClient::new(channel)
                .max_decoding_message_size(MAX_DOCUMENT_BYTES as usize + 1024)
                .issue_invite(wire::InferenceInviteRequest {
                    endpoint: endpoint.to_string(),
                })
                .await
                .map_err(rpc_error)?;
        String::from_utf8(response.into_inner().invitation_json)
            .map_err(|_| RemoteClientError::Response)
    }))
}

pub fn remote_revoke(fingerprint: &str) -> Result<(), RemoteClientError> {
    let fingerprint =
        fingerprint_from_hex(fingerprint).ok_or(RemoteClientError::InvalidFingerprint)?;
    let home = crate::config::home_directory().map_err(RemoteClientError::Config)?;
    run(bounded(&CancelToken::never(), ADMIN_TIMEOUT, async {
        let channel = admin_channel(&home).await?;
        wire::inference_admin_service_client::InferenceAdminServiceClient::new(channel)
            .max_decoding_message_size(1024)
            .revoke(wire::InferenceRevokeRequest {
                fingerprint: fingerprint.to_vec(),
            })
            .await
            .map_err(|status| {
                if protocol::rpc_failure(&status)
                    .is_some_and(|failure| failure.code == RpcErrorCode::DurabilityUnconfirmed)
                {
                    RemoteClientError::RevocationUnconfirmed
                } else {
                    rpc_error(status)
                }
            })?;
        Ok(())
    }))
}

async fn admin_channel(home: &Path) -> Result<Channel, RemoteClientError> {
    let runtime = Paths::in_home(home).runtime();
    Endpoint::from_shared(format!("unix://{}", runtime.socket_path().display()))
        .map_err(|_| RemoteClientError::Connection)?
        .connect()
        .await
        .map_err(|_| RemoteClientError::Connection)
}
