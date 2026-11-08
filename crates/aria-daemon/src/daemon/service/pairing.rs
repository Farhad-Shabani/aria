//! Pairing a phone: the Mac app shows a one-time code, and the phone
//! presents it to have its certificate pinned.

use std::sync::Arc;
use std::time::Instant;

use tonic::{Request, Response, Status};

use crate::ipc::wire;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};

use super::super::relay::{Invite, PairingCodes, Phones, presented};
use aria_primitives::error_chain;

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
            return Err(super::failed(
                METHOD,
                RpcFailure::new(
                    RpcErrorCode::Unauthenticated,
                    "the pairing code is wrong, used, or expired",
                ),
            ));
        };
        let phones = Arc::clone(&self.phones);
        super::blocking(METHOD, move || phones.admit(phone))
            .await?
            .map_err(|error| crate::daemon::relay::redacted(super::failed(METHOD, &error)))?;
        self.codes.record_pairing();
        super::served(METHOD);
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
        let (code, expires_in) = self
            .codes
            .issue(Instant::now())
            .map_err(|error| super::failed(METHOD, &error))?;
        let invite = Invite {
            instance: self.instance.clone(),
            code,
            fingerprint: self.fingerprint,
            expires_in,
            pairing_count: self.codes.pairing_count(),
        };
        let response = wire::IssueInviteResponse::try_from(invite).map_err(|error| {
            super::failed(
                METHOD,
                RpcFailure::new(RpcErrorCode::Internal, error_chain(&error)),
            )
        })?;
        // The Mac app polls this while its pairing pane is open.
        tracing::debug!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(response))
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
        let expires_in = std::time::Duration::try_from(issued.expires_in.unwrap()).unwrap();
        assert!(expires_in <= std::time::Duration::from_secs(60));
        assert_eq!(issued.pairing_count, 0);

        assert!(codes.redeem(live.raw(), Instant::now()));
        codes.record_pairing();
        let reissued = rpc
            .issue_invite(Request::new(wire::IssueInviteRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert_ne!(reissued.code, issued.code);
        assert_eq!(reissued.pairing_count, 1);
    }
}
