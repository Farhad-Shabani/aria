pub(crate) mod client;
pub(crate) mod engine;
pub(crate) mod host;
pub(crate) mod identity;
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

    /// Serializes pairing on this machine.
    pub(crate) fn pairing_lock(&self) -> (PathBuf, PathBuf) {
        (
            self.directory.join("client.sock"),
            self.directory.join("client.lock"),
        )
    }

    pub(crate) fn runtime(&self) -> RuntimePaths {
        RuntimePaths::in_directory(self.directory.join("runtime"))
    }
}

/// Whether this home holds a pin for `endpoint`.
pub(crate) fn is_paired_with_server(home: &Path, endpoint: &ServerEndpoint) -> bool {
    let path = Paths::in_home(home).client_identity();
    path.is_file()
        && identity::ClientIdentity::open(&path)
            .is_ok_and(|identity| identity.paired(endpoint).is_ok())
}
