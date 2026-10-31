//! The desktop's side of pairing: redeeming an invitation and pinning the server.

use std::fs;
use std::io;
use std::path::Path;

use aria_kit::cancel::CancelToken;

use super::client::{ADMIN_TIMEOUT, RemoteClientError, bounded, identity_error, rpc_error, run};
use super::identity::{ClientIdentity, Invitation, MAX_DOCUMENT_BYTES};
use super::{Paths, tls};
use crate::ipc::wire;
use crate::primitives::local_socket::ExclusiveListener;
use crate::primitives::private_document::PrivateDocument;
use crate::primitives::private_fs::prepare_directory;

/// Pairs with the server named in an invitation read from `input`, so the
/// secret never appears on a command line.
pub fn remote_pair(input: impl std::io::Read) -> Result<String, RemoteClientError> {
    let invitation = read_invitation(input)?;
    let home = crate::config::home_directory().map_err(RemoteClientError::Config)?;
    pair_at(&home, &invitation)
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

/// What forgetting removed. A server may trust any key it removed until it
/// revokes that fingerprint there.
#[derive(Debug, PartialEq, Eq)]
pub enum Forgotten {
    NothingPaired,
    Server {
        client_fingerprint: String,
    },
    /// The key from a pairing that stopped before pinning the server, which
    /// may still have accepted it.
    UnfinishedPairing {
        client_fingerprint: String,
    },
    /// A file that would not open, so its fingerprint is unknown.
    Unreadable,
}

/// Removes the key along with the pin; the next pairing makes a new one.
pub(crate) fn forget(home: &Path) -> Result<Forgotten, RemoteClientError> {
    let paths = Paths::in_home(home);
    let path = paths.client_identity();
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Forgotten::NothingPaired);
        }
        Err(_) => return Err(RemoteClientError::Storage),
    }
    let _lock = ExclusiveListener::bind(paths.pairing_socket(), paths.pairing_lock())
        .map_err(|_| RemoteClientError::PairingLock)?;
    let forgotten = match ClientIdentity::open(&path) {
        Ok(identity) => {
            let client_fingerprint = identity.fingerprint().to_owned();
            if identity.paired_server().is_some() {
                Forgotten::Server { client_fingerprint }
            } else {
                Forgotten::UnfinishedPairing { client_fingerprint }
            }
        }
        Err(_) => Forgotten::Unreadable,
    };
    if !PrivateDocument::new(&path, MAX_DOCUMENT_BYTES)
        .remove()
        .map_err(|_| RemoteClientError::Storage)?
    {
        return Ok(Forgotten::NothingPaired);
    }
    fs::File::open(paths.directory())
        .and_then(|directory| directory.sync_all())
        .map_err(|_| RemoteClientError::ForgetUnconfirmed(forgotten.revoke_hint()))?;
    Ok(forgotten)
}

impl Forgotten {
    /// What the server needs to stop trusting this machine.
    pub fn revoke_hint(&self) -> String {
        match self {
            Self::NothingPaired => String::new(),
            Self::Server { client_fingerprint }
            | Self::UnfinishedPairing { client_fingerprint } => {
                format!("aria remote revoke {client_fingerprint}")
            }
            Self::Unreadable => "its fingerprint is unknown".to_owned(),
        }
    }
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

    /// A home whose inference directory exists, so tests can place files.
    fn prepared_home() -> (tempfile::TempDir, Paths) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::in_home(home.path());
        prepare_directory(&home.path().join(".aria")).unwrap();
        prepare_directory(paths.directory()).unwrap();
        (home, paths)
    }

    #[test]
    fn forgetting_a_pinned_server_returns_its_revoke_fingerprint() {
        let (home, paths) = prepared_home();
        let fingerprint = super::super::pin_server_for_tests(home.path(), "https://box.test:9851");

        assert_eq!(
            forget(home.path()).unwrap(),
            Forgotten::Server {
                client_fingerprint: fingerprint
            }
        );
        assert!(!paths.client_identity().exists());
        assert_eq!(forget(home.path()).unwrap(), Forgotten::NothingPaired);
    }

    #[test]
    fn an_unfinished_or_unreadable_pairing_is_removed_and_named() {
        let (home, paths) = prepared_home();
        let identity = ClientIdentity::load_or_create(&paths.client_identity()).unwrap();
        let fingerprint = identity.fingerprint().to_owned();
        assert_eq!(
            forget(home.path()).unwrap(),
            Forgotten::UnfinishedPairing {
                client_fingerprint: fingerprint
            }
        );

        fs::write(paths.client_identity(), "{").unwrap();
        assert_eq!(forget(home.path()).unwrap(), Forgotten::Unreadable);
        assert!(!paths.client_identity().exists());
    }

    #[test]
    fn a_pairing_in_progress_is_not_forgotten() {
        let (home, paths) = prepared_home();
        fs::write(paths.client_identity(), "{").unwrap();
        let _pairing =
            ExclusiveListener::bind(paths.pairing_socket(), paths.pairing_lock()).unwrap();

        assert!(matches!(
            forget(home.path()),
            Err(RemoteClientError::PairingLock)
        ));
        assert!(paths.client_identity().exists());
    }
}
