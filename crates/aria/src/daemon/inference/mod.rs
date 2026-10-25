pub(crate) mod admin;
pub(crate) mod client;
pub(crate) mod engine;
pub(crate) mod host;
pub(crate) mod identity;
pub(crate) mod pairing;
pub(crate) mod tls;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::ServerEndpoint;
use crate::runtime::RuntimePaths;

/// Leaves room past the audio bound for the final decode.
pub(crate) const OVERALL_TIMEOUT: Duration = Duration::from_secs(720);
pub(crate) const IDLE_INPUT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const QUEUE_CAPACITY: usize = 16;
pub(crate) const OUTPUT_STALL_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const OUTPUT_POLL: Duration = Duration::from_millis(5);

/// Where inference keeps its private state under one home.
pub(crate) struct Paths {
    directory: PathBuf,
}

impl Paths {
    pub(crate) fn in_home(home: &Path) -> Self {
        Self {
            directory: home.join(".aria/inference"),
        }
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn client_identity(&self) -> PathBuf {
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
pub(crate) struct PairedServer {
    pub(crate) endpoint: ServerEndpoint,
    pub(crate) client_fingerprint: String,
}

pub(crate) fn paired_server(home: &Path) -> Result<Option<PairedServer>, identity::Error> {
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

/// Stores a client identity pinned to `endpoint`, as pairing would, and
/// returns its fingerprint.
#[cfg(test)]
pub(crate) fn pin_server_for_tests(home: &Path, endpoint: &str) -> String {
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
    crate::primitives::private_fs::prepare_directory(&home.join(".aria")).unwrap();
    crate::primitives::private_document::PrivateDocument::new(
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
    fn a_home_reports_the_server_it_holds_a_pin_for() {
        let home = tempfile::tempdir().unwrap();
        let aria = home.path().join(".aria");
        crate::primitives::private_fs::prepare_directory(&aria).unwrap();
        assert_eq!(paired_server(home.path()).unwrap(), None);
        assert!(
            !aria.join("inference").exists(),
            "looking leaves an unpaired home alone"
        );

        let hex = pin_server_for_tests(home.path(), "https://example.test:9851");
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
