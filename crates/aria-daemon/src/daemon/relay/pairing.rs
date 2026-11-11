//! This Mac's certificate, which phones pin, and the phones it pins in turn.
//! One private file holds both.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Status};

use aria_primitives::error_chain;
use aria_primitives::private_document::{DocumentError, PrivateDocument};
use aria_settings::rpc::{RpcErrorCode, RpcFailure};

use super::code::{PairingCode, PairingCodes};

pub(crate) const RELATIVE_PATH: &str = ".aria/phone/pairing.json";
const MAX_FILE_BYTES: u64 = 16 * 1024;
const MAX_PHONES: usize = 16;
/// The certificate's subject alternative name. The phone never checks it;
/// the pin is the check.
const CERTIFICATE_NAME: &str = "aria.local";
const CERTIFICATE_LIFETIME_DAYS: i64 = 3650;

pub(in crate::daemon) struct Pairing {
    pub(in crate::daemon) certificate_pem: String,
    pub(in crate::daemon) key_pem: String,
    /// The value a phone pins.
    pub(in crate::daemon) certificate_fingerprint: [u8; 32],
    pub(in crate::daemon) phones: Arc<Phones>,
}

/// The phones this Mac pins, by certificate fingerprint, and the code that
/// pins the next one. Once the daemon runs, the only writer of the pairing
/// file.
#[derive(Debug)]
pub(in crate::daemon) struct Phones {
    path: PathBuf,
    codes: PairingCodes,
    /// Held for the whole of a write. Handshakes read `paired`, so they
    /// never wait on the disk.
    writing: Mutex<()>,
    paired: Mutex<Paired>,
}

#[derive(Debug)]
struct Paired {
    fingerprints: Vec<[u8; 32]>,
    /// The next revoke cancels it, ending every connection opened since the
    /// last one.
    revocation: CancellationToken,
}

impl Phones {
    pub(in crate::daemon) fn contains(&self, fingerprint: &[u8; 32]) -> bool {
        self.paired().fingerprints.contains(fingerprint)
    }

    /// What ends a paired phone's connection: cancelled when the phone is
    /// revoked. `None` for a phone this Mac does not pin.
    pub(in crate::daemon) fn connection(
        &self,
        fingerprint: &[u8; 32],
    ) -> Option<CancellationToken> {
        let paired = self.paired();
        paired
            .fingerprints
            .contains(fingerprint)
            .then(|| paired.revocation.clone())
    }

    /// What the next revoke cancels.
    pub(in crate::daemon) fn revocation(&self) -> CancellationToken {
        self.paired().revocation.clone()
    }

    /// The live code and how long it has left, minting one if none is live.
    pub(in crate::daemon) fn issue(&self) -> Result<(PairingCode, Duration), PairingError> {
        self.codes.issue(Instant::now())
    }

    /// Whether a code is live, so an unknown phone may connect to redeem it.
    pub(in crate::daemon) fn pairing_is_open(&self) -> bool {
        self.codes.is_live(Instant::now())
    }

    /// Phones paired since the daemon started.
    pub(in crate::daemon) fn pairing_count(&self) -> u64 {
        self.codes.pairing_count()
    }

    /// Pins `fingerprint` in exchange for the live code, on disk before in
    /// memory. Pairing a paired phone again writes nothing.
    pub(in crate::daemon) fn pair(
        &self,
        fingerprint: [u8; 32],
        code: &str,
    ) -> Result<(), PairingError> {
        self.pair_with(fingerprint, || self.codes.redeem(code, Instant::now()))
    }

    /// Redeems under the write lock, so a revoke either sees the phone
    /// pinned or retires the code before it is redeemed.
    fn pair_with(
        &self,
        fingerprint: [u8; 32],
        redeem: impl FnOnce() -> bool,
    ) -> Result<(), PairingError> {
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        if !redeem() {
            return Err(PairingError::CodeRefused);
        }
        let mut phones = {
            let paired = self.paired();
            if paired.fingerprints.contains(&fingerprint) {
                self.codes.record_pairing();
                return Ok(());
            }
            if paired.fingerprints.len() >= MAX_PHONES {
                return Err(PairingError::Full);
            }
            paired.fingerprints.clone()
        };
        phones.push(fingerprint);
        let document = PrivateDocument::new(&self.path, MAX_FILE_BYTES);
        let mut stored = document
            .read::<StoredPairing>()?
            .ok_or(PairingError::Missing)?;
        // From memory, not the file: a revoke whose write failed left the
        // file pinning phones this daemon no longer trusts.
        stored.phones = phones;
        write(&document, &stored)?;
        let mut paired = self.paired();
        paired.fingerprints.push(fingerprint);
        self.codes.record_pairing();
        tracing::info!(paired_phones = paired.fingerprints.len(), "paired a phone");
        Ok(())
    }

    /// Unpins every phone, closes their connections, and ends the live code,
    /// in memory before on disk: a failed write still cuts them off until
    /// the daemon restarts. This Mac keeps its certificate, so each phone can
    /// pair with it again.
    pub(in crate::daemon) fn revoke_all(&self) -> Result<(), PairingError> {
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        self.codes.retire();
        let revoked = {
            let mut paired = self.paired();
            std::mem::take(&mut paired.revocation).cancel();
            std::mem::take(&mut paired.fingerprints).len()
        };
        self.unpin_stored()
            .map_err(|error| PairingError::RevokeUnconfirmed(Box::new(error)))?;
        tracing::info!(revoked_phones = revoked, "revoked every phone");
        Ok(())
    }

    /// A file that is gone or damaged pins nobody, since the next start
    /// mints new material over it, so there is nothing to rewrite.
    fn unpin_stored(&self) -> Result<(), PairingError> {
        let document = PrivateDocument::new(&self.path, MAX_FILE_BYTES);
        let mut stored = match document.read::<StoredPairing>() {
            Ok(Some(stored)) => stored,
            Ok(None) | Err(DocumentError::Parse(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        stored.phones.clear();
        write(&document, &stored)
    }

    // Each change only pushes or takes, so a poisoned lock still holds a
    // usable list.
    fn paired(&self) -> std::sync::MutexGuard<'_, Paired> {
        self.paired.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPairing {
    instance: String,
    certificate_pem: String,
    key_pem: String,
    phones: Vec<[u8; 32]>,
}

/// Reads the material, minting it on first use. A changed hostname rewrites
/// the stored name and keeps everything else.
pub(in crate::daemon) fn load_or_create(
    path: &Path,
    instance: &str,
) -> Result<Pairing, PairingError> {
    let document = PrivateDocument::new(path, MAX_FILE_BYTES);
    let (stored, fingerprint) = match read_usable(&document)? {
        Some((stored, fingerprint)) if stored.instance == instance => (stored, fingerprint),
        Some((mut stored, fingerprint)) => {
            stored.instance = instance.to_owned();
            // The material still works; the stored name is only a label.
            if let Err(error) = write(&document, &stored) {
                tracing::warn!(
                    error = %error_chain(&error),
                    "kept the pairing, but could not record the new hostname"
                );
            }
            (stored, fingerprint)
        }
        None => {
            let (fresh, fingerprint) = generate(instance)?;
            write(&document, &fresh)?;
            tracing::info!("minted this Mac's pairing material");
            (fresh, fingerprint)
        }
    };
    Ok(Pairing {
        certificate_pem: stored.certificate_pem,
        key_pem: stored.key_pem,
        certificate_fingerprint: fingerprint,
        phones: Arc::new(Phones {
            path: path.to_owned(),
            codes: PairingCodes::default(),
            writing: Mutex::new(()),
            paired: Mutex::new(Paired {
                fingerprints: stored.phones,
                revocation: CancellationToken::new(),
            }),
        }),
    })
}

/// SHA-256 of a certificate's DER bytes: how either side names the other.
pub(in crate::daemon) fn fingerprint(der: &[u8]) -> [u8; 32] {
    Sha256::digest(der).into()
}

/// The stored material and its fingerprint. `None` when there is none or it
/// is damaged: no phone can use damaged material, so replacing it revokes
/// nothing. An unreadable file still fails closed.
fn read_usable(
    document: &PrivateDocument,
) -> Result<Option<(StoredPairing, [u8; 32])>, PairingError> {
    let stored = match document.read::<StoredPairing>() {
        Ok(Some(stored)) => stored,
        Ok(None) => return Ok(None),
        Err(DocumentError::Parse(error)) => {
            tracing::warn!(
                error = %error_chain(&error),
                "the pairing file is damaged; minting new pairing material"
            );
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    match CertificateDer::from_pem_slice(stored.certificate_pem.as_bytes()) {
        Ok(der) => {
            let fingerprint = fingerprint(&der);
            Ok(Some((stored, fingerprint)))
        }
        Err(error) => {
            tracing::warn!(
                error = %error_chain(&error),
                "the stored pairing certificate is not PEM; minting new pairing material"
            );
            Ok(None)
        }
    }
}

fn write(document: &PrivateDocument, stored: &StoredPairing) -> Result<(), PairingError> {
    let committed = document.prepare(stored)?.commit()?;
    // The rename already landed, so the material is on disk either way.
    if let Err(error) = committed.confirm_durability() {
        tracing::warn!(
            error = %error_chain(&error),
            "the pairing file is written, but its durability is unconfirmed"
        );
    }
    Ok(())
}

fn generate(instance: &str) -> Result<(StoredPairing, [u8; 32]), PairingError> {
    let minted = mint(CERTIFICATE_NAME, Some(instance))?;
    let stored = StoredPairing {
        instance: instance.to_owned(),
        certificate_pem: minted.certificate_pem,
        key_pem: minted.key_pem,
        phones: Vec::new(),
    };
    Ok((stored, minted.fingerprint))
}

pub(in crate::daemon) struct Minted {
    pub(in crate::daemon) certificate_pem: String,
    pub(in crate::daemon) key_pem: String,
    pub(in crate::daemon) fingerprint: [u8; 32],
}

pub(in crate::daemon) fn mint(
    name: &str,
    common_name: Option<&str>,
) -> Result<Minted, rcgen::Error> {
    let mut params = rcgen::CertificateParams::new(vec![name.to_owned()])?;
    if let Some(common_name) = common_name {
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
    }
    params.not_after =
        time::OffsetDateTime::now_utc() + time::Duration::days(CERTIFICATE_LIFETIME_DAYS);
    let key = rcgen::KeyPair::generate()?;
    let certificate = params.self_signed(&key)?;
    Ok(Minted {
        certificate_pem: certificate.pem(),
        key_pem: key.serialize_pem(),
        fingerprint: fingerprint(certificate.der()),
    })
}

pub(super) fn random_hex(byte_count: usize) -> Result<String, PairingError> {
    let mut bytes = vec![0u8; byte_count];
    getrandom::fill(&mut bytes).map_err(PairingError::Random)?;
    Ok(to_hex(&bytes))
}

pub(in crate::daemon) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(in crate::daemon) fn fingerprint_from_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; 32];
    for (byte, digits) in bytes.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let digits = std::str::from_utf8(digits).ok()?;
        *byte = u8::from_str_radix(digits, 16).ok()?;
    }
    Some(bytes)
}

/// The fingerprint of the peer's certificate.
pub(in crate::daemon) fn presented<T>(request: &Request<T>) -> Option<[u8; 32]> {
    let certificates = request.peer_certs()?;
    certificates.first().map(|leaf| fingerprint(leaf))
}

pub(in crate::daemon) fn require_paired(
    phones: Arc<Phones>,
) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone + Send + Sync + 'static {
    move |request| match presented(&request) {
        Some(fingerprint) if phones.contains(&fingerprint) => Ok(request),
        _ => Err(crate::daemon::service::failed(
            "lan",
            RpcFailure::new(
                RpcErrorCode::Unauthenticated,
                "pair this device with the Mac first",
            ),
        )),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("the pairing file is not usable")]
    Document(#[from] DocumentError),

    #[error("the system random source failed")]
    Random(#[source] getrandom::Error),

    #[error("failed to mint the pairing certificate")]
    Certificate(#[from] rcgen::Error),

    #[error("this Mac already pairs {MAX_PHONES} phones")]
    Full,

    #[error("the pairing file is gone")]
    Missing,

    #[error("the pairing code is wrong, used, or expired")]
    CodeRefused,

    #[error("the phones are cut off, but the pairing file still pins them")]
    RevokeUnconfirmed(#[source] Box<Self>),
}

impl From<&PairingError> for RpcErrorCode {
    fn from(error: &PairingError) -> Self {
        match error {
            PairingError::Full => Self::PermissionDenied,
            PairingError::CodeRefused => Self::Unauthenticated,
            PairingError::RevokeUnconfirmed(_) => Self::DurabilityUnconfirmed,
            PairingError::Document(_)
            | PairingError::Random(_)
            | PairingError::Certificate(_)
            | PairingError::Missing => Self::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn fingerprints_round_trip_through_hex_and_reject_anything_else() {
        let fingerprint = [0xab; 32];
        assert_eq!(
            fingerprint_from_hex(&to_hex(&fingerprint)),
            Some(fingerprint)
        );
        assert_eq!(fingerprint_from_hex(&"AB".repeat(32)), Some(fingerprint));
        for bad in ["é".repeat(32), "ab".repeat(33), "+f".repeat(32)] {
            assert_eq!(fingerprint_from_hex(&bad), None, "{bad}");
        }
    }

    fn pairing_path(directory: &tempfile::TempDir) -> std::path::PathBuf {
        directory.path().join("phone").join("pairing.json")
    }

    fn mac() -> (tempfile::TempDir, std::path::PathBuf, Pairing) {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);
        let pairing = load_or_create(&path, "Aria (studio)").unwrap();
        (directory, path, pairing)
    }

    fn stored(path: &Path) -> StoredPairing {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn phone(index: u8) -> [u8; 32] {
        [index; 32]
    }

    #[test]
    fn first_load_mints_private_material_and_later_loads_keep_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);

        let first = load_or_create(&path, "Aria (studio)").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(first.certificate_pem.contains("BEGIN CERTIFICATE"));
        assert!(first.key_pem.contains("PRIVATE KEY"));
        assert!(stored(&path).phones.is_empty());

        let second = load_or_create(&path, "Aria (studio)").unwrap();
        assert_eq!(second.certificate_pem, first.certificate_pem);
    }

    #[test]
    fn a_renamed_mac_keeps_its_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);

        let before = load_or_create(&path, "Aria (old)").unwrap();
        before.phones.pair_with(phone(1), || true).unwrap();
        let after = load_or_create(&path, "Aria (new)").unwrap();

        assert_eq!(after.certificate_pem, before.certificate_pem);
        assert!(after.phones.contains(&phone(1)));
        assert_eq!(stored(&path).instance, "Aria (new)");
    }

    #[test]
    fn the_fingerprint_hashes_the_certificate_der() {
        let (_directory, _path, pairing) = mac();
        let der = CertificateDer::from_pem_slice(pairing.certificate_pem.as_bytes()).unwrap();
        assert_eq!(
            pairing.certificate_fingerprint,
            <[u8; 32]>::from(Sha256::digest(&der))
        );
    }

    #[test]
    fn a_paired_phone_survives_a_reload() {
        let (_directory, path, pairing) = mac();

        pairing.phones.pair_with(phone(1), || true).unwrap();
        pairing.phones.pair_with(phone(1), || true).unwrap();

        assert!(pairing.phones.contains(&phone(1)));
        assert!(!pairing.phones.contains(&phone(2)));
        let reloaded = load_or_create(&path, "Aria (studio)").unwrap();
        assert!(reloaded.phones.contains(&phone(1)));
        assert_eq!(stored(&path).phones, [phone(1)]);
    }

    #[test]
    fn a_code_pins_one_phone_and_counts_it() {
        let (_directory, _path, pairing) = mac();
        let (code, _) = pairing.phones.issue().unwrap();

        pairing.phones.pair(phone(1), code.raw()).unwrap();
        let reused = pairing.phones.pair(phone(2), code.raw());

        assert!(pairing.phones.contains(&phone(1)));
        assert!(matches!(reused, Err(PairingError::CodeRefused)));
        assert!(!pairing.phones.contains(&phone(2)));
        assert_eq!(pairing.phones.pairing_count(), 1);
    }

    #[test]
    fn revoking_ends_the_live_code() {
        let (_directory, _path, pairing) = mac();
        let (code, _) = pairing.phones.issue().unwrap();

        pairing.phones.revoke_all().unwrap();

        assert!(!pairing.phones.pairing_is_open());
        let refused = pairing.phones.pair(phone(1), code.raw());
        assert!(matches!(refused, Err(PairingError::CodeRefused)));
    }

    #[test]
    fn revoking_unpins_every_phone_and_keeps_the_certificate() {
        let (_directory, path, pairing) = mac();
        pairing.phones.pair_with(phone(1), || true).unwrap();
        pairing.phones.pair_with(phone(2), || true).unwrap();

        pairing.phones.revoke_all().unwrap();

        assert!(!pairing.phones.contains(&phone(1)));
        assert!(!pairing.phones.contains(&phone(2)));
        assert!(stored(&path).phones.is_empty());
        let reloaded = load_or_create(&path, "Aria (studio)").unwrap();
        assert_eq!(reloaded.certificate_pem, pairing.certificate_pem);
        assert!(!reloaded.phones.contains(&phone(1)));
    }

    #[test]
    fn revoking_ends_open_connections_but_not_later_ones() {
        let (_directory, _path, pairing) = mac();
        pairing.phones.pair_with(phone(1), || true).unwrap();
        let before = pairing.phones.connection(&phone(1)).unwrap();

        pairing.phones.revoke_all().unwrap();

        assert!(before.is_cancelled());
        assert!(pairing.phones.connection(&phone(1)).is_none());
        pairing.phones.pair_with(phone(1), || true).unwrap();
        let after = pairing.phones.connection(&phone(1)).unwrap();
        assert!(!after.is_cancelled());
    }

    #[test]
    fn a_revoke_racing_a_handshake_never_leaves_an_open_connection() {
        let (_directory, _path, pairing) = mac();
        pairing.phones.pair_with(phone(1), || true).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let phones = Arc::clone(&pairing.phones);
        let ready = Arc::clone(&barrier);
        let handshake = std::thread::spawn(move || {
            ready.wait();
            phones.connection(&phone(1))
        });

        barrier.wait();
        pairing.phones.revoke_all().unwrap();

        if let Some(connection) = handshake.join().unwrap() {
            assert!(connection.is_cancelled());
        }
        assert!(pairing.phones.connection(&phone(1)).is_none());
    }

    /// The revoke starts while the code is redeemed, before the phone is
    /// pinned. It must wait for the pin, then remove it.
    #[test]
    fn a_revoke_during_a_pairing_still_unpins_the_phone() {
        let (_directory, _path, pairing) = mac();
        let phones = Arc::clone(&pairing.phones);
        let mut revoking = None;

        pairing
            .phones
            .pair_with(phone(1), || {
                revoking = Some(std::thread::spawn(move || phones.revoke_all()));
                // Long enough for a revoke that ignored the lock to finish first.
                std::thread::sleep(std::time::Duration::from_millis(50));
                true
            })
            .unwrap();
        revoking.unwrap().join().unwrap().unwrap();

        assert!(!pairing.phones.contains(&phone(1)));
    }

    /// The test stands in for a revoke: it holds the write lock and retires
    /// the code. A pairing that redeemed before taking the lock would pin
    /// the phone once the lock is free.
    #[test]
    fn a_pairing_redeems_only_once_a_revoke_lets_go() {
        let (_directory, _path, pairing) = mac();
        let (code, _) = pairing.phones.issue().unwrap();
        let writing = pairing.phones.writing.lock().unwrap();
        let phones = Arc::clone(&pairing.phones);
        let pairer = std::thread::spawn(move || phones.pair(phone(1), code.raw()));
        // Long enough for a pairing that ignored the lock to redeem first.
        std::thread::sleep(std::time::Duration::from_millis(50));
        pairing.phones.codes.retire();
        drop(writing);

        let refused = pairer.join().unwrap();

        assert!(matches!(refused, Err(PairingError::CodeRefused)));
        assert!(!pairing.phones.contains(&phone(1)));
    }

    #[test]
    fn a_revoke_that_cannot_be_written_still_cuts_the_phones_off() {
        let (_directory, path, pairing) = mac();
        pairing.phones.pair_with(phone(1), || true).unwrap();
        let connection = pairing.phones.connection(&phone(1)).unwrap();
        // A file others can read is refused, so the revoke can't rewrite it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let refused = pairing.phones.revoke_all();

        assert!(matches!(refused, Err(PairingError::RevokeUnconfirmed(_))));
        assert_eq!(
            RpcErrorCode::from(&refused.unwrap_err()),
            RpcErrorCode::DurabilityUnconfirmed
        );
        assert!(connection.is_cancelled());
        assert!(!pairing.phones.contains(&phone(1)));
    }

    /// The next start mints new material over a damaged file.
    #[test]
    fn revoking_over_a_damaged_file_is_saved() {
        let (_directory, path, pairing) = mac();
        pairing.phones.pair_with(phone(1), || true).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        pairing.phones.revoke_all().unwrap();

        assert!(!pairing.phones.contains(&phone(1)));
        let reloaded = load_or_create(&path, "Aria (studio)").unwrap();
        assert!(!reloaded.phones.contains(&phone(1)));
    }

    #[test]
    fn pairing_after_an_unsaved_revoke_does_not_pin_the_revoked_phones_again() {
        let (_directory, path, pairing) = mac();
        pairing.phones.pair_with(phone(1), || true).unwrap();
        let mode = |bits| std::fs::Permissions::from_mode(bits);
        std::fs::set_permissions(&path, mode(0o644)).unwrap();
        let unsaved = pairing.phones.revoke_all();
        std::fs::set_permissions(&path, mode(0o600)).unwrap();
        assert!(matches!(unsaved, Err(PairingError::RevokeUnconfirmed(_))));
        assert_eq!(stored(&path).phones, [phone(1)], "the write did fail");

        pairing.phones.pair_with(phone(2), || true).unwrap();

        assert_eq!(stored(&path).phones, [phone(2)]);
    }

    #[test]
    fn pairing_past_the_cap_is_refused() {
        let (_directory, path, pairing) = mac();
        for index in 0..MAX_PHONES {
            pairing
                .phones
                .pair_with(phone(index as u8), || true)
                .unwrap();
        }

        let refused = pairing.phones.pair_with(phone(MAX_PHONES as u8), || true);

        assert!(matches!(refused, Err(PairingError::Full)));
        assert!(!pairing.phones.contains(&phone(MAX_PHONES as u8)));
        assert_eq!(stored(&path).phones.len(), MAX_PHONES);
    }

    #[test]
    fn a_bearer_era_file_is_replaced_with_fresh_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);
        let before = load_or_create(&path, "Aria (studio)").unwrap();
        let bearer_era = serde_json::json!({
            "instance": "Aria (studio)",
            "token": "a".repeat(64),
            "certificate_pem": before.certificate_pem,
            "key_pem": before.key_pem,
        });
        std::fs::write(&path, serde_json::to_vec(&bearer_era).unwrap()).unwrap();

        let after = load_or_create(&path, "Aria (studio)").unwrap();

        assert_ne!(after.certificate_pem, before.certificate_pem);
        assert!(stored(&path).phones.is_empty());
    }

    #[test]
    fn a_corrupt_certificate_is_replaced_with_fresh_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);
        let before = load_or_create(&path, "Aria (studio)").unwrap();

        let mut corrupt = stored(&path);
        corrupt.certificate_pem = "-----BEGIN CERTIFICATE-----\nnot base64!\n".to_owned();
        std::fs::write(&path, serde_json::to_vec(&corrupt).unwrap()).unwrap();

        let after = load_or_create(&path, "Aria (studio)").unwrap();
        assert!(after.certificate_pem.contains("BEGIN CERTIFICATE"));
        assert_ne!(
            after.certificate_fingerprint,
            before.certificate_fingerprint
        );
    }

    #[test]
    fn an_unparseable_file_is_replaced_with_fresh_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);
        load_or_create(&path, "Aria (studio)").unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        let after = load_or_create(&path, "Aria (studio)").unwrap();
        assert_eq!(stored(&path).certificate_pem, after.certificate_pem);
    }

    #[test]
    fn the_file_shape_is_stable() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);
        load_or_create(&path, "Aria (studio)").unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["certificate_pem", "instance", "key_pem", "phones"]);
    }
}
