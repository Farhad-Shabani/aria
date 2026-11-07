//! The desktop's side of pairing: redeeming an invitation and pinning the server.

use std::fs;
use std::io;
use std::path::Path;

use aria_kit::cancel::CancelToken;
use aria_settings::rpc::RpcErrorCode;

use super::client::{ADMIN_TIMEOUT, RemoteClientError, bounded, identity_error, rpc_error, run};
use super::identity::{ClientIdentity, Invitation, MAX_DOCUMENT_BYTES};
use super::{Paths, tls};
use crate::daemon::relay::to_hex;
use crate::ipc::wire;
use aria_primitives::local_socket::ExclusiveListener;
use aria_primitives::private_document::PrivateDocument;
use aria_primitives::private_fs::prepare_directory;

/// Reads an invitation from `input`, so the secret never appears on a
/// command line.
pub fn read_invitation(input: impl std::io::Read) -> Result<String, RemoteClientError> {
    use std::io::Read;
    let mut invitation = String::new();
    input
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_string(&mut invitation)
        .map_err(RemoteClientError::invitation)?;
    bounded_invitation(&invitation)?;
    Ok(invitation)
}

#[derive(Debug, thiserror::Error)]
#[error("the invitation exceeds {} KiB", MAX_DOCUMENT_BYTES / 1024)]
struct InvitationTooLarge;

fn bounded_invitation(invitation: &str) -> Result<(), RemoteClientError> {
    if invitation.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(RemoteClientError::invitation(InvitationTooLarge));
    }
    Ok(())
}

/// Pairs with the server `invitation` names, and pins it in place of any
/// earlier one.
pub fn pair_server(home: &Path, invitation: &str) -> Result<String, RemoteClientError> {
    bounded_invitation(invitation)?;
    let invitation: Invitation =
        serde_json::from_str(invitation).map_err(RemoteClientError::invitation)?;
    let paths = Paths::in_home(home);
    if let Some(parent) = paths.directory().parent() {
        prepare_directory(parent).map_err(RemoteClientError::storage)?;
    }
    prepare_directory(paths.directory()).map_err(RemoteClientError::storage)?;
    let _lock = ExclusiveListener::bind(paths.pairing_socket(), paths.pairing_lock())
        .map_err(RemoteClientError::PairingLock)?;
    let mut identity =
        ClientIdentity::load_or_create(&paths.client_identity()).map_err(identity_error)?;
    let endpoint = tls::invited_endpoint(&identity, &invitation)
        .map_err(|error| RemoteClientError::Identity(error.into()))?;
    run(bounded(&CancelToken::never(), ADMIN_TIMEOUT, async {
        let channel = endpoint
            .connect()
            .await
            .map_err(|error| RemoteClientError::Connection(Some(error)))?;
        wire::inference_pairing_service_client::InferencePairingServiceClient::new(channel)
            .max_decoding_message_size(1024)
            .pair(wire::InferencePairRequest {
                code: invitation.code.clone(),
            })
            .await
            .map_err(rpc_error)?;
        Ok(())
    }))?;
    identity.remember(&invitation).map_err(identity_error)?;
    Ok(to_hex(&identity.fingerprint()))
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
pub fn forget(home: &Path) -> Result<Forgotten, RemoteClientError> {
    let paths = Paths::in_home(home);
    let path = paths.client_identity();
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Forgotten::NothingPaired);
        }
        Err(error) => return Err(RemoteClientError::storage(error)),
    }
    let _lock = ExclusiveListener::bind(paths.pairing_socket(), paths.pairing_lock())
        .map_err(RemoteClientError::PairingLock)?;
    let forgotten = match ClientIdentity::open(&path) {
        Ok(identity) => {
            let client_fingerprint = to_hex(&identity.fingerprint());
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
        .map_err(RemoteClientError::storage)?
    {
        return Ok(Forgotten::NothingPaired);
    }
    match fs::File::open(paths.directory()).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(forgotten),
        Err(source) => Err(RemoteClientError::ForgetUnconfirmed { forgotten, source }),
    }
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

/// Why pairing failed, as `aria remote pair --json` names it for a script or
/// the app to branch on. A name never changes meaning once shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingFailure {
    /// The invitation can't be read, or the server refused it.
    InvitationInvalid,
    Busy,
    Unreachable,
    Timeout,
    PermissionDenied,
    Failed,
}

impl PairingFailure {
    pub fn label(self) -> &'static str {
        match self {
            Self::InvitationInvalid => "invitation_invalid",
            Self::Busy => "busy",
            Self::Unreachable => "unreachable",
            Self::Timeout => "timeout",
            Self::PermissionDenied => "permission_denied",
            Self::Failed => "failed",
        }
    }
}

impl From<&RemoteClientError> for PairingFailure {
    fn from(error: &RemoteClientError) -> Self {
        match error {
            RemoteClientError::Invitation(_)
            | RemoteClientError::InvalidEndpoint
            | RemoteClientError::Rpc(
                RpcErrorCode::Unauthenticated | RpcErrorCode::InvalidRequest,
            ) => Self::InvitationInvalid,
            RemoteClientError::PairingLock(_) => Self::Busy,
            RemoteClientError::Connection(_) => Self::Unreachable,
            RemoteClientError::Timeout => Self::Timeout,
            RemoteClientError::Rpc(RpcErrorCode::PermissionDenied) => Self::PermissionDenied,
            RemoteClientError::Rpc(_)
            | RemoteClientError::Config(_)
            | RemoteClientError::Storage(_)
            | RemoteClientError::InvalidFingerprint
            | RemoteClientError::Identity(_)
            | RemoteClientError::DurabilityUnconfirmed
            | RemoteClientError::PinUnconfirmed(_)
            | RemoteClientError::RevocationUnconfirmed
            | RemoteClientError::ForgetUnconfirmed { .. }
            | RemoteClientError::Runtime(_)
            | RemoteClientError::Cancelled
            | RemoteClientError::Unsupported
            | RemoteClientError::Response
            | RemoteClientError::Decode(_)
            | RemoteClientError::AudioTooLong
            | RemoteClientError::Audio(_)
            | RemoteClientError::PartialAudio
            | RemoteClientError::Backpressure => Self::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_failure_the_app_explains_gets_its_code() {
        let cases = [
            (RemoteClientError::InvalidEndpoint, "invitation_invalid"),
            (RemoteClientError::Connection(None), "unreachable"),
            (RemoteClientError::Timeout, "timeout"),
            (
                RemoteClientError::Rpc(RpcErrorCode::Unauthenticated),
                "invitation_invalid",
            ),
            (
                RemoteClientError::Rpc(RpcErrorCode::InvalidRequest),
                "invitation_invalid",
            ),
            (
                RemoteClientError::Rpc(RpcErrorCode::PermissionDenied),
                "permission_denied",
            ),
            (RemoteClientError::Rpc(RpcErrorCode::Internal), "failed"),
        ];
        for (error, code) in cases {
            assert_eq!(PairingFailure::from(&error).label(), code, "{error}");
        }
    }

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
        let fingerprint = super::super::pair_server_for_tests(home.path(), "https://box.test:9851");

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
        let fingerprint = to_hex(&identity.fingerprint());
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
    fn a_storage_failure_keeps_its_cause_under_the_same_message() {
        let home = tempfile::tempdir().unwrap();
        prepare_directory(&home.path().join(".aria")).unwrap();
        fs::write(Paths::in_home(home.path()).directory(), "").unwrap();

        let error = forget(home.path()).unwrap_err();
        let kind = std::iter::successors(std::error::Error::source(&error), |cause| cause.source())
            .find_map(|cause| cause.downcast_ref::<io::Error>())
            .map(io::Error::kind);
        assert_eq!(kind, Some(io::ErrorKind::NotADirectory));
        assert!(
            aria_primitives::error_chain(&error)
                .starts_with("cannot prepare remote client storage: ")
        );
    }

    #[test]
    fn a_pairing_in_progress_is_not_forgotten() {
        let (home, paths) = prepared_home();
        fs::write(paths.client_identity(), "{").unwrap();
        let _pairing =
            ExclusiveListener::bind(paths.pairing_socket(), paths.pairing_lock()).unwrap();

        assert!(matches!(
            forget(home.path()),
            Err(RemoteClientError::PairingLock(_))
        ));
        assert!(paths.client_identity().exists());
    }
}
