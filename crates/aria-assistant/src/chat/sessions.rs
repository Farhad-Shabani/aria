//! The conversation record: which assistant session continues each key.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use super::SessionKey;

/// Resume ids on disk. They outlive the daemon and a reboot, like the
/// assistants' own history.
pub struct SessionStore {
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
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn recall(&self, key: &SessionKey) -> Result<Option<String>, SessionStoreError> {
        let _lock = self.lock();
        Ok(read_records(&self.path)?
            .into_iter()
            .find(|record| record.key == *key)
            .map(|record| record.id))
    }

    pub fn record(&self, key: SessionKey, id: &str) -> Result<(), SessionStoreError> {
        let _lock = self.lock();
        let mut records = read_records(&self.path)?;
        records.retain(|record| record.key != key);
        records.push(SessionRecord {
            key,
            id: id.to_owned(),
        });
        write_records(&self.path, &records)
    }

    pub fn forget(&self, key: &SessionKey) -> Result<(), SessionStoreError> {
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
    let write = |source| SessionStoreError {
        action: "write",
        source,
    };
    // A temp file opens private to its owner, and the rename replaces a file
    // that may have been written under the umask.
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new("."))).map_err(write)?;
    file.write_all(json.as_bytes()).map_err(write)?;
    file.persist(path).map_err(|error| write(error.error))?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("failed to {action} the conversation record")]
pub struct SessionStoreError {
    action: &'static str,
    #[source]
    source: std::io::Error,
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_public_record_file_becomes_private_on_the_next_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat-session.json");
        std::fs::write(&path, "[]").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let store = SessionStore::open(&path);
        let key = SessionKey {
            assistant_id: "claude".to_owned(),
            name: "/tmp".to_owned(),
        };

        store.record(key.clone(), "resume").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(store.recall(&key).unwrap().as_deref(), Some("resume"));
    }
}
