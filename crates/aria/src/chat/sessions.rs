//! The conversation record: which agent session continues each key.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use super::SessionKey;

/// Resume ids on disk. They outlive the daemon and a reboot, like the
/// agents' own history.
pub(crate) struct SessionStore {
    path: PathBuf,
    /// Asks on different records run side by side; the file is the one
    /// thing they share.
    lock: Mutex<()>,
}

#[derive(Serialize, Deserialize)]
struct SessionRecord {
    #[serde(flatten)]
    key: SessionKey,
    id: String,
}

impl SessionStore {
    pub(crate) fn open(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn recall(&self, key: &SessionKey) -> Result<Option<String>, SessionStoreError> {
        let _lock = self.lock();
        Ok(read_records(&self.path)?
            .into_iter()
            .find(|record| record.key == *key)
            .map(|record| record.id))
    }

    pub(crate) fn record(&self, key: SessionKey, id: &str) -> Result<(), SessionStoreError> {
        let _lock = self.lock();
        let mut records = read_records(&self.path)?;
        records.retain(|record| record.key != key);
        records.push(SessionRecord {
            key,
            id: id.to_owned(),
        });
        write_records(&self.path, &records)
    }

    pub(crate) fn forget(&self, key: &SessionKey) -> Result<(), SessionStoreError> {
        let _lock = self.lock();
        let mut records = read_records(&self.path)?;
        records.retain(|record| record.key != *key);
        write_records(&self.path, &records)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A file that does not decode counts as empty: a stale file should not
/// block an ask, and the next reply rewrites it.
fn read_records(path: &Path) -> Result<Vec<SessionRecord>, SessionStoreError> {
    let record = match std::fs::read_to_string(path) {
        Ok(record) => record,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SessionStoreError {
                action: "read",
                source,
            });
        }
    };
    match serde_json::from_str(&record) {
        Ok(records) => Ok(records),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "ignoring an unreadable session record");
            Ok(Vec::new())
        }
    }
}

fn write_records(path: &Path, records: &[SessionRecord]) -> Result<(), SessionStoreError> {
    let json = serde_json::to_string(records).map_err(|error| SessionStoreError {
        action: "encode",
        source: std::io::Error::other(error),
    })?;
    std::fs::write(path, json).map_err(|source| SessionStoreError {
        action: "write",
        source,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("failed to {action} the conversation record")]
pub(crate) struct SessionStoreError {
    action: &'static str,
    #[source]
    source: std::io::Error,
}
