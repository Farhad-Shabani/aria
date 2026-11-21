//! This Mac's certificate, which phones pin, and the phones it pins in turn.
//! One private file holds both. The certificate's key is a secret apart from
//! it. A second file keeps a record of each phone, so the Mac can list them.
//! It decides nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Status};

use aria_platform::secret::{Secret, SecretError};
use aria_primitives::error_chain;
use aria_primitives::private_document::{DocumentError, PrivateDocument};
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::sign::CertifiedKey;

use super::code::{PairingCode, PairingCodes};

pub(crate) const RELATIVE_PATH: &str = ".aria/phone/pairing.json";
/// Beside the pairing file. A separate file, because a daemon that predates
/// records refuses a pairing file with fields it doesn't know, and mints new
/// material over it.
const RECORDS_FILE_NAME: &str = "phones.json";
// As Apple names the key AirPlay pairs with: "AirPlay Server Identity".
const KEY_SERVICE: &str = "Aria Phone Pairing Identity";
const KEY_ACCOUNT: &str = "Aria";
/// Where the key is kept where there's no Keychain.
const KEY_FILE_NAME: &str = "pairing.key";
const MAX_FILE_BYTES: u64 = 16 * 1024;
const MAX_PHONES: usize = 16;
const MAX_HARDWARE_MODEL_BYTES: usize = 32;
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
    records: PathBuf,
    codes: PairingCodes,
    /// Held for the whole of a write. Handshakes read `paired`, so they
    /// never wait on the disk.
    writing: Mutex<()>,
    paired: Mutex<Paired>,
}

#[derive(Debug)]
struct Paired {
    /// In the order they paired.
    phones: Vec<PairedPhone>,
    /// What ends each pinned phone's connections: its revoke cancels it.
    connections: HashMap<[u8; 32], CancellationToken>,
    /// What ends connections that opened before their phone was pinned.
    /// Which phone pairs on one is unknown, so any revoke cancels it.
    unpinned: CancellationToken,
}

impl Paired {
    fn pins(&self, fingerprint: &[u8; 32]) -> bool {
        self.phones
            .iter()
            .any(|phone| &phone.fingerprint == fingerprint)
    }
}

/// A phone this Mac pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairedPhone {
    pub(crate) fingerprint: [u8; 32],
    /// `None` for a phone that paired before the Mac kept records.
    pub(crate) record: Option<PhoneRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhoneRecord {
    /// `None` when the phone didn't say.
    pub(crate) hardware_model: Option<HardwareModel>,
    pub(crate) paired_at: SystemTime,
}

/// A phone's model identifier, as `uname` reports it, such as `iPhone18,1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct HardwareModel(String);

impl HardwareModel {
    /// `None` for an empty identifier, which a phone sends when it doesn't
    /// say.
    pub(in crate::daemon) fn from_identifier(value: String) -> Result<Option<Self>, PairingError> {
        if value.is_empty() {
            return Ok(None);
        }
        Self::try_from(value).map(Some)
    }
}

impl TryFrom<String> for HardwareModel {
    type Error = PairingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let is_valid = !value.is_empty()
            && value.len() <= MAX_HARDWARE_MODEL_BYTES
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b',');
        if !is_valid {
            return Err(PairingError::HardwareModelRefused);
        }
        Ok(Self(value))
    }
}

impl From<HardwareModel> for String {
    fn from(model: HardwareModel) -> Self {
        model.0
    }
}

impl Phones {
    pub(in crate::daemon) fn contains(&self, fingerprint: &[u8; 32]) -> bool {
        self.paired().pins(fingerprint)
    }

    /// What ends a paired phone's connection: cancelled when the phone is
    /// revoked. `None` for a phone this Mac does not pin.
    pub(in crate::daemon) fn connection(
        &self,
        fingerprint: &[u8; 32],
    ) -> Option<CancellationToken> {
        let mut paired = self.paired();
        if !paired.pins(fingerprint) {
            return None;
        }
        Some(paired.connections.entry(*fingerprint).or_default().clone())
    }

    /// What ends a connection that opened before its phone was pinned: the
    /// next revoke of any phone.
    pub(in crate::daemon) fn unpinned_connection(&self) -> CancellationToken {
        self.paired().unpinned.clone()
    }

    /// The phones this Mac pins, in the order they paired.
    pub(in crate::daemon) fn list(&self) -> Vec<PairedPhone> {
        self.paired().phones.clone()
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
        hardware_model: Option<HardwareModel>,
    ) -> Result<(), PairingError> {
        self.pair_with(fingerprint, hardware_model, || {
            self.codes.redeem(code, Instant::now())
        })
    }

    /// Redeems under the write lock, so a revoke either sees the phone
    /// pinned or retires the code before it is redeemed.
    fn pair_with(
        &self,
        fingerprint: [u8; 32],
        hardware_model: Option<HardwareModel>,
        redeem: impl FnOnce() -> bool,
    ) -> Result<(), PairingError> {
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        if !redeem() {
            return Err(PairingError::CodeRefused);
        }
        let mut phones = {
            let paired = self.paired();
            if paired.pins(&fingerprint) {
                self.codes.record_pairing();
                return Ok(());
            }
            if paired.phones.len() >= MAX_PHONES {
                return Err(PairingError::Full);
            }
            paired.phones.clone()
        };
        phones.push(PairedPhone {
            fingerprint,
            record: Some(PhoneRecord {
                hardware_model,
                paired_at: SystemTime::now(),
            }),
        });
        let document = PrivateDocument::new(&self.path, MAX_FILE_BYTES);
        let mut stored = document
            .read::<StoredPairing>()?
            .ok_or(PairingError::Missing)?;
        // From memory, not the file: a revoke whose write failed left the
        // file pinning phones this daemon no longer trusts.
        stored.phones = phones.iter().map(|phone| phone.fingerprint).collect();
        write(&document, &stored)?;
        self.write_records(&phones);
        let mut paired = self.paired();
        // The write lock kept every other change out since the clone.
        paired.phones = phones;
        self.codes.record_pairing();
        tracing::info!(paired_phones = paired.phones.len(), "paired a phone");
        Ok(())
    }

    /// Unpins one phone and closes its connections, and those still
    /// pairing, in memory before on disk, as `revoke_all` does. The live
    /// code stays, for the other phones. A phone this Mac doesn't pin
    /// changes nothing.
    pub(in crate::daemon) fn revoke(&self, fingerprint: &[u8; 32]) -> Result<(), PairingError> {
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let phones = {
            let mut paired = self.paired();
            if !paired.pins(fingerprint) {
                return Ok(());
            }
            paired
                .phones
                .retain(|phone| &phone.fingerprint != fingerprint);
            if let Some(connection) = paired.connections.remove(fingerprint) {
                connection.cancel();
            }
            std::mem::take(&mut paired.unpinned).cancel();
            paired.phones.clone()
        };
        self.store(&phones)
            .map_err(|error| PairingError::RevokeUnconfirmed(Box::new(error)))?;
        tracing::info!(paired_phones = phones.len(), "revoked a phone");
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
            for connection in std::mem::take(&mut paired.connections).into_values() {
                connection.cancel();
            }
            std::mem::take(&mut paired.unpinned).cancel();
            std::mem::take(&mut paired.phones).len()
        };
        self.store(&[])
            .map_err(|error| PairingError::RevokeUnconfirmed(Box::new(error)))?;
        tracing::info!(revoked_phones = revoked, "revoked every phone");
        Ok(())
    }

    /// Pins only `phones` on disk. A file that is gone or damaged pins
    /// nobody, since the next start mints new material over it, so there is
    /// nothing to rewrite.
    fn store(&self, phones: &[PairedPhone]) -> Result<(), PairingError> {
        let document = PrivateDocument::new(&self.path, MAX_FILE_BYTES);
        let mut stored = match document.read::<StoredPairing>() {
            Ok(Some(stored)) => stored,
            Ok(None) | Err(DocumentError::Parse(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        stored.phones = phones.iter().map(|phone| phone.fingerprint).collect();
        write(&document, &stored)?;
        self.write_records(phones);
        Ok(())
    }

    /// A failed write only leaves the phones without records after a restart, so
    /// it is logged, not returned.
    fn write_records(&self, phones: &[PairedPhone]) {
        let stored = StoredRecords {
            phones: phones
                .iter()
                .filter_map(|phone| {
                    let record = phone.record.as_ref()?;
                    Some(StoredRecord {
                        fingerprint: phone.fingerprint,
                        hardware_model: record.hardware_model.clone(),
                        paired_at_ms: record
                            .paired_at
                            .duration_since(UNIX_EPOCH)
                            .map_or(0, |since| {
                                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
                            }),
                    })
                })
                .collect(),
        };
        let document = PrivateDocument::new(&self.records, MAX_FILE_BYTES);
        if let Err(error) = write(&document, &stored) {
            tracing::warn!(
                error = %error_chain(&error),
                "the phones are saved, but their records are not"
            );
        }
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
    phones: Vec<[u8; 32]>,
}

struct Material {
    stored: StoredPairing,
    key_pem: String,
    fingerprint: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecords {
    phones: Vec<StoredRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    fingerprint: [u8; 32],
    hardware_model: Option<HardwareModel>,
    paired_at_ms: u64,
}

/// Each pinned phone, with its record where the records file has one. A
/// record for a phone the pairing file doesn't pin is dropped. A records file
/// that is gone or unusable leaves every phone without one.
fn with_records(pins: Vec<[u8; 32]>, path: &Path) -> Vec<PairedPhone> {
    let records = match PrivateDocument::new(path, MAX_FILE_BYTES).read::<StoredRecords>() {
        Ok(Some(stored)) => stored.phones,
        Ok(None) => Vec::new(),
        Err(error) => {
            tracing::warn!(
                error = %error_chain(&error),
                "the phone records are unusable; listing the phones without them"
            );
            Vec::new()
        }
    };
    pins.into_iter()
        .map(|fingerprint| PairedPhone {
            fingerprint,
            record: records
                .iter()
                .find(|record| record.fingerprint == fingerprint)
                .map(|record| PhoneRecord {
                    hardware_model: record.hardware_model.clone(),
                    paired_at: UNIX_EPOCH + Duration::from_millis(record.paired_at_ms),
                }),
        })
        .collect()
}

/// Reads the material, minting it on first use. A changed hostname rewrites
/// the stored name and keeps everything else.
pub(in crate::daemon) fn load_or_create(
    path: &Path,
    instance: &str,
) -> Result<Pairing, PairingError> {
    let document = PrivateDocument::new(path, MAX_FILE_BYTES);
    let secret =
        crate::daemon::secret::secret(KEY_SERVICE, KEY_ACCOUNT, path.with_file_name(KEY_FILE_NAME));
    let material = match read_usable(&document, &secret)? {
        Some(mut material) => {
            if material.stored.instance != instance {
                material.stored.instance = instance.to_owned();
                // The material still works; the stored name is only a label.
                if let Err(error) = write(&document, &material.stored) {
                    tracing::warn!(
                        error = %error_chain(&error),
                        "kept the pairing, but could not record the new hostname"
                    );
                }
            }
            material
        }
        None => {
            let fresh = generate(instance)?;
            // Before the file, so a file never names a key that isn't kept.
            secret.write(&fresh.key_pem)?;
            write(&document, &fresh.stored)?;
            tracing::info!("minted this Mac's pairing material");
            fresh
        }
    };
    let records = path.with_file_name(RECORDS_FILE_NAME);
    let phones = with_records(material.stored.phones, &records);
    Ok(Pairing {
        certificate_pem: material.stored.certificate_pem,
        key_pem: material.key_pem,
        certificate_fingerprint: material.fingerprint,
        phones: Arc::new(Phones {
            path: path.to_owned(),
            records,
            codes: PairingCodes::default(),
            writing: Mutex::new(()),
            paired: Mutex::new(Paired {
                phones,
                connections: HashMap::new(),
                unpinned: CancellationToken::new(),
            }),
        }),
    })
}

/// SHA-256 of a certificate's DER bytes: how either side names the other.
pub(in crate::daemon) fn fingerprint(der: &[u8]) -> [u8; 32] {
    Sha256::digest(der).into()
}

/// The stored material. `None` when there is none, it is damaged, or its key
/// is gone or another certificate's: no phone can use such material, so
/// replacing it revokes nothing. A file from before the key became a secret
/// reads as damaged. An unreadable file or secret still fails closed.
fn read_usable(
    document: &PrivateDocument,
    secret: &Secret,
) -> Result<Option<Material>, PairingError> {
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
    let certificate = match CertificateDer::from_pem_slice(stored.certificate_pem.as_bytes()) {
        Ok(der) => der,
        Err(error) => {
            tracing::warn!(
                error = %error_chain(&error),
                "the stored pairing certificate is not PEM; minting new pairing material"
            );
            return Ok(None);
        }
    };
    let Some(key_pem) = secret.read()? else {
        tracing::warn!("the pairing key is gone; minting new pairing material");
        return Ok(None);
    };
    let fingerprint = fingerprint(&certificate);
    // A crash between writing the key and the file, or a restored backup,
    // leaves a key that belongs to another certificate.
    if !keys_match(certificate, &key_pem) {
        tracing::warn!("the pairing key is not this certificate's; minting new pairing material");
        return Ok(None);
    }
    Ok(Some(Material {
        stored,
        key_pem,
        fingerprint,
    }))
}

fn keys_match(certificate: CertificateDer<'static>, key_pem: &str) -> bool {
    let Ok(key) = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()) else {
        return false;
    };
    CertifiedKey::from_der(vec![certificate], key, &ring::default_provider())
        .and_then(|certified| certified.keys_match())
        .is_ok()
}

fn write(document: &PrivateDocument, stored: &impl Serialize) -> Result<(), PairingError> {
    let committed = document.prepare(stored)?.commit()?;
    // The rename already landed, so the material is on disk either way.
    if let Err(error) = committed.confirm_durability() {
        tracing::warn!(
            error = %error_chain(&error),
            "a pairing file is written, but its durability is unconfirmed"
        );
    }
    Ok(())
}

fn generate(instance: &str) -> Result<Material, PairingError> {
    let minted = mint(CERTIFICATE_NAME, Some(instance))?;
    Ok(Material {
        stored: StoredPairing {
            instance: instance.to_owned(),
            certificate_pem: minted.certificate_pem,
            phones: Vec::new(),
        },
        key_pem: minted.key_pem,
        fingerprint: minted.fingerprint,
    })
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

    #[error("the pairing key is not usable")]
    Key(#[from] SecretError),

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

    #[error(
        "the hardware model must be up to {MAX_HARDWARE_MODEL_BYTES} ASCII letters, digits, and commas"
    )]
    HardwareModelRefused,
}

impl From<&PairingError> for RpcErrorCode {
    fn from(error: &PairingError) -> Self {
        match error {
            PairingError::Full => Self::PermissionDenied,
            PairingError::CodeRefused => Self::Unauthenticated,
            PairingError::RevokeUnconfirmed(_) => Self::DurabilityUnconfirmed,
            PairingError::HardwareModelRefused => Self::InvalidRequest,
            PairingError::Document(_)
            | PairingError::Key(_)
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
        before.phones.pair_with(phone(1), None, || true).unwrap();
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

        pairing.phones.pair_with(phone(1), None, || true).unwrap();
        pairing.phones.pair_with(phone(1), None, || true).unwrap();

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

        pairing.phones.pair(phone(1), code.raw(), None).unwrap();
        let reused = pairing.phones.pair(phone(2), code.raw(), None);

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
        let refused = pairing.phones.pair(phone(1), code.raw(), None);
        assert!(matches!(refused, Err(PairingError::CodeRefused)));
    }

    #[test]
    fn revoking_unpins_every_phone_and_keeps_the_certificate() {
        let (_directory, path, pairing) = mac();
        pairing.phones.pair_with(phone(1), None, || true).unwrap();
        pairing.phones.pair_with(phone(2), None, || true).unwrap();

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
        pairing.phones.pair_with(phone(1), None, || true).unwrap();
        let before = pairing.phones.connection(&phone(1)).unwrap();

        pairing.phones.revoke_all().unwrap();

        assert!(before.is_cancelled());
        assert!(pairing.phones.connection(&phone(1)).is_none());
        pairing.phones.pair_with(phone(1), None, || true).unwrap();
        let after = pairing.phones.connection(&phone(1)).unwrap();
        assert!(!after.is_cancelled());
    }

    #[test]
    fn a_revoke_racing_a_handshake_never_leaves_an_open_connection() {
        let (_directory, _path, pairing) = mac();
        pairing.phones.pair_with(phone(1), None, || true).unwrap();
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
            .pair_with(phone(1), None, || {
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
        let pairer = std::thread::spawn(move || phones.pair(phone(1), code.raw(), None));
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
        pairing.phones.pair_with(phone(1), None, || true).unwrap();
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
        pairing.phones.pair_with(phone(1), None, || true).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        pairing.phones.revoke_all().unwrap();

        assert!(!pairing.phones.contains(&phone(1)));
        let reloaded = load_or_create(&path, "Aria (studio)").unwrap();
        assert!(!reloaded.phones.contains(&phone(1)));
    }

    #[test]
    fn pairing_after_an_unsaved_revoke_does_not_pin_the_revoked_phones_again() {
        let (_directory, path, pairing) = mac();
        pairing.phones.pair_with(phone(1), None, || true).unwrap();
        let mode = |bits| std::fs::Permissions::from_mode(bits);
        std::fs::set_permissions(&path, mode(0o644)).unwrap();
        let unsaved = pairing.phones.revoke_all();
        std::fs::set_permissions(&path, mode(0o600)).unwrap();
        assert!(matches!(unsaved, Err(PairingError::RevokeUnconfirmed(_))));
        assert_eq!(stored(&path).phones, [phone(1)], "the write did fail");

        pairing.phones.pair_with(phone(2), None, || true).unwrap();

        assert_eq!(stored(&path).phones, [phone(2)]);
    }

    #[test]
    fn pairing_past_the_cap_is_refused() {
        let (_directory, path, pairing) = mac();
        for index in 0..MAX_PHONES {
            pairing
                .phones
                .pair_with(phone(index as u8), None, || true)
                .unwrap();
        }

        let refused = pairing
            .phones
            .pair_with(phone(MAX_PHONES as u8), None, || true);

        assert!(matches!(refused, Err(PairingError::Full)));
        assert!(!pairing.phones.contains(&phone(MAX_PHONES as u8)));
        assert_eq!(stored(&path).phones.len(), MAX_PHONES);
    }

    #[test]
    fn a_damaged_file_is_replaced_with_fresh_material() {
        type Damage = fn(&Path, &Pairing);
        let rows: [(&str, Damage); 4] = [
            ("a bearer-era file", |path, before| {
                let bearer_era = serde_json::json!({
                    "instance": "Aria (studio)",
                    "token": "a".repeat(64),
                    "certificate_pem": before.certificate_pem,
                    "key_pem": before.key_pem,
                });
                std::fs::write(path, serde_json::to_vec(&bearer_era).unwrap()).unwrap();
            }),
            ("a file that still holds its key", |path, before| {
                let mut held = serde_json::to_value(stored(path)).unwrap();
                held["key_pem"] = serde_json::json!(before.key_pem);
                std::fs::write(path, serde_json::to_vec(&held).unwrap()).unwrap();
            }),
            ("a corrupt certificate", |path, _| {
                let mut corrupt = stored(path);
                corrupt.certificate_pem = "-----BEGIN CERTIFICATE-----\nnot base64!\n".to_owned();
                std::fs::write(path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
            }),
            ("an unparseable file", |path, _| {
                std::fs::write(path, b"{ not json").unwrap();
            }),
        ];

        for (label, damage) in rows {
            let (_directory, path, before) = mac();
            damage(&path, &before);

            let after = load_or_create(&path, "Aria (studio)").unwrap();

            assert_ne!(after.certificate_pem, before.certificate_pem, "{label}");
            assert!(
                after.certificate_pem.contains("BEGIN CERTIFICATE"),
                "{label}"
            );
            assert_ne!(
                after.certificate_fingerprint, before.certificate_fingerprint,
                "{label}"
            );
            assert_eq!(
                stored(&path).certificate_pem,
                after.certificate_pem,
                "{label}"
            );
            assert!(stored(&path).phones.is_empty(), "{label}");
        }
    }

    fn model(identifier: &str) -> Option<HardwareModel> {
        HardwareModel::from_identifier(identifier.to_owned()).unwrap()
    }

    #[test]
    fn revoking_one_phone_ends_only_its_connections() {
        let (_directory, path, pairing) = mac();
        pairing.phones.pair_with(phone(1), None, || true).unwrap();
        pairing.phones.pair_with(phone(2), None, || true).unwrap();
        let first = pairing.phones.connection(&phone(1)).unwrap();
        let second = pairing.phones.connection(&phone(2)).unwrap();

        pairing.phones.revoke(&phone(1)).unwrap();

        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        assert!(!pairing.phones.contains(&phone(1)));
        assert!(pairing.phones.contains(&phone(2)));
        assert_eq!(stored(&path).phones, [phone(2)]);
    }

    #[test]
    fn any_revoke_ends_connections_still_pairing() {
        type Revoke = fn(&Phones);
        let rows: [(&str, Revoke); 2] = [
            ("one", |phones| phones.revoke(&phone(1)).unwrap()),
            ("all", |phones| phones.revoke_all().unwrap()),
        ];
        for (label, revoke) in rows {
            let (_directory, _path, pairing) = mac();
            let pairing_connection = pairing.phones.unpinned_connection();
            pairing.phones.pair_with(phone(1), None, || true).unwrap();

            revoke(&pairing.phones);

            assert!(pairing_connection.is_cancelled(), "{label}");
            assert!(
                !pairing.phones.unpinned_connection().is_cancelled(),
                "{label}"
            );
        }
    }

    #[test]
    fn revoking_one_phone_keeps_the_live_code() {
        let (_directory, _path, pairing) = mac();
        let (code, _) = pairing.phones.issue().unwrap();
        pairing.phones.pair_with(phone(1), None, || true).unwrap();

        pairing.phones.revoke(&phone(1)).unwrap();

        pairing.phones.pair(phone(2), code.raw(), None).unwrap();
        assert!(pairing.phones.contains(&phone(2)));
    }

    #[test]
    fn a_paired_phone_keeps_its_model_and_date_across_a_reload() {
        let (_directory, path, pairing) = mac();
        let before = SystemTime::now() - Duration::from_millis(1);
        pairing
            .phones
            .pair_with(phone(1), model("iPhone18,1"), || true)
            .unwrap();
        pairing.phones.pair_with(phone(2), None, || true).unwrap();
        let after = SystemTime::now();

        let listed = load_or_create(&path, "Aria (studio)")
            .unwrap()
            .phones
            .list();

        let fingerprints: Vec<_> = listed.iter().map(|phone| phone.fingerprint).collect();
        assert_eq!(fingerprints, [phone(1), phone(2)]);
        let first = listed[0].record.clone().unwrap();
        assert_eq!(first.hardware_model, model("iPhone18,1"));
        assert!(before <= first.paired_at && first.paired_at <= after);
        assert_eq!(listed[1].record.clone().unwrap().hardware_model, None);
    }

    #[test]
    fn a_revoked_phone_pairs_again_with_a_new_record() {
        let (_directory, path, pairing) = mac();
        pairing
            .phones
            .pair_with(phone(1), model("iPhone15,2"), || true)
            .unwrap();
        pairing.phones.revoke(&phone(1)).unwrap();
        pairing
            .phones
            .pair_with(phone(1), model("iPhone18,1"), || true)
            .unwrap();

        let listed = load_or_create(&path, "Aria (studio)")
            .unwrap()
            .phones
            .list();

        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].record.clone().unwrap().hardware_model,
            model("iPhone18,1")
        );
    }

    #[test]
    fn phones_without_usable_records_still_list() {
        type Damage = fn(&Path);
        let rows: [(&str, Damage); 2] = [
            ("no records file", |records| {
                std::fs::remove_file(records).unwrap();
            }),
            ("a damaged records file", |records| {
                std::fs::write(records, b"{ not json").unwrap();
            }),
        ];
        for (label, damage) in rows {
            let (_directory, path, pairing) = mac();
            pairing
                .phones
                .pair_with(phone(1), model("iPhone18,1"), || true)
                .unwrap();
            damage(&path.with_file_name(RECORDS_FILE_NAME));

            let listed = load_or_create(&path, "Aria (studio)")
                .unwrap()
                .phones
                .list();

            assert_eq!(listed.len(), 1, "{label}");
            assert_eq!(listed[0].fingerprint, phone(1), "{label}");
            assert_eq!(listed[0].record, None, "{label}");
        }
    }

    #[test]
    fn a_hardware_model_is_up_to_32_ascii_letters_digits_and_commas() {
        for bad in ["iPhone 18", "iPhone18,1\n", "é", &"x".repeat(33)] {
            let refused = HardwareModel::from_identifier(bad.to_owned());
            assert!(
                matches!(refused, Err(PairingError::HardwareModelRefused)),
                "{bad:?}"
            );
        }

        assert_eq!(HardwareModel::from_identifier(String::new()).unwrap(), None);
        assert!(
            HardwareModel::from_identifier("x".repeat(32))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn revoking_a_phone_the_mac_does_not_pin_changes_nothing() {
        let (_directory, path, pairing) = mac();
        pairing.phones.pair_with(phone(1), None, || true).unwrap();
        let pairing_connection = pairing.phones.unpinned_connection();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();

        pairing.phones.revoke(&phone(2)).unwrap();

        assert!(!pairing_connection.is_cancelled());
        assert!(pairing.phones.contains(&phone(1)));
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            written
        );
    }

    /// The key is a secret apart from the file, so the file holds no key.
    #[test]
    fn the_file_shape_is_stable() {
        let (_directory, path, pairing) = mac();
        pairing
            .phones
            .pair_with(phone(1), model("iPhone18,1"), || true)
            .unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["certificate_pem", "instance", "phones"]);
    }

    fn key_path(path: &Path) -> std::path::PathBuf {
        path.with_file_name(KEY_FILE_NAME)
    }

    #[test]
    fn a_lost_key_mints_new_material() {
        let (_directory, path, before) = mac();
        std::fs::remove_file(key_path(&path)).unwrap();

        let after = load_or_create(&path, "Aria (studio)").unwrap();

        assert_ne!(
            after.certificate_fingerprint,
            before.certificate_fingerprint
        );
    }

    #[test]
    fn another_certificates_key_mints_new_material() {
        let (_directory, path, before) = mac();
        let other = mint(CERTIFICATE_NAME, None).unwrap();
        std::fs::write(key_path(&path), serde_json::to_vec(&other.key_pem).unwrap()).unwrap();

        let after = load_or_create(&path, "Aria (studio)").unwrap();

        assert_ne!(
            after.certificate_fingerprint,
            before.certificate_fingerprint
        );
        assert!(after.key_pem != other.key_pem);
    }

    #[test]
    fn an_unreadable_key_fails_closed_and_mints_nothing() {
        let (_directory, path, before) = mac();
        before.phones.pair_with(phone(1), None, || true).unwrap();
        let file = std::fs::read(&path).unwrap();
        std::fs::remove_file(key_path(&path)).unwrap();
        std::fs::create_dir(key_path(&path)).unwrap();

        assert!(matches!(
            load_or_create(&path, "Aria (studio)"),
            Err(PairingError::Key(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), file);
    }
}
