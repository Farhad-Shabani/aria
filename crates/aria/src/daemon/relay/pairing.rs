//! This Mac's certificate, which phones pin, and the phones it pins in turn.
//! One private file holds both. Deleting it and restarting the daemon
//! revokes every phone.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tonic::{Request, Status};

use crate::primitives::error_chain;
use crate::primitives::private_document::{DocumentError, PrivateDocument};
use crate::rpc::{RpcErrorCode, RpcFailure};

/// Where the material lives under the user's home. The Mac app deletes this
/// file to revoke every phone, so its path is pinned by a test.
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

/// The phones this Mac pins, by certificate fingerprint. Once the daemon
/// runs, the only writer of the pairing file.
#[derive(Debug)]
pub(in crate::daemon) struct Phones {
    path: PathBuf,
    /// Held for the whole of a write. Handshakes read `paired`, so they
    /// never wait on the disk.
    writing: Mutex<()>,
    paired: Mutex<Vec<[u8; 32]>>,
}

impl Phones {
    pub(in crate::daemon) fn contains(&self, fingerprint: &[u8; 32]) -> bool {
        self.paired().contains(fingerprint)
    }

    /// Pins `fingerprint`, on disk before in memory. Pinning a paired phone
    /// again changes nothing.
    pub(in crate::daemon) fn admit(&self, fingerprint: [u8; 32]) -> Result<(), PairingError> {
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        {
            let paired = self.paired();
            if paired.contains(&fingerprint) {
                return Ok(());
            }
            if paired.len() >= MAX_PHONES {
                return Err(PairingError::Full);
            }
        }
        let document = PrivateDocument::new(&self.path, MAX_FILE_BYTES);
        let mut stored = document
            .read::<StoredPairing>()?
            .ok_or(PairingError::Missing)?;
        stored.phones.push(fingerprint);
        write(&document, &stored)?;
        let mut paired = self.paired();
        paired.push(fingerprint);
        tracing::info!(paired_phones = paired.len(), "paired a phone");
        Ok(())
    }

    // A push is the only change, and it either happened or did not, so a
    // poisoned lock still holds a usable list.
    fn paired(&self) -> std::sync::MutexGuard<'_, Vec<[u8; 32]>> {
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
            writing: Mutex::new(()),
            paired: Mutex::new(stored.phones),
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

pub(in crate::daemon) fn from_hex(value: &str) -> Option<[u8; 32]> {
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
        _ => {
            tracing::debug!("refused a request from a phone that is not paired");
            Err(RpcFailure::new(
                RpcErrorCode::Unauthenticated,
                "pair this device with the Mac first",
            )
            .into())
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PairingError {
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
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn fingerprints_round_trip_through_hex_and_reject_anything_else() {
        let fingerprint = [0xab; 32];
        assert_eq!(from_hex(&to_hex(&fingerprint)), Some(fingerprint));
        assert_eq!(from_hex(&"AB".repeat(32)), Some(fingerprint));
        for bad in ["é".repeat(32), "ab".repeat(33), "+f".repeat(32)] {
            assert_eq!(from_hex(&bad), None, "{bad}");
        }
    }

    fn pairing_path(directory: &tempfile::TempDir) -> std::path::PathBuf {
        directory.path().join("phone").join("pairing.json")
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
        before.phones.admit(phone(1)).unwrap();
        let after = load_or_create(&path, "Aria (new)").unwrap();

        assert_eq!(after.certificate_pem, before.certificate_pem);
        assert!(after.phones.contains(&phone(1)));
        assert_eq!(stored(&path).instance, "Aria (new)");
    }

    #[test]
    fn the_fingerprint_hashes_the_certificate_der() {
        let directory = tempfile::tempdir().unwrap();
        let pairing = load_or_create(&pairing_path(&directory), "Aria (studio)").unwrap();
        let der = CertificateDer::from_pem_slice(pairing.certificate_pem.as_bytes()).unwrap();
        assert_eq!(
            pairing.certificate_fingerprint,
            <[u8; 32]>::from(Sha256::digest(&der))
        );
    }

    #[test]
    fn a_paired_phone_survives_a_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);
        let pairing = load_or_create(&path, "Aria (studio)").unwrap();

        pairing.phones.admit(phone(1)).unwrap();
        pairing.phones.admit(phone(1)).unwrap();

        assert!(pairing.phones.contains(&phone(1)));
        assert!(!pairing.phones.contains(&phone(2)));
        let reloaded = load_or_create(&path, "Aria (studio)").unwrap();
        assert!(reloaded.phones.contains(&phone(1)));
        assert_eq!(stored(&path).phones, [phone(1)]);
    }

    #[test]
    fn pairing_past_the_cap_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);
        let pairing = load_or_create(&path, "Aria (studio)").unwrap();
        for index in 0..MAX_PHONES {
            pairing.phones.admit(phone(index as u8)).unwrap();
        }

        let refused = pairing.phones.admit(phone(MAX_PHONES as u8));

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

    /// The Mac app deletes this path to revoke every phone.
    #[test]
    fn the_file_shape_is_stable() {
        assert_eq!(RELATIVE_PATH, ".aria/phone/pairing.json");
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
