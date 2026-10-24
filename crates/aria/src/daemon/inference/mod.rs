pub(crate) mod client;
pub(crate) mod engine;
pub(crate) mod host;
pub(crate) mod identity;
pub(crate) mod tls;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::ServerEndpoint;
use crate::runtime::RuntimePaths;
use crate::transcription::{Language, TranscriptUpdate};

/// Uploads are mono PCM16 at this rate, so sample positions are wire units.
const SAMPLE_RATE: u64 = 16_000;
pub(crate) const MAX_AUDIO_SECONDS: u64 = 600;
pub(crate) const MAX_AUDIO_SAMPLES: u64 = MAX_AUDIO_SECONDS * SAMPLE_RATE;
pub(crate) const MAX_AUDIO_BYTES: usize = MAX_AUDIO_SAMPLES as usize * 2;
pub(crate) const MAX_AUDIO_MESSAGE_BYTES: usize = 256 * 1024;
/// Bounds one provisional, one finalized segment, and the cumulative transcript.
pub(crate) const MAX_TEXT_BYTES: usize = 128 * 1024;
/// An update can carry a segment and the completed transcript together.
pub(crate) const MAX_RESPONSE_BYTES: usize = 2 * MAX_TEXT_BYTES + 1024;
pub(crate) const MAX_ENGINE_NAME_BYTES: usize = 64;
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
pub(crate) fn is_paired(home: &Path, endpoint: &ServerEndpoint) -> bool {
    let path = Paths::in_home(home).client_identity();
    path.is_file()
        && identity::ClientIdentity::open(&path)
            .is_ok_and(|identity| identity.paired(endpoint).is_ok())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Strategy {
    Whole,
    Streaming,
}

pub(crate) struct Start {
    pub engine: String,
    pub language: Language,
    pub strategy: Strategy,
}

/// What a server runs. Every engine transcribes whole recordings; some also
/// stream.
pub(crate) struct Info {
    pub engine: String,
    pub streaming: bool,
}

#[derive(Clone)]
pub(crate) struct Transcript {
    pub text: String,
    pub engine: String,
}

pub(crate) enum Event {
    Update {
        update: TranscriptUpdate,
        retained_from: u64,
        read_to: u64,
        completed: Option<Transcript>,
    },
    Complete(Transcript),
}
