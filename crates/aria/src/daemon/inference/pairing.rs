//! The desktop's side of pairing: redeeming an invitation and pinning the server.

use std::path::Path;

use aria_kit::cancel::CancelToken;

use super::client::{ADMIN_TIMEOUT, RemoteClientError, bounded, identity_error, rpc_error, run};
use super::identity::{ClientIdentity, Invitation, MAX_DOCUMENT_BYTES};
use super::{Paths, tls};
use crate::config::Environment;
use crate::ipc::wire;
use crate::primitives::local_socket::ExclusiveListener;
use crate::primitives::private_fs::prepare_directory;

/// Pairs with the server named in an invitation read from `input`, so the
/// secret never appears on a command line.
pub fn remote_pair(input: impl std::io::Read) -> Result<String, RemoteClientError> {
    let invitation = read_invitation(input)?;
    let environment = Environment::capture().map_err(RemoteClientError::Config)?;
    pair_at(environment.home(), &invitation)
}

fn read_invitation(input: impl std::io::Read) -> Result<String, RemoteClientError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    input
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| RemoteClientError::Invitation)?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(RemoteClientError::Invitation);
    }
    String::from_utf8(bytes).map_err(|_| RemoteClientError::Invitation)
}

pub(super) fn pair_at(home: &Path, invitation: &str) -> Result<String, RemoteClientError> {
    if invitation.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(RemoteClientError::Invitation);
    }
    let invitation: Invitation =
        serde_json::from_str(invitation).map_err(|_| RemoteClientError::Invitation)?;
    let paths = Paths::in_home(home);
    if let Some(parent) = paths.directory().parent() {
        prepare_directory(parent).map_err(|_| RemoteClientError::Storage)?;
    }
    prepare_directory(paths.directory()).map_err(|_| RemoteClientError::Storage)?;
    let _lock = ExclusiveListener::bind(paths.pairing_socket(), paths.pairing_lock())
        .map_err(|_| RemoteClientError::PairingLock)?;
    let mut identity =
        ClientIdentity::load_or_create(&paths.client_identity()).map_err(identity_error)?;
    let endpoint =
        tls::invited_endpoint(&identity, &invitation).map_err(|_| RemoteClientError::Identity)?;
    run(bounded(&CancelToken::never(), ADMIN_TIMEOUT, async {
        let channel = endpoint
            .connect()
            .await
            .map_err(|_| RemoteClientError::Connection)?;
        wire::pairing_service_client::PairingServiceClient::new(channel)
            .max_decoding_message_size(1024)
            .pair(wire::PairRequest {
                code: invitation.code.clone(),
            })
            .await
            .map_err(rpc_error)?;
        Ok(())
    }))?;
    identity.remember(&invitation).map_err(identity_error)?;
    Ok(identity.fingerprint().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitations_are_bounded_utf8() {
        let limit = MAX_DOCUMENT_BYTES as usize;
        assert!(read_invitation(&vec![b'a'; limit + 1][..]).is_err());
        assert!(read_invitation(&[255][..]).is_err());
        assert_eq!(read_invitation(&b"{}"[..]).unwrap(), "{}");
    }
}
