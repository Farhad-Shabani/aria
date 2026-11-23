use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::PairedServer;
use crate::daemon::relay::{PairingCode, PairingCodes, PairingError, fingerprint, mint, to_hex};
use aria_platform::secret::{Secret, SecretError};
use aria_primitives::cancel::CancelToken;
use aria_primitives::private_document::{DocumentError, PrivateDocument};
use aria_settings::config::ServerEndpoint;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};

pub(super) const MAX_DOCUMENT_BYTES: u64 = 16 * 1024;
const MAX_CLIENTS: usize = 16;
pub(super) const CERTIFICATE_NAME: &str = "aria.inference";
const KEY_SERVICE: &str = "Aria Server Pairing Identity";
const SERVER_KEY_ACCOUNT: &str = "server";
const CLIENT_KEY_ACCOUNT: &str = "client";

/// Parsing checks the pin and the code, so a parsed invitation is usable.
#[derive(Serialize, Deserialize)]
#[serde(try_from = "UncheckedInvitation")]
pub(crate) struct Invitation {
    pub(super) endpoint: ServerEndpoint,
    pub(super) certificate_pem: String,
    pub(super) fingerprint: [u8; 32],
    pub(super) code: String,
}

impl Invitation {
    pub(in crate::daemon) fn into_json(self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(&self).map_err(Error::Encode)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedInvitation {
    endpoint: ServerEndpoint,
    certificate_pem: String,
    fingerprint: [u8; 32],
    code: String,
}

impl TryFrom<UncheckedInvitation> for Invitation {
    type Error = Error;

    fn try_from(invitation: UncheckedInvitation) -> Result<Self, Error> {
        validate_pin(&invitation.certificate_pem, &invitation.fingerprint)?;
        if !PairingCode::is_well_formed(&invitation.code) {
            return Err(Error::InvalidInvitation);
        }
        Ok(Self {
            endpoint: invitation.endpoint,
            certificate_pem: invitation.certificate_pem,
            fingerprint: invitation.fingerprint,
            code: invitation.code,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinnedServer {
    pub endpoint: ServerEndpoint,
    pub certificate_pem: String,
    pub fingerprint: [u8; 32],
}

pub(super) struct Credentials {
    pub certificate_pem: String,
    pub key_pem: String,
}

impl Credentials {
    fn stored(&self) -> StoredCredentials {
        StoredCredentials::new(self.certificate_pem.clone())
    }
}

/// What a document keeps of its credentials. The key is a secret apart.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCredentials {
    certificate_pem: String,
    /// Only a document from before the key became a secret has one. Such a
    /// document reads as none, so its pairing starts over.
    #[serde(default, rename = "key_pem", skip_serializing)]
    held_key: Option<IgnoredAny>,
}

impl StoredCredentials {
    fn new(certificate_pem: String) -> Self {
        Self {
            certificate_pem,
            held_key: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredServer {
    credentials: StoredCredentials,
    clients: Vec<[u8; 32]>,
}

pub(in crate::daemon) struct ServerAccess {
    path: PathBuf,
    credentials: Credentials,
    codes: PairingCodes,
    state: Mutex<Trust>,
}

struct Trust {
    clients: Vec<[u8; 32]>,
    connections: HashMap<[u8; 32], CancellationToken>,
    active: Option<([u8; 32], CancellationToken)>,
    stopped: bool,
}

impl ServerAccess {
    pub(in crate::daemon) fn load_or_create(path: &Path) -> Result<Arc<Self>, Error> {
        let (stored, credentials) = load_or_create(
            path,
            &key_secret(SERVER_KEY_ACCOUNT, path),
            |stored: &StoredServer| &stored.credentials,
            |credentials| StoredServer {
                credentials,
                clients: Vec::new(),
            },
        )?;
        super::tls::validate_credentials(&credentials)?;
        if stored.clients.len() > MAX_CLIENTS
            || stored
                .clients
                .iter()
                .enumerate()
                .any(|(index, pin)| stored.clients[..index].contains(pin))
        {
            return Err(Error::InvalidIdentity);
        }
        Ok(Arc::new(Self {
            path: path.to_owned(),
            credentials,
            codes: PairingCodes::default(),
            state: Mutex::new(Trust {
                clients: stored.clients,
                connections: HashMap::new(),
                active: None,
                stopped: false,
            }),
        }))
    }

    pub(in crate::daemon) fn issue(&self, endpoint: &ServerEndpoint) -> Result<Invitation, Error> {
        let (code, _) = self.codes.issue(Instant::now())?;
        let certificate =
            CertificateDer::from_pem_slice(self.credentials.certificate_pem.as_bytes())
                .map_err(|_| Error::InvalidIdentity)?;
        Ok(Invitation {
            endpoint: endpoint.clone(),
            certificate_pem: self.credentials.certificate_pem.clone(),
            fingerprint: fingerprint(&certificate),
            code: code.raw().to_owned(),
        })
    }

    pub(super) fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    /// Whether a pairing code is outstanding, so an unknown client may connect.
    pub(super) fn pairing_is_open(&self) -> bool {
        self.codes.is_live(Instant::now())
    }

    pub(in crate::daemon) fn contains(&self, pin: &[u8; 32]) -> bool {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        !state.stopped && state.clients.contains(pin)
    }

    pub(super) fn connection(&self, pin: [u8; 32]) -> Option<CancellationToken> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopped || !state.clients.contains(&pin) {
            return None;
        }
        Some(state.connections.entry(pin).or_default().clone())
    }

    pub(in crate::daemon) fn pair(&self, pin: [u8; 32], code: &str) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopped {
            return Err(Error::Stopped);
        }
        if !self.codes.redeem(code, Instant::now()) {
            return Err(Error::Unauthenticated);
        }
        self.grant(&mut state, pin, |clients| self.write(clients))
    }

    fn grant(
        &self,
        state: &mut Trust,
        pin: [u8; 32],
        write: impl FnOnce(&[[u8; 32]]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if state.clients.contains(&pin) {
            return Ok(());
        }
        if state.clients.len() >= MAX_CLIENTS {
            return Err(Error::Full);
        }
        let mut clients = state.clients.clone();
        clients.push(pin);
        write(&clients)?;
        state.clients = clients;
        Ok(())
    }

    pub(in crate::daemon) fn admit(self: &Arc<Self>, pin: [u8; 32]) -> Result<Admission, Error> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopped {
            return Err(Error::Stopped);
        }
        if !state.clients.contains(&pin) {
            return Err(Error::Unauthenticated);
        }
        if state.active.is_some() {
            return Err(Error::Busy);
        }
        let cancel = CancellationToken::new();
        state.active = Some((pin, cancel.clone()));
        Ok(Admission {
            access: Arc::clone(self),
            cancel,
        })
    }

    pub(in crate::daemon) fn revoke(&self, pin: [u8; 32]) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopped {
            return Err(Error::Stopped);
        }
        self.remove(&mut state, pin, |clients| self.write(clients))
    }

    fn remove(
        &self,
        state: &mut Trust,
        pin: [u8; 32],
        write: impl FnOnce(&[[u8; 32]]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        state.clients.retain(|client| client != &pin);
        if let Some(cancel) = state.connections.remove(&pin) {
            cancel.cancel();
        }
        if let Some((active, cancel)) = &state.active
            && active == &pin
        {
            cancel.cancel();
        }
        write(&state.clients).map_err(|error| Error::RevokeUnconfirmed(Box::new(error)))
    }

    pub(in crate::daemon) fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stopped = true;
        for cancel in state.connections.values() {
            cancel.cancel();
        }
        if let Some((_, cancel)) = &state.active {
            cancel.cancel();
        }
    }

    fn write(&self, clients: &[[u8; 32]]) -> Result<(), Error> {
        persist(
            &PrivateDocument::new(&self.path, MAX_DOCUMENT_BYTES),
            &StoredServer {
                credentials: self.credentials.stored(),
                clients: clients.to_vec(),
            },
        )
    }
}

pub(in crate::daemon) struct Admission {
    access: Arc<ServerAccess>,
    cancel: CancellationToken,
}

impl Admission {
    pub(in crate::daemon) fn async_cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    /// The same cancellation, for synchronous model work.
    pub(in crate::daemon) fn blocking_cancel(&self) -> CancelToken {
        let cancel = self.cancel.clone();
        CancelToken::from_fn(move || cancel.is_cancelled())
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.access
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active = None;
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredClient {
    credentials: StoredCredentials,
    server: Option<PinnedServer>,
}

/// The key stays in its secret until a connection asks for it, so looking
/// at a pairing never reaches the Keychain.
pub(crate) struct ClientIdentity {
    path: PathBuf,
    certificate_pem: String,
    server: Option<PinnedServer>,
    fingerprint: [u8; 32],
}

impl ClientIdentity {
    /// Opens an access that pairing already created.
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let stored = read_current(
            &PrivateDocument::new(path, MAX_DOCUMENT_BYTES),
            |stored: &StoredClient| &stored.credentials,
        )?
        .ok_or(Error::Unpaired)?;
        Self::validated(path, stored.credentials.certificate_pem, stored.server)
    }

    pub(super) fn load_or_create(path: &Path) -> Result<Self, Error> {
        let (stored, credentials) = load_or_create(
            path,
            &client_key(path),
            |stored: &StoredClient| &stored.credentials,
            |credentials| StoredClient {
                credentials,
                server: None,
            },
        )?;
        super::tls::validate_credentials(&credentials)?;
        Self::validated(path, credentials.certificate_pem, stored.server)
    }

    fn validated(
        path: &Path,
        certificate_pem: String,
        server: Option<PinnedServer>,
    ) -> Result<Self, Error> {
        if let Some(server) = &server {
            validate_pin(&server.certificate_pem, &server.fingerprint)?;
        }
        let certificate = CertificateDer::from_pem_slice(certificate_pem.as_bytes())
            .map_err(|_| Error::InvalidIdentity)?;
        super::tls::validate_certificate(&certificate)?;
        Ok(Self {
            path: path.to_owned(),
            fingerprint: fingerprint(&certificate),
            certificate_pem,
            server,
        })
    }

    /// Reads the key, so only a connection calls it.
    pub(super) fn credentials(&self) -> Result<Credentials, Error> {
        let key_pem = client_key(&self.path).read()?.ok_or(Error::KeyGone)?;
        let credentials = Credentials {
            certificate_pem: self.certificate_pem.clone(),
            key_pem,
        };
        super::tls::validate_credentials(&credentials)?;
        Ok(credentials)
    }

    pub(super) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub(crate) fn paired_server(&self) -> Option<PairedServer> {
        self.server.as_ref().map(|server| PairedServer {
            endpoint: server.endpoint.clone(),
            client_fingerprint: to_hex(&self.fingerprint),
        })
    }

    pub(super) fn pinned_server(&self) -> Result<&PinnedServer, Error> {
        self.server.as_ref().ok_or(Error::Unpaired)
    }

    pub(super) fn remember(&mut self, invitation: &Invitation) -> Result<(), Error> {
        let server = PinnedServer {
            endpoint: invitation.endpoint.clone(),
            certificate_pem: invitation.certificate_pem.clone(),
            fingerprint: invitation.fingerprint,
        };
        persist(
            &PrivateDocument::new(&self.path, MAX_DOCUMENT_BYTES),
            &StoredClient {
                credentials: StoredCredentials::new(self.certificate_pem.clone()),
                server: Some(server.clone()),
            },
        )?;
        self.server = Some(server);
        Ok(())
    }
}

fn validate_pin(pem: &str, pin: &[u8; 32]) -> Result<(), Error> {
    let certificate =
        CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|_| Error::InvalidIdentity)?;
    if &fingerprint(&certificate) != pin {
        return Err(Error::InvalidIdentity);
    }
    super::tls::validate_certificate(&certificate)?;
    Ok(())
}

fn generate() -> Result<Credentials, Error> {
    let minted = mint(CERTIFICATE_NAME, None)?;
    Ok(Credentials {
        certificate_pem: minted.certificate_pem,
        key_pem: minted.key_pem,
    })
}

pub(super) fn client_key(path: &Path) -> Secret {
    key_secret(CLIENT_KEY_ACCOUNT, path)
}

/// Where there is no Keychain, the key sits beside its document.
fn key_secret(account: &'static str, document: &Path) -> Secret {
    crate::daemon::secret::secret(KEY_SERVICE, account, document.with_extension("key"))
}

/// Reads the document and its credentials, or persists `create`'s document
/// with new credentials when there is none yet. A document whose key is gone
/// is invalid, never replaced.
fn load_or_create<T: Serialize + serde::de::DeserializeOwned>(
    path: &Path,
    secret: &Secret,
    credentials_of: impl Fn(&T) -> &StoredCredentials,
    create: impl FnOnce(StoredCredentials) -> T,
) -> Result<(T, Credentials), Error> {
    let document = PrivateDocument::new(path, MAX_DOCUMENT_BYTES);
    if let Some(stored) = read_current(&document, &credentials_of)? {
        let key_pem = secret.read()?.ok_or(Error::KeyGone)?;
        let certificate_pem = credentials_of(&stored).certificate_pem.clone();
        return Ok((
            stored,
            Credentials {
                certificate_pem,
                key_pem,
            },
        ));
    }
    let credentials = generate()?;
    // Before the document, so a document never names a key that isn't kept.
    secret.write(&credentials.key_pem)?;
    let stored = create(credentials.stored());
    persist(&document, &stored)?;
    Ok((stored, credentials))
}

/// The document, or `None` when there is none or it is from before the key
/// became a secret.
fn read_current<T: serde::de::DeserializeOwned>(
    document: &PrivateDocument,
    credentials_of: impl Fn(&T) -> &StoredCredentials,
) -> Result<Option<T>, Error> {
    let Some(stored) = document.read::<T>()? else {
        return Ok(None);
    };
    if credentials_of(&stored).held_key.is_some() {
        tracing::warn!("a remote access document from before the Keychain; pairing starts over");
        return Ok(None);
    }
    Ok(Some(stored))
}

fn persist(document: &PrivateDocument, value: &impl Serialize) -> Result<(), Error> {
    document
        .prepare(value)?
        .commit()?
        .confirm_durability()
        .map_err(Error::DurabilityUnconfirmed)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the remote access document is not usable")]
    Document(#[from] DocumentError),
    #[error("the remote access key is not usable")]
    Key(#[from] SecretError),
    #[error("the pairing key is gone; pair again")]
    KeyGone,
    #[error(
        "the access was published, but durability is unconfirmed; authorization was not granted"
    )]
    DurabilityUnconfirmed(#[source] DocumentError),
    #[error("revocation applies in this process, but restart durability is unconfirmed")]
    RevokeUnconfirmed(#[source] Box<Self>),
    #[error("the remote access or certificate is invalid")]
    InvalidIdentity,
    #[error("the invitation is invalid")]
    InvalidInvitation,
    #[error("the remote endpoint must be an HTTPS origin without credentials, query, or fragment")]
    InvalidEndpoint,
    #[error("this endpoint has not been paired")]
    Unpaired,
    #[error("the pairing changed after this engine was chosen")]
    PairingMoved,
    #[error("the client is not authorized")]
    Unauthenticated,
    #[error("the remote server already pairs 16 clients")]
    Full,
    #[error("the remote server is busy")]
    Busy,
    #[error("the remote server is stopping")]
    Stopped,
    #[error("failed to create the remote certificate")]
    Certificate(#[from] rcgen::Error),
    #[error("failed to issue an invitation")]
    Invite(#[from] PairingError),
    #[error("failed to encode the invitation")]
    Encode(#[source] serde_json::Error),
    #[error("the remote TLS access is not usable")]
    Tls(#[from] super::tls::Error),
}

impl From<Error> for RpcFailure {
    fn from(error: Error) -> Self {
        let (code, message) = match error {
            Error::RevokeUnconfirmed(_) => (
                RpcErrorCode::DurabilityUnconfirmed,
                "client denied for this process; revocation durability is unconfirmed",
            ),
            Error::DurabilityUnconfirmed(_) => (
                RpcErrorCode::DurabilityUnconfirmed,
                "trust was written but durability is unconfirmed",
            ),
            Error::InvalidInvitation | Error::InvalidEndpoint => (
                RpcErrorCode::InvalidRequest,
                "invalid remote invitation or endpoint",
            ),
            Error::Unauthenticated | Error::Unpaired | Error::PairingMoved => (
                RpcErrorCode::Unauthenticated,
                "remote access is not authorized",
            ),
            Error::Full => (
                RpcErrorCode::PermissionDenied,
                "client enrollment limit reached",
            ),
            Error::Busy => (RpcErrorCode::Busy, "inference is busy"),
            Error::Stopped => (RpcErrorCode::ShuttingDown, "inference host is stopping"),
            Error::Document(_)
            | Error::Key(_)
            | Error::KeyGone
            | Error::InvalidIdentity
            | Error::Certificate(_)
            | Error::Invite(_)
            | Error::Encode(_)
            | Error::Tls(_) => (RpcErrorCode::Internal, "remote access operation failed"),
        };
        Self::new(code, message)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn path(directory: &tempfile::TempDir) -> PathBuf {
        directory.path().join("inference").join("server.json")
    }

    fn access() -> (tempfile::TempDir, PathBuf, Arc<ServerAccess>) {
        let directory = tempfile::tempdir().unwrap();
        let path = path(&directory);
        let access = ServerAccess::load_or_create(&path).unwrap();
        (directory, path, access)
    }

    fn localhost(port: u16) -> ServerEndpoint {
        ServerEndpoint::parse(&format!("https://localhost:{port}")).unwrap()
    }

    fn grant(access: &ServerAccess, pin: [u8; 32]) {
        let invitation = access.issue(&localhost(5000)).unwrap();
        access.pair(pin, &invitation.code).unwrap();
    }

    enum WriteFault {
        Prepare,
        Rename,
        Durability,
    }

    fn faulted_write(
        access: &ServerAccess,
        clients: &[[u8; 32]],
        fault: &WriteFault,
    ) -> Result<(), Error> {
        let error = || DocumentError::File {
            action: "injected storage failure",
            source: io::ErrorKind::Other.into(),
        };
        if matches!(fault, WriteFault::Prepare) {
            return Err(error().into());
        }
        let document = PrivateDocument::new(&access.path, MAX_DOCUMENT_BYTES);
        let prepared = document.prepare(&StoredServer {
            credentials: access.credentials.stored(),
            clients: clients.to_vec(),
        })?;
        if matches!(fault, WriteFault::Rename) {
            return Err(error().into());
        }
        let _committed = prepared.commit()?;
        Err(Error::DurabilityUnconfirmed(
            DocumentError::DurabilityUnconfirmed(io::ErrorKind::Other.into()),
        ))
    }

    #[test]
    fn a_document_from_before_the_keychain_starts_over() {
        let (_directory, path, before) = access();
        grant(&before, [1; 32]);
        let mut held: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        held["credentials"]["key_pem"] = serde_json::json!(before.credentials.key_pem);
        std::fs::write(&path, serde_json::to_vec(&held).unwrap()).unwrap();

        let after = ServerAccess::load_or_create(&path).unwrap();

        assert!(after.credentials.certificate_pem != before.credentials.certificate_pem);
        assert!(!after.contains(&[1; 32]));
        assert!(
            !String::from_utf8(std::fs::read(&path).unwrap())
                .unwrap()
                .contains("key_pem")
        );
    }

    #[test]
    fn a_document_whose_key_is_gone_or_unreadable_is_refused_and_kept() {
        let (_directory, path, _) = access();
        let document = std::fs::read(&path).unwrap();
        let key = path.with_extension("key");
        std::fs::remove_file(&key).unwrap();
        assert!(matches!(
            ServerAccess::load_or_create(&path),
            Err(Error::KeyGone)
        ));
        std::fs::create_dir(&key).unwrap();
        assert!(matches!(
            ServerAccess::load_or_create(&path),
            Err(Error::Key(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), document);
    }

    #[test]
    fn malformed_identity_is_never_replaced() {
        let (_directory, path, _) = access();
        for bytes in [b"{ invalid".as_slice(), b"{\"unknown\":true}"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(ServerAccess::load_or_create(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn identity_rejects_mismatched_keys_and_excess_clients() {
        let (_directory, path, before) = access();
        let key = key_secret(SERVER_KEY_ACCOUNT, &path);
        key.write(&generate().unwrap().key_pem).unwrap();
        assert!(matches!(
            ServerAccess::load_or_create(&path),
            Err(Error::Tls(_))
        ));
        key.write(&before.credentials.key_pem).unwrap();
        let document = PrivateDocument::new(&path, MAX_DOCUMENT_BYTES);
        let mut stored: StoredServer = document.read().unwrap().unwrap();
        stored.clients = (0..=MAX_CLIENTS).map(|index| [index as u8; 32]).collect();
        persist(&document, &stored).unwrap();
        assert!(matches!(
            ServerAccess::load_or_create(&path),
            Err(Error::InvalidIdentity)
        ));
    }

    #[test]
    fn identity_rejects_public_files_symlinks_and_oversized_files() {
        let (_directory, path, _) = access();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ServerAccess::load_or_create(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let linked = path.with_file_name("linked.json");
        symlink(&path, &linked).unwrap();
        assert!(ServerAccess::load_or_create(&linked).is_err());
        std::fs::write(&path, vec![b' '; MAX_DOCUMENT_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            ServerAccess::load_or_create(&path),
            Err(Error::Document(DocumentError::TooLarge { .. }))
        ));
    }

    #[test]
    fn a_pairing_code_grants_once_and_trust_survives_reload() {
        let (_directory, path, access) = access();
        let invite = access.issue(&localhost(5000)).unwrap();
        access.pair([1; 32], &invite.code).unwrap();
        assert!(matches!(
            access.pair([2; 32], &invite.code),
            Err(Error::Unauthenticated)
        ));
        assert!(
            ServerAccess::load_or_create(&path)
                .unwrap()
                .contains(&[1; 32])
        );
    }

    #[test]
    fn a_tampered_invitation_does_not_parse() {
        let (_directory, _, access) = access();
        let issued = serde_json::to_value(access.issue(&localhost(5000)).unwrap()).unwrap();
        assert!(serde_json::from_value::<Invitation>(issued.clone()).is_ok());
        for (field, tampered) in [
            ("code", serde_json::json!("not a pairing code")),
            ("fingerprint", serde_json::json!(vec![0_u8; 32])),
        ] {
            let mut invitation = issued.clone();
            invitation[field] = tampered;
            assert!(serde_json::from_value::<Invitation>(invitation).is_err());
        }
    }

    #[test]
    fn revocation_ends_peer_connections_and_pairing_creates_a_fresh_lease() {
        let (_directory, _, access) = access();
        grant(&access, [1; 32]);
        grant(&access, [2; 32]);
        let first = access.connection([1; 32]).unwrap();
        let another = access.connection([1; 32]).unwrap();
        let other_peer = access.connection([2; 32]).unwrap();

        access.revoke([1; 32]).unwrap();

        assert!(first.is_cancelled());
        assert!(another.is_cancelled());
        assert!(!other_peer.is_cancelled());
        assert!(access.connection([1; 32]).is_none());
        grant(&access, [1; 32]);
        let replacement = access.connection([1; 32]).unwrap();
        assert!(!replacement.is_cancelled());
        assert!(first.is_cancelled());

        access.stop();

        assert!(replacement.is_cancelled());
        assert!(other_peer.is_cancelled());
        assert!(access.connection([1; 32]).is_none());
        assert!(access.connection([2; 32]).is_none());
    }

    #[test]
    fn revoke_racing_connection_never_leaves_an_uncancelled_lease() {
        let (_directory, _, access) = access();
        grant(&access, [1; 32]);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker = Arc::clone(&access);
        let ready = Arc::clone(&barrier);
        let attempt = std::thread::spawn(move || {
            ready.wait();
            worker.connection([1; 32])
        });
        barrier.wait();
        access.revoke([1; 32]).unwrap();
        if let Some(connection) = attempt.join().unwrap() {
            assert!(connection.is_cancelled());
        }
        assert!(access.connection([1; 32]).is_none());
    }

    #[test]
    fn revoked_work_is_cancelled_but_holds_admission_until_drop() {
        let (_directory, _, access) = access();
        grant(&access, [1; 32]);
        grant(&access, [2; 32]);
        let admitted = access.admit([1; 32]).unwrap();
        let token = admitted.blocking_cancel();
        access.revoke([1; 32]).unwrap();
        assert!(admitted.async_cancel().is_cancelled());
        assert!(token.is_cancelled());
        assert!(matches!(access.admit([1; 32]), Err(Error::Unauthenticated)));
        assert!(matches!(access.admit([2; 32]), Err(Error::Busy)));
        drop(admitted);
        assert!(access.admit([2; 32]).is_ok());
    }

    #[test]
    fn revoke_racing_admission_never_leaves_uncancelled_work() {
        let (_directory, _, access) = access();
        grant(&access, [1; 32]);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker = Arc::clone(&access);
        let ready = Arc::clone(&barrier);
        let attempt = std::thread::spawn(move || {
            ready.wait();
            worker.admit([1; 32])
        });
        barrier.wait();
        access.revoke([1; 32]).unwrap();
        match attempt.join().unwrap() {
            Ok(admitted) => assert!(admitted.async_cancel().is_cancelled()),
            Err(Error::Unauthenticated) => {}
            Err(error) => panic!("unexpected admission failure: {error}"),
        }
        assert!(matches!(access.admit([1; 32]), Err(Error::Unauthenticated)));
    }

    #[test]
    fn a_grant_is_unusable_after_every_storage_failure() {
        for fault in [
            WriteFault::Prepare,
            WriteFault::Rename,
            WriteFault::Durability,
        ] {
            let (_directory, path, access) = access();
            let mut state = access.state.lock().unwrap();
            assert!(
                access
                    .grant(&mut state, [1; 32], |clients| faulted_write(
                        &access, clients, &fault
                    ))
                    .is_err()
            );
            drop(state);
            assert!(!access.contains(&[1; 32]));
            let stored: StoredServer = PrivateDocument::new(path, MAX_DOCUMENT_BYTES)
                .read()
                .unwrap()
                .unwrap();
            assert_eq!(
                stored.clients.contains(&[1; 32]),
                matches!(fault, WriteFault::Durability)
            );
        }
    }

    #[test]
    fn a_revoke_keeps_runtime_denial_after_every_storage_failure() {
        for fault in [
            WriteFault::Prepare,
            WriteFault::Rename,
            WriteFault::Durability,
        ] {
            let (_directory, path, access) = access();
            grant(&access, [1; 32]);
            let admitted = access.admit([1; 32]).unwrap();
            let connection = access.connection([1; 32]).unwrap();
            let mut state = access.state.lock().unwrap();
            assert!(matches!(
                access.remove(&mut state, [1; 32], |clients| faulted_write(
                    &access, clients, &fault
                )),
                Err(Error::RevokeUnconfirmed(_))
            ));
            drop(state);
            assert!(admitted.async_cancel().is_cancelled());
            assert!(connection.is_cancelled());
            assert!(access.connection([1; 32]).is_none());
            assert!(!access.contains(&[1; 32]));
            grant(&access, [2; 32]);
            let restarted = ServerAccess::load_or_create(&path).unwrap();
            assert!(!restarted.contains(&[1; 32]));
            assert!(restarted.contains(&[2; 32]));
        }
    }

    #[test]
    fn client_persists_identity_before_pairing_and_remembers_only_explicit_server() {
        let directory = tempfile::tempdir().unwrap();
        let server = ServerAccess::load_or_create(&path(&directory)).unwrap();
        let path = path(&directory).with_file_name("client.json");
        assert!(matches!(ClientIdentity::open(&path), Err(Error::Unpaired)));
        assert!(!path.exists());
        let mut client = ClientIdentity::load_or_create(&path).unwrap();
        let certificate = client.credentials().unwrap().certificate_pem;
        assert_eq!(
            ClientIdentity::load_or_create(&path)
                .unwrap()
                .credentials()
                .unwrap()
                .certificate_pem,
            certificate
        );
        assert!(matches!(client.pinned_server(), Err(Error::Unpaired)));
        client
            .remember(&server.issue(&localhost(5000)).unwrap())
            .unwrap();
        assert_eq!(
            ClientIdentity::load_or_create(&path)
                .unwrap()
                .pinned_server()
                .unwrap()
                .endpoint,
            localhost(5000)
        );
    }

    #[test]
    fn stopping_rejects_late_revocation_without_writing() {
        let (_directory, path, access) = access();
        grant(&access, [1; 32]);
        let stored = std::fs::read(&path).unwrap();

        access.stop();

        assert!(matches!(access.revoke([1; 32]), Err(Error::Stopped)));
        assert_eq!(std::fs::read(&path).unwrap(), stored);
        assert!(
            ServerAccess::load_or_create(&path)
                .unwrap()
                .contains(&[1; 32])
        );
    }

    #[test]
    fn stopping_cancels_the_active_operation_and_denies_admission() {
        let (_directory, _, access) = access();
        grant(&access, [1; 32]);
        let admitted = access.admit([1; 32]).unwrap();
        access.stop();
        assert!(admitted.async_cancel().is_cancelled());
        assert!(!access.contains(&[1; 32]));
        assert!(matches!(access.admit([1; 32]), Err(Error::Stopped)));
    }
}
