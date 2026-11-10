//! The resume record: which assistant session continues each key. Also the
//! file policy every chat record file shares.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::SessionKey;

/// Every ask a client names mints a record, so the file keeps only the
/// newest ones. A dropped chat starts afresh on its next ask.
const MAX_RECORDS: usize = 1_000;

const RECORD: &str = "resume record";

/// Resume ids on disk. They outlive the daemon and a reboot, like the
/// assistants' own history.
pub struct SessionStore {
    path: PathBuf,
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

    pub fn recall(&self, key: &SessionKey) -> Result<Option<String>, RecordError> {
        let _lock = lock(&self.lock);
        Ok(read_records::<SessionRecord>(&self.path, RECORD)?
            .into_iter()
            .find(|record| record.key == *key)
            .map(|record| record.id))
    }

    pub fn record(&self, key: SessionKey, id: &str) -> Result<(), RecordError> {
        let _lock = lock(&self.lock);
        let mut records = read_records::<SessionRecord>(&self.path, RECORD)?;
        records.retain(|record| record.key != key);
        records.push(SessionRecord {
            key,
            id: id.to_owned(),
        });
        let excess = records.len().saturating_sub(MAX_RECORDS);
        records.drain(..excess);
        write_records(&self.path, RECORD, &records)
    }

    /// Moves the resume id, if there is one, to `new`, replacing any stale
    /// one there.
    pub fn rename(&self, old: &SessionKey, new: SessionKey) -> Result<(), RecordError> {
        let _lock = lock(&self.lock);
        let mut records = read_records::<SessionRecord>(&self.path, RECORD)?;
        records.retain(|record| record.key != new);
        for record in &mut records {
            if record.key == *old {
                record.key = new.clone();
            }
        }
        write_records(&self.path, RECORD, &records)
    }

    pub fn forget(&self, key: &SessionKey) -> Result<(), RecordError> {
        let _lock = lock(&self.lock);
        let mut records = read_records::<SessionRecord>(&self.path, RECORD)?;
        records.retain(|record| record.key != *key);
        write_records(&self.path, RECORD, &records)
    }
}

/// Chats on different keys run side by side; a record file is the one thing
/// they share. The lock guards no data, so a panic leaves nothing to repair.
pub(super) fn lock(lock: &Mutex<()>) -> MutexGuard<'_, ()> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A file that does not decode counts as empty: a stale file should not
/// block a chat, and the next write replaces it.
pub(super) fn read_records<R: DeserializeOwned>(
    path: &Path,
    record: &'static str,
) -> Result<Vec<R>, RecordError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(RecordError {
                action: "read",
                record,
                source,
            });
        }
    };
    match serde_json::from_str(&contents) {
        Ok(records) => Ok(records),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), record, "ignoring an unreadable chat record");
            Ok(Vec::new())
        }
    }
}

pub(super) fn write_records<R: Serialize>(
    path: &Path,
    record: &'static str,
    records: &[R],
) -> Result<(), RecordError> {
    let json = serde_json::to_string(records).map_err(|error| RecordError {
        action: "encode",
        record,
        source: std::io::Error::other(error),
    })?;
    let write = |source| RecordError {
        action: "write",
        record,
        source,
    };
    // A temp file opens private to its owner, and the rename replaces a file
    // that may have been written under the umask.
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(write)?;
    file.write_all(json.as_bytes()).map_err(write)?;
    file.persist(path).map_err(|error| write(error.error))?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("failed to {action} the {record}")]
pub struct RecordError {
    action: &'static str,
    record: &'static str,
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

    #[test]
    fn a_renamed_chat_keeps_its_resume_id_and_drops_a_stale_one() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path().join("chat-session.json"));
        let key = |name: &str| SessionKey {
            assistant_id: "claude-code".to_owned(),
            name: name.to_owned(),
        };
        store.record(key("trip"), "trip-session").unwrap();
        store.record(key("Weekend"), "stale-session").unwrap();

        store.rename(&key("trip"), key("Weekend")).unwrap();

        assert_eq!(
            store.recall(&key("Weekend")).unwrap().as_deref(),
            Some("trip-session")
        );
        assert_eq!(store.recall(&key("trip")).unwrap(), None);
    }

    #[test]
    fn the_record_keeps_its_newest_thousand_and_drops_the_oldest() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat-session.json");
        let key = |name: usize| SessionKey {
            assistant_id: "claude-code".to_owned(),
            name: name.to_string(),
        };
        let full = (0..MAX_RECORDS)
            .map(|name| SessionRecord {
                key: key(name),
                id: format!("resume-{name}"),
            })
            .collect::<Vec<_>>();
        write_records(&path, RECORD, &full).unwrap();
        let store = SessionStore::open(&path);

        store.record(key(MAX_RECORDS), "newest").unwrap();

        assert_eq!(store.recall(&key(0)).unwrap(), None);
        assert_eq!(store.recall(&key(1)).unwrap().as_deref(), Some("resume-1"));
        assert_eq!(
            store.recall(&key(MAX_RECORDS)).unwrap().as_deref(),
            Some("newest")
        );
        assert_eq!(
            read_records::<SessionRecord>(&path, RECORD).unwrap().len(),
            MAX_RECORDS
        );
    }
}
