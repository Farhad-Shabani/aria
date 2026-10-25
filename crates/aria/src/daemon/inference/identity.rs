use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::ServerEndpoint;
use crate::daemon::relay::{PairingCode, PairingCodes, PairingError, fingerprint, mint};
use crate::primitives::cancel::CancelToken;
use crate::primitives::private_document::{DocumentError, PrivateDocument};
use crate::rpc::{RpcErrorCode, RpcFailure};

pub(super) const MAX_DOCUMENT_BYTES: u64 = 16 * 1024;
const MAX_CLIENTS: usize = 16;
pub(super) const CERTIFICATE_NAME: &str = "aria.inference";

/// Parsing checks the pin and the code, so a parsed invitation is usable.
#[derive(Serialize, Deserialize)]
#[serde(try_from = "UncheckedInvitation")]
pub(crate) struct Invitation {
    pub(super) endpoint: ServerEndpoint,
    pub(super) certificate_pem: String,
    pub(super) fingerprint: [u8; 32],
    pub(super) code: String,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Credentials {
    pub certificate_pem: String,
    pub key_pem: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredServer {
    credentials: Credentials,
    clients: Vec<[u8; 32]>,
}

#[derive(Serialize)]
struct ServerUpdate<'a> {
    credentials: &'a Credentials,
    clients: &'a [[u8; 32]],
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
        let stored = load_or_create(path, || {
            Ok(StoredServer {
                credentials: generate()?,
                clients: Vec::new(),
            })
        })?;
        super::tls::validate_credentials(&stored.credentials)?;
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
            credentials: stored.credentials,
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
            &ServerUpdate {
                credentials: &self.credentials,
                clients,
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
    credentials: Credentials,
    server: Option<PinnedServer>,
}

pub(crate) struct ClientIdentity {
    path: PathBuf,
    stored: StoredClient,
}

impl ClientIdentity {
    /// Opens an access that pairing already created.
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let stored = PrivateDocument::new(path, MAX_DOCUMENT_BYTES)
            .read::<StoredClient>()?
            .ok_or(Error::Unpaired)?;
        Self::validated(path, stored)
    }

    pub(super) fn load_or_create(path: &Path) -> Result<Self, Error> {
        let stored = load_or_create(path, || {
            Ok(StoredClient {
                credentials: generate()?,
                server: None,
            })
        })?;
        Self::validated(path, stored)
    }

    fn validated(path: &Path, stored: StoredClient) -> Result<Self, Error> {
        super::tls::validate_credentials(&stored.credentials)?;
        if let Some(server) = &stored.server {
            validate_pin(&server.certificate_pem, &server.fingerprint)?;
        }
        Ok(Self {
            path: path.to_owned(),
            stored,
        })
    }

    pub(super) fn credentials(&self) -> &Credentials {
        &self.stored.credentials
    }

    pub(super) fn fingerprint(&self) -> Result<[u8; 32], Error> {
        let certificate =
            CertificateDer::from_pem_slice(self.stored.credentials.certificate_pem.as_bytes())
                .map_err(|_| Error::InvalidIdentity)?;
        Ok(fingerprint(&certificate))
    }

    pub(super) fn paired_endpoint(&self) -> Option<&ServerEndpoint> {
        self.stored.server.as_ref().map(|server| &server.endpoint)
    }

    pub(super) fn pinned_server(&self, endpoint: &ServerEndpoint) -> Result<&PinnedServer, Error> {
        self.stored
            .server
            .as_ref()
            .filter(|server| &server.endpoint == endpoint)
            .ok_or(Error::Unpaired)
    }

    pub(super) fn remember(&mut self, invitation: &Invitation) -> Result<(), Error> {
        let server = PinnedServer {
            endpoint: invitation.endpoint.clone(),
            certificate_pem: invitation.certificate_pem.clone(),
            fingerprint: invitation.fingerprint,
        };
        #[derive(Serialize)]
        struct Update<'a> {
            credentials: &'a Credentials,
            server: &'a PinnedServer,
        }
        persist(
            &PrivateDocument::new(&self.path, MAX_DOCUMENT_BYTES),
            &Update {
                credentials: &self.stored.credentials,
                server: &server,
            },
        )?;
        self.stored.server = Some(server);
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

/// Reads the document, or persists `create`'s value when there is none yet.
fn load_or_create<T: Serialize + serde::de::DeserializeOwned>(
    path: &Path,
    create: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    let document = PrivateDocument::new(path, MAX_DOCUMENT_BYTES);
    if let Some(stored) = document.read::<T>()? {
        return Ok(stored);
    }
    let stored = create()?;
    persist(&document, &stored)?;
    Ok(stored)
}

fn persist(document: &PrivateDocument, value: &impl Serialize) -> Result<(), Error> {
    document
        .prepare(value)?
        .commit()?
        .confirm_durability()
        .map_err(Error::DurabilityUnconfirmed)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("the remote access document is not usable")]
    Document(#[from] DocumentError),
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
            Error::Unauthenticated | Error::Unpaired => (
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
            | Error::InvalidIdentity
            | Error::Certificate(_)
            | Error::Invite(_)
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
        let prepared = document.prepare(&ServerUpdate {
            credentials: &access.credentials,
            clients,
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
    fn malformed_identity_is_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = path(&directory);
        ServerAccess::load_or_create(&path).unwrap();
        for bytes in [b"{ invalid".as_slice(), b"{\"unknown\":true}"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(ServerAccess::load_or_create(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn identity_rejects_mismatched_keys_and_excess_clients() {
        let directory = tempfile::tempdir().unwrap();
        let path = path(&directory);
        ServerAccess::load_or_create(&path).unwrap();
        let document = PrivateDocument::new(&path, MAX_DOCUMENT_BYTES);
        let mut stored: StoredServer = document.read().unwrap().unwrap();
        stored.credentials.key_pem = generate().unwrap().key_pem;
        persist(&document, &stored).unwrap();
        assert!(matches!(
            ServerAccess::load_or_create(&path),
            Err(Error::Tls(_))
        ));
        stored.credentials = generate().unwrap();
        stored.clients = (0..=MAX_CLIENTS).map(|index| [index as u8; 32]).collect();
        persist(&document, &stored).unwrap();
        assert!(matches!(
            ServerAccess::load_or_create(&path),
            Err(Error::InvalidIdentity)
        ));
    }

    #[test]
    fn identity_rejects_public_files_symlinks_and_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = path(&directory);
        ServerAccess::load_or_create(&path).unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let path = path(&directory);
        let access = ServerAccess::load_or_create(&path).unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let access = ServerAccess::load_or_create(&path(&directory)).unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let access = ServerAccess::load_or_create(&path(&directory)).unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let access = ServerAccess::load_or_create(&path(&directory)).unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let access = ServerAccess::load_or_create(&path(&directory)).unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let access = ServerAccess::load_or_create(&path(&directory)).unwrap();
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
            let directory = tempfile::tempdir().unwrap();
            let path = path(&directory);
            let access = ServerAccess::load_or_create(&path).unwrap();
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
            let directory = tempfile::tempdir().unwrap();
            let path = path(&directory);
            let access = ServerAccess::load_or_create(&path).unwrap();
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
        let certificate = client.credentials().certificate_pem.clone();
        assert_eq!(
            ClientIdentity::load_or_create(&path)
                .unwrap()
                .credentials()
                .certificate_pem,
            certificate
        );
        assert!(matches!(
            client.pinned_server(&localhost(5000)),
            Err(Error::Unpaired)
        ));
        client
            .remember(&server.issue(&localhost(5000)).unwrap())
            .unwrap();
        assert!(
            ClientIdentity::load_or_create(&path)
                .unwrap()
                .pinned_server(&localhost(5000))
                .is_ok()
        );
        let respelled = ServerEndpoint::parse("https://LOCALHOST:5000/").unwrap();
        assert!(client.pinned_server(&respelled).is_ok());
        assert!(matches!(
            client.pinned_server(&localhost(5001)),
            Err(Error::Unpaired)
        ));
    }

    #[test]
    fn stopping_rejects_late_revocation_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = path(&directory);
        let access = ServerAccess::load_or_create(&path).unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let access = ServerAccess::load_or_create(&path(&directory)).unwrap();
        grant(&access, [1; 32]);
        let admitted = access.admit([1; 32]).unwrap();
        access.stop();
        assert!(admitted.async_cancel().is_cancelled());
        assert!(!access.contains(&[1; 32]));
        assert!(matches!(access.admit([1; 32]), Err(Error::Stopped)));
    }
}
