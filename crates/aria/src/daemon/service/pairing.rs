//! Pairing a phone: the Mac app shows a one-time code, and the phone
//! presents it to have its certificate pinned.

use std::sync::Arc;
use std::time::Instant;

use tonic::{Request, Response, Status};

use crate::ipc::wire;
use crate::primitives::error_chain;
use crate::rpc::{RpcErrorCode, RpcFailure};

use super::super::relay::{PairingCodes, PairingError, Phones, presented};

/// Served on the LAN to unpaired phones too: the code is the credential.
pub(in crate::daemon) struct PairingRpc {
    codes: Arc<PairingCodes>,
    phones: Arc<Phones>,
}

impl PairingRpc {
    pub(in crate::daemon) fn new(codes: Arc<PairingCodes>, phones: Arc<Phones>) -> Self {
        Self { codes, phones }
    }
}

#[tonic::async_trait]
impl wire::pairing_service_server::PairingService for PairingRpc {
    async fn pair(
        &self,
        request: Request<wire::PairRequest>,
    ) -> Result<Response<wire::PairResponse>, Status> {
        const METHOD: &str = "PairingService/Pair";
        let phone = presented(&request);
        let code = request.into_inner().code;
        let Some(phone) = phone.filter(|_| self.codes.redeem(&code, Instant::now())) else {
            let failure = RpcFailure::new(
                RpcErrorCode::Unauthenticated,
                "the pairing code is wrong, used, or expired",
            );
            // Unpaired phones reach this method, so a stranger can make it
            // log. Quieter than `super::failed`, and never the code.
            tracing::debug!(
                method = METHOD,
                status = failure.code.label(),
                "request failed"
            );
            return Err(failure.into());
        };
        let phones = Arc::clone(&self.phones);
        tokio::task::spawn_blocking(move || phones.admit(phone))
            .await
            .map_err(|_panicked| {
                tracing::error!(method = METHOD, "the pairing job panicked");
                super::failed(
                    METHOD,
                    RpcFailure::new(RpcErrorCode::Internal, "pinning the phone failed"),
                )
            })?
            .map_err(|error| {
                let code = match error {
                    PairingError::Full => RpcErrorCode::PermissionDenied,
                    PairingError::Document(_)
                    | PairingError::Random(_)
                    | PairingError::Certificate(_)
                    | PairingError::Missing => {
                        tracing::error!(
                            method = METHOD,
                            error = %error_chain(&error),
                            "failed to pin the phone"
                        );
                        RpcErrorCode::Internal
                    }
                };
                super::failed(METHOD, RpcFailure::new(code, error_chain(&error)))
            })?;
        tracing::info!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(wire::PairResponse {}))
    }
}

/// Served on the unix socket only, and only while the relay runs.
pub(in crate::daemon) struct InviteRpc {
    codes: Arc<PairingCodes>,
    instance: String,
    fingerprint: [u8; 32],
}

impl InviteRpc {
    pub(in crate::daemon) fn new(
        codes: Arc<PairingCodes>,
        instance: String,
        fingerprint: [u8; 32],
    ) -> Self {
        Self {
            codes,
            instance,
            fingerprint,
        }
    }
}

#[tonic::async_trait]
impl wire::invite_service_server::InviteService for InviteRpc {
    async fn issue_invite(
        &self,
        _request: Request<wire::IssueInviteRequest>,
    ) -> Result<Response<wire::IssueInviteResponse>, Status> {
        const METHOD: &str = "InviteService/IssueInvite";
        let (code, remaining) = self.codes.issue(Instant::now()).map_err(|error| {
            super::failed(
                METHOD,
                RpcFailure::new(RpcErrorCode::Internal, error_chain(&error)),
            )
        })?;
        let expires_in_ms = u32::try_from(remaining.as_millis()).map_err(|_| {
            super::failed(
                METHOD,
                RpcFailure::new(
                    RpcErrorCode::Internal,
                    "the code lifetime overflows u32 milliseconds",
                ),
            )
        })?;
        // The Mac app polls this while its pairing pane is open.
        tracing::debug!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(wire::IssueInviteResponse {
            instance_name: self.instance.clone(),
            code: code.raw().to_owned(),
            fingerprint: self.fingerprint.to_vec(),
            expires_in_ms,
        }))
    }
}

#[cfg(test)]
mod tests {
    use wire::invite_service_server::InviteService;

    use super::*;

    #[tokio::test]
    async fn issue_invite_reports_the_live_code_and_the_pin() {
        let directory = tempfile::tempdir().unwrap();
        let pairing = crate::daemon::relay::load_pairing(
            &directory.path().join("pairing.json"),
            "Aria (test)",
        )
        .unwrap();
        let codes = Arc::new(PairingCodes::default());
        let rpc = InviteRpc::new(
            Arc::clone(&codes),
            "Aria (test)".to_owned(),
            pairing.certificate_fingerprint,
        );

        let issued = rpc
            .issue_invite(Request::new(wire::IssueInviteRequest {}))
            .await
            .unwrap()
            .into_inner();
        let (live, _) = codes.issue(Instant::now()).unwrap();

        assert_eq!(issued.instance_name, "Aria (test)");
        assert_eq!(issued.code, live.raw());
        assert_eq!(issued.fingerprint, pairing.certificate_fingerprint);
        assert!(issued.expires_in_ms <= 60_000);
    }
}
