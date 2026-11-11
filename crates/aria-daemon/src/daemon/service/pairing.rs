//! Pairing a phone: the Mac app shows a one-time code, and the phone
//! presents it to have its certificate pinned.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::ipc::wire;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};

use super::super::relay::{Invite, PairingError, Phones, presented};
use aria_primitives::error_chain;

/// Served on the LAN to unpaired phones too: the code is the credential.
pub(in crate::daemon) struct PairingRpc {
    phones: Arc<Phones>,
}

impl PairingRpc {
    pub(in crate::daemon) fn new(phones: Arc<Phones>) -> Self {
        Self { phones }
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
        let Some(phone) = phone else {
            return Err(super::failed(METHOD, &PairingError::CodeRefused));
        };
        let phones = Arc::clone(&self.phones);
        super::blocking(METHOD, move || phones.pair(phone, &code))
            .await?
            .map_err(|error| crate::daemon::relay::redacted(super::failed(METHOD, &error)))?;
        super::served(METHOD);
        Ok(Response::new(wire::PairResponse {}))
    }
}

/// Served on the unix socket only, and only while the relay runs.
pub(in crate::daemon) struct PairingAdminRpc {
    phones: Arc<Phones>,
    instance: String,
    fingerprint: [u8; 32],
}

impl PairingAdminRpc {
    pub(in crate::daemon) fn new(
        phones: Arc<Phones>,
        instance: String,
        fingerprint: [u8; 32],
    ) -> Self {
        Self {
            phones,
            instance,
            fingerprint,
        }
    }
}

#[tonic::async_trait]
impl wire::pairing_admin_service_server::PairingAdminService for PairingAdminRpc {
    async fn issue_invite(
        &self,
        _request: Request<wire::IssueInviteRequest>,
    ) -> Result<Response<wire::IssueInviteResponse>, Status> {
        const METHOD: &str = "PairingAdminService/IssueInvite";
        let (code, expires_in) = self
            .phones
            .issue()
            .map_err(|error| super::failed(METHOD, &error))?;
        let invite = Invite {
            instance: self.instance.clone(),
            code,
            fingerprint: self.fingerprint,
            expires_in,
            pairing_count: self.phones.pairing_count(),
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

    async fn revoke_phones(
        &self,
        _request: Request<wire::RevokePhonesRequest>,
    ) -> Result<Response<wire::RevokePhonesResponse>, Status> {
        const METHOD: &str = "PairingAdminService/RevokePhones";
        let phones = Arc::clone(&self.phones);
        super::blocking(METHOD, move || phones.revoke_all())
            .await?
            .map_err(|error| super::failed(METHOD, &error))?;
        super::served(METHOD);
        Ok(Response::new(wire::RevokePhonesResponse {}))
    }
}

#[cfg(test)]
mod tests {
    use wire::pairing_admin_service_server::PairingAdminService;

    use super::*;

    #[tokio::test]
    async fn issue_invite_reports_the_live_code_and_the_pin() {
        let directory = tempfile::tempdir().unwrap();
        let pairing = crate::daemon::relay::load_pairing(
            &directory.path().join("pairing.json"),
            "Aria (test)",
        )
        .unwrap();
        let rpc = PairingAdminRpc::new(
            Arc::clone(&pairing.phones),
            "Aria (test)".to_owned(),
            pairing.certificate_fingerprint,
        );

        let issued = rpc
            .issue_invite(Request::new(wire::IssueInviteRequest {}))
            .await
            .unwrap()
            .into_inner();
        let (live, _) = pairing.phones.issue().unwrap();

        assert_eq!(issued.instance_name, "Aria (test)");
        assert_eq!(issued.code, live.raw());
        assert_eq!(issued.fingerprint, pairing.certificate_fingerprint);
        let expires_in = std::time::Duration::try_from(issued.expires_in.unwrap()).unwrap();
        assert!(expires_in <= std::time::Duration::from_secs(60));
        assert_eq!(issued.pairing_count, 0);

        pairing.phones.pair([1; 32], live.raw()).unwrap();
        let reissued = rpc
            .issue_invite(Request::new(wire::IssueInviteRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert_ne!(reissued.code, issued.code);
        assert_eq!(reissued.pairing_count, 1);
    }

    #[tokio::test]
    async fn revoke_phones_unpins_every_phone() {
        let directory = tempfile::tempdir().unwrap();
        let pairing = crate::daemon::relay::load_pairing(
            &directory.path().join("pairing.json"),
            "Aria (test)",
        )
        .unwrap();
        let rpc = PairingAdminRpc::new(
            Arc::clone(&pairing.phones),
            "Aria (test)".to_owned(),
            pairing.certificate_fingerprint,
        );
        let (live, _) = pairing.phones.issue().unwrap();
        pairing.phones.pair([1; 32], live.raw()).unwrap();

        rpc.revoke_phones(Request::new(wire::RevokePhonesRequest {}))
            .await
            .unwrap();

        assert!(!pairing.phones.contains(&[1; 32]));
    }
}
