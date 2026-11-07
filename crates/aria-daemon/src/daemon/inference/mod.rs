pub(crate) mod admin;
pub(crate) mod client;
pub(crate) mod engine;
pub(crate) mod host;
pub(crate) mod identity;
pub(crate) mod pairing;
pub(crate) mod tls;

use std::path::{Path, PathBuf};
use std::time::Duration;

use aria_platform::runtime::RuntimePaths;
use aria_settings::config::ServerEndpoint;
use aria_voice::config::PairingId;

/// Leaves room past the audio bound for the final decode.
pub(crate) const OVERALL_TIMEOUT: Duration = Duration::from_secs(720);
pub(crate) const IDLE_INPUT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const QUEUE_CAPACITY: usize = 16;
pub(crate) const OUTPUT_STALL_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const OUTPUT_POLL: Duration = Duration::from_millis(5);

/// Where inference keeps its private state under one home.
pub struct Paths {
    directory: PathBuf,
}

impl Paths {
    pub fn in_home(home: &Path) -> Self {
        Self {
            directory: home.join(".aria/inference"),
        }
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn client_identity(&self) -> PathBuf {
        self.directory.join("client.json")
    }

    pub(crate) fn server_identity(&self) -> PathBuf {
        self.directory.join("server.json")
    }

    /// With `pairing_lock`, serializes pairing on this machine.
    pub(crate) fn pairing_socket(&self) -> PathBuf {
        self.directory.join("client.sock")
    }

    pub(crate) fn pairing_lock(&self) -> PathBuf {
        self.directory.join("client.lock")
    }

    pub(crate) fn runtime(&self) -> RuntimePaths {
        RuntimePaths::in_directory(self.directory.join("runtime"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedServer {
    pub endpoint: ServerEndpoint,
    pub client_fingerprint: String,
}

pub fn paired_server(home: &Path) -> Result<Option<PairedServer>, identity::Error> {
    let path = Paths::in_home(home).client_identity();
    // Opening creates the private directory; an unpaired home stays untouched.
    if !path.is_file() {
        return Ok(None);
    }
    match identity::ClientIdentity::open(&path) {
        Ok(identity) => Ok(identity.paired_server()),
        Err(identity::Error::Unpaired) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The pairing as it stands now. Anything that keeps a transcriber from opening
/// it reads as no pairing.
pub(crate) fn pairing_id(home: &Path) -> Option<PairingId> {
    let path = Paths::in_home(home).client_identity();
    if !path.is_file() {
        return None;
    }
    let identity = identity::ClientIdentity::open(&path).ok()?;
    pairing_id_of(&identity).ok()
}

pub(crate) fn pairing_id_of(
    identity: &identity::ClientIdentity,
) -> Result<PairingId, identity::Error> {
    use sha2::Digest;

    let server = identity.pinned_server()?;
    // The endpoint is the one part of variable length, so it goes last.
    let id = sha2::Sha256::new()
        .chain_update(server.fingerprint)
        .chain_update(identity.fingerprint())
        .chain_update(server.endpoint.to_string())
        .finalize();
    Ok(PairingId::new(id.into()))
}

/// Stores a client identity pinned to `endpoint`, as pairing would, and
/// returns its fingerprint.
#[cfg(any(test, feature = "test-support"))]
pub fn pair_server_for_tests(home: &Path, endpoint: &str) -> String {
    use sha2::Digest;

    let params = rcgen::CertificateParams::new(vec!["aria.inference".to_owned()]).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let fingerprint: [u8; 32] = sha2::Sha256::digest(certificate.der()).into();
    let stored = serde_json::json!({
        "credentials": {
            "certificate_pem": certificate.pem(),
            "key_pem": key.serialize_pem(),
        },
        "server": {
            "endpoint": endpoint,
            "certificate_pem": certificate.pem(),
            "fingerprint": fingerprint,
        },
    });
    aria_primitives::private_fs::prepare_directory(&home.join(".aria")).unwrap();
    aria_primitives::private_document::PrivateDocument::new(
        Paths::in_home(home).client_identity(),
        identity::MAX_DOCUMENT_BYTES,
    )
    .prepare(&stored)
    .unwrap()
    .commit()
    .unwrap()
    .confirm_durability()
    .unwrap();
    fingerprint
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pairing_id_reads_as_it_stands_and_changes_with_each_pairing() {
        let home = tempfile::tempdir().unwrap();
        assert!(pairing_id(home.path()).is_none());

        pair_server_for_tests(home.path(), "https://example.test:9851");
        let first = pairing_id(home.path()).unwrap();
        pair_server_for_tests(home.path(), "https://example.test:9851");
        let second = pairing_id(home.path()).unwrap();
        assert!(first != second, "another server at the same address");

        replace_client_key(home.path());
        assert!(
            pairing_id(home.path()).unwrap() != second,
            "the same server, paired again with a new key"
        );
        std::fs::remove_file(Paths::in_home(home.path()).client_identity()).unwrap();
        assert!(pairing_id(home.path()).is_none());
    }

    /// Keeps the pinned server and gives this machine a new key, as
    /// forgetting and pairing the same server again would.
    fn replace_client_key(home: &Path) {
        let path = Paths::in_home(home).client_identity();
        let mut stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let params = rcgen::CertificateParams::new(vec!["aria.inference".to_owned()]).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        stored["credentials"] = serde_json::json!({
            "certificate_pem": params.self_signed(&key).unwrap().pem(),
            "key_pem": key.serialize_pem(),
        });
        aria_primitives::private_document::PrivateDocument::new(
            &path,
            identity::MAX_DOCUMENT_BYTES,
        )
        .prepare(&stored)
        .unwrap()
        .commit()
        .unwrap()
        .confirm_durability()
        .unwrap();
    }

    #[test]
    fn a_home_reports_the_server_it_holds_a_pin_for() {
        let home = tempfile::tempdir().unwrap();
        let aria = home.path().join(".aria");
        aria_primitives::private_fs::prepare_directory(&aria).unwrap();
        assert_eq!(paired_server(home.path()).unwrap(), None);
        assert!(
            !aria.join("inference").exists(),
            "looking leaves an unpaired home alone"
        );

        let hex = pair_server_for_tests(home.path(), "https://example.test:9851");
        assert_eq!(
            paired_server(home.path()).unwrap(),
            Some(PairedServer {
                endpoint: ServerEndpoint::parse("https://example.test:9851").unwrap(),
                client_fingerprint: hex,
            }),
            "the client signs with this certificate"
        );

        let path = Paths::in_home(home.path()).client_identity();
        std::fs::write(&path, "{").unwrap();
        assert!(
            paired_server(home.path()).is_err(),
            "a damaged pin is not hidden"
        );
    }
}
