//! What a phone needs to reach this Mac's relay: a bearer token it
//! presents, and a certificate it pins. Both are minted once and kept in a
//! private file; deleting that file and restarting the daemon revokes every
//! paired phone at once.
//!
//! The certificate is what makes the token safe to send. Without it any host
//! advertising this Mac's name over Bonjour would be handed the token on the
//! phone's first call.

use std::fmt;
use std::path::Path;

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tonic::{Request, Status};

use crate::primitives::private_document::{DocumentError, PrivateDocument};
use crate::rpc::{RpcErrorCode, RpcFailure};

/// Where the material lives under the user's home. The Mac app deletes this
/// file to revoke every phone, so its path is pinned by a test.
pub(crate) const RELATIVE_PATH: &str = ".aria/phone/pairing.json";
const MAX_FILE_BYTES: u64 = 16 * 1024;
const TOKEN_BYTES: usize = 32;
/// The certificate's subject alternative name. The phone never checks it;
/// the pin is the check.
const CERTIFICATE_NAME: &str = "aria.local";
const CERTIFICATE_LIFETIME_DAYS: i64 = 3650;

#[derive(Clone)]
pub(in crate::daemon) struct Pairing {
    pub(in crate::daemon) token: Token,
    pub(in crate::daemon) certificate_pem: String,
    pub(in crate::daemon) key_pem: String,
    /// SHA-256 of the certificate's DER bytes, the value a phone pins.
    pub(in crate::daemon) certificate_fingerprint: [u8; 32],
}

/// 32 random bytes as lowercase hex. Its `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::daemon) struct Token(String);

impl Token {
    /// The value a client sends. The daemon hands it out only to a phone
    /// that redeems a pairing code.
    pub(in crate::daemon) fn raw(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(redacted)")
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPairing {
    instance: String,
    token: String,
    certificate_pem: String,
    key_pem: String,
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
                    ?error,
                    "kept the pairing, but could not record the new hostname"
                );
            }
            (stored, fingerprint)
        }
        None => {
            let (fresh, fingerprint) = generate(instance)?;
            write(&document, &fresh)?;
            (fresh, fingerprint)
        }
    };
    Ok(Pairing {
        token: Token(stored.token),
        certificate_fingerprint: fingerprint,
        certificate_pem: stored.certificate_pem,
        key_pem: stored.key_pem,
    })
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
                ?error,
                "the pairing file is damaged; minting new pairing material"
            );
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    if !is_well_formed(&stored.token) {
        tracing::warn!("the stored pairing token is malformed; minting new pairing material");
        return Ok(None);
    }
    match CertificateDer::from_pem_slice(stored.certificate_pem.as_bytes()) {
        Ok(der) => {
            let fingerprint = Sha256::digest(der).into();
            Ok(Some((stored, fingerprint)))
        }
        Err(error) => {
            tracing::warn!(
                ?error,
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
            ?error,
            "the pairing file is written, but its durability is unconfirmed"
        );
    }
    Ok(())
}

fn generate(instance: &str) -> Result<(StoredPairing, [u8; 32]), PairingError> {
    let token = random_hex(TOKEN_BYTES)?;

    let mut params = rcgen::CertificateParams::new(vec![CERTIFICATE_NAME.to_owned()])?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, instance);
    params.not_after =
        time::OffsetDateTime::now_utc() + time::Duration::days(CERTIFICATE_LIFETIME_DAYS);
    let key = rcgen::KeyPair::generate()?;
    let certificate = params.self_signed(&key)?;

    let stored = StoredPairing {
        instance: instance.to_owned(),
        token,
        certificate_pem: certificate.pem(),
        key_pem: key.serialize_pem(),
    };
    Ok((stored, Sha256::digest(certificate.der()).into()))
}

pub(super) fn random_hex(byte_count: usize) -> Result<String, PairingError> {
    let mut bytes = vec![0u8; byte_count];
    getrandom::fill(&mut bytes).map_err(PairingError::Random)?;
    Ok(to_hex(&bytes))
}

pub(super) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_well_formed(token: &str) -> bool {
    token.len() == TOKEN_BYTES * 2
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A tonic interceptor: the request carries `authorization: Bearer <token>`
/// or it is refused before any handler runs. The compare is constant-time;
/// the header value is never logged.
pub(in crate::daemon) fn require_bearer(
    token: Token,
) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone + Send + Sync + 'static {
    move |request| {
        let presented = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        match presented {
            Some(presented) if bool::from(presented.as_bytes().ct_eq(token.0.as_bytes())) => {
                Ok(request)
            }
            _ => Err(RpcFailure::new(
                RpcErrorCode::Unauthenticated,
                "pair this device with the Mac first",
            )
            .into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(in crate::daemon) enum PairingError {
    #[error("the pairing file is not usable")]
    Document(#[from] DocumentError),

    #[error("the system random source failed")]
    Random(#[source] getrandom::Error),

    #[error("failed to mint the pairing certificate")]
    Certificate(#[from] rcgen::Error),
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tonic::metadata::MetadataValue;

    use super::*;

    fn pairing_path(directory: &tempfile::TempDir) -> std::path::PathBuf {
        directory.path().join("phone").join("pairing.json")
    }

    #[test]
    fn first_load_mints_private_material_and_later_loads_keep_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);

        let first = load_or_create(&path, "Aria (studio)").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(is_well_formed(&first.token.0));
        assert!(first.certificate_pem.contains("BEGIN CERTIFICATE"));
        assert!(first.key_pem.contains("PRIVATE KEY"));

        let second = load_or_create(&path, "Aria (studio)").unwrap();
        assert_eq!(second.token, first.token);
        assert_eq!(second.certificate_pem, first.certificate_pem);
    }

    #[test]
    fn a_renamed_mac_keeps_its_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);

        let before = load_or_create(&path, "Aria (old)").unwrap();
        let after = load_or_create(&path, "Aria (new)").unwrap();

        assert_eq!(after.token, before.token);
        let stored: StoredPairing = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored.instance, "Aria (new)");
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
    fn a_malformed_token_is_replaced_with_fresh_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);
        let before = load_or_create(&path, "Aria (studio)").unwrap();

        let mut stored: StoredPairing =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        stored.token = "short".to_owned();
        std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

        let after = load_or_create(&path, "Aria (studio)").unwrap();
        assert!(is_well_formed(&after.token.0));
        assert_ne!(after.certificate_pem, before.certificate_pem);
    }

    #[test]
    fn a_corrupt_certificate_is_replaced_with_fresh_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = pairing_path(&directory);
        let before = load_or_create(&path, "Aria (studio)").unwrap();

        let mut stored: StoredPairing =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        stored.certificate_pem = "-----BEGIN CERTIFICATE-----\nnot base64!\n".to_owned();
        std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

        let after = load_or_create(&path, "Aria (studio)").unwrap();
        assert!(is_well_formed(&after.token.0));
        assert_ne!(after.token, before.token);
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
        assert!(is_well_formed(&after.token.0));
        let stored: StoredPairing = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored.token, after.token.0);
    }

    /// Files already on disk must keep loading.
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
        assert_eq!(keys, ["certificate_pem", "instance", "key_pem", "token"]);
    }

    fn request_with(header: Option<&str>) -> Request<()> {
        let mut request = Request::new(());
        if let Some(header) = header {
            request
                .metadata_mut()
                .insert("authorization", MetadataValue::try_from(header).unwrap());
        }
        request
    }

    #[test]
    fn the_bearer_check_admits_only_the_exact_token() {
        let token = Token("a".repeat(64));
        let mut check = require_bearer(token);

        let failure = check(request_with(None)).unwrap_err();
        assert_eq!(failure.code(), tonic::Code::Unauthenticated);
        assert_eq!(
            crate::ipc::protocol::rpc_failure(&failure).unwrap().code,
            RpcErrorCode::Unauthenticated
        );
        assert!(check(request_with(Some("Bearer bbbb"))).is_err());
        assert!(check(request_with(Some(&"a".repeat(64)))).is_err());
        assert!(check(request_with(Some(&format!("Bearer {}", "a".repeat(64))))).is_ok());
    }

    #[test]
    fn a_token_never_prints_itself() {
        assert_eq!(
            format!("{:?}", Token("secret".to_owned())),
            "Token(redacted)"
        );
    }
}
