//! A small JSON document only its owner may read, replaced atomically.
//!
//! A write lands whole or not at all: the bytes go to a temporary file beside
//! the target, reach the disk, and only then take the target's name. A reader
//! that arrives mid-write sees the old document, never a half-written one.
//! [`CommittedWrite::confirm_durability`] is the separate step that says the
//! rename itself survives a power cut.
//!
//! Errors here name the fault, not the path. The caller knows which of its
//! documents it was writing, and says so when it wraps the failure.

use std::fs::{File, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::private_fs::{self, PRIVATE_FILE_MODE, PrivateDirectoryError, PrivateFileError};

/// One document at one path. The byte ceiling is the caller's: a document
/// that outgrows it is a bug or a tampered file, never something to parse.
pub struct PrivateDocument {
    path: PathBuf,
    maximum_bytes: u64,
}

impl PrivateDocument {
    pub fn new(path: impl Into<PathBuf>, maximum_bytes: u64) -> Self {
        Self {
            path: path.into(),
            maximum_bytes,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The parsed document, or `None` when nothing is written there yet.
    /// Creates the containing directory, so a first read leaves a private
    /// home for the first write.
    pub fn read<T: DeserializeOwned>(&self) -> Result<Option<T>, DocumentError> {
        private_fs::prepare_directory(self.parent()?).map_err(DocumentError::Directory)?;

        let Some(file) = self.open_existing()? else {
            return Ok(None);
        };
        let length = self.validate(&file)?;
        let contents = self.read_all(file, length)?;
        serde_json::from_slice(&contents)
            .map(Some)
            .map_err(DocumentError::Parse)
    }

    /// Serializes and stages the write. Nothing is visible at the path until
    /// [`PreparedWrite::commit`].
    pub fn prepare<T: Serialize>(&self, value: &T) -> Result<PreparedWrite, DocumentError> {
        let directory = self.parent()?;
        private_fs::prepare_directory(directory).map_err(DocumentError::Directory)?;

        let mut contents = serde_json::to_vec_pretty(value).map_err(DocumentError::Serialize)?;
        contents.push(b'\n');
        if contents.len() as u64 > self.maximum_bytes {
            return Err(DocumentError::SerializedTooLarge {
                actual_bytes: contents.len() as u64,
                maximum_bytes: self.maximum_bytes,
            });
        }

        // A target that is a symlink, someone else's, or wrongly permissioned
        // is refused before anything is written.
        if let Some(file) = self.open_existing()? {
            self.validate(&file)?;
        }

        let file = |action: &'static str| move |source| DocumentError::File { action, source };
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).map_err(file("create a temporary"))?;
        temporary
            .as_file()
            .set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))
            .map_err(file("secure a temporary"))?;
        temporary
            .write_all(&contents)
            .map_err(file("write a temporary"))?;
        temporary
            .as_file_mut()
            .sync_all()
            .map_err(file("synchronize a temporary"))?;

        Ok(PreparedWrite {
            temporary,
            path: self.path.clone(),
            directory: directory.to_path_buf(),
        })
    }

    /// Creates the document empty, and only if nothing is there yet.
    /// `false` when something already is.
    ///
    /// The filesystem decides, not the caller: a name that folds onto an
    /// existing one — a different case, a different unicode normalization —
    /// is refused here, which no comparison of names could do portably.
    pub fn claim(&self) -> Result<bool, DocumentError> {
        private_fs::prepare_directory(self.parent()?).map_err(DocumentError::Directory)?;
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        let mode = Mode::from_bits_retain(PRIVATE_FILE_MODE as rustix::fs::RawMode);
        match rustix::fs::open(&self.path, flags, mode) {
            Ok(_descriptor) => Ok(true),
            Err(error) if error == rustix::io::Errno::EXIST => Ok(false),
            Err(source) => Err(DocumentError::File {
                action: "claim",
                source: source.into(),
            }),
        }
    }

    /// Deletes the document. `false` when there was nothing there.
    pub fn remove(&self) -> Result<bool, DocumentError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(DocumentError::File {
                action: "delete",
                source,
            }),
        }
    }

    fn parent(&self) -> Result<&Path, DocumentError> {
        self.path.parent().ok_or(DocumentError::NoParentDirectory)
    }

    fn open_existing(&self) -> Result<Option<File>, DocumentError> {
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        match rustix::fs::open(&self.path, flags, Mode::empty()) {
            Ok(descriptor) => Ok(Some(File::from(descriptor))),
            Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
            Err(source) => Err(DocumentError::File {
                action: "open",
                source: source.into(),
            }),
        }
    }

    fn validate(&self, file: &File) -> Result<u64, DocumentError> {
        let metadata = private_fs::verify_file(file).map_err(DocumentError::Unusable)?;
        if metadata.len() > self.maximum_bytes {
            return Err(DocumentError::TooLarge {
                actual_bytes: metadata.len(),
                maximum_bytes: self.maximum_bytes,
            });
        }
        Ok(metadata.len())
    }

    /// Reads one byte past the ceiling, so a file that grew between the size
    /// check and the read is caught rather than truncated into a parse.
    fn read_all(&self, file: File, length: u64) -> Result<Vec<u8>, DocumentError> {
        let mut contents = Vec::with_capacity(length as usize);
        file.take(self.maximum_bytes + 1)
            .read_to_end(&mut contents)
            .map_err(|source| DocumentError::File {
                action: "read",
                source,
            })?;
        if contents.len() as u64 > self.maximum_bytes {
            return Err(DocumentError::TooLarge {
                actual_bytes: contents.len() as u64,
                maximum_bytes: self.maximum_bytes,
            });
        }
        Ok(contents)
    }
}

/// Bytes on the disk under a temporary name. Dropping this instead of
/// committing leaves the document as it was.
pub struct PreparedWrite {
    temporary: tempfile::NamedTempFile,
    path: PathBuf,
    directory: PathBuf,
}

impl PreparedWrite {
    pub fn commit(self) -> Result<CommittedWrite, DocumentError> {
        let Self {
            temporary,
            path,
            directory,
        } = self;
        temporary
            .persist(&path)
            .map_err(|error| DocumentError::File {
                action: "replace the",
                source: error.error,
            })?;
        Ok(CommittedWrite { directory })
    }
}

/// The document is in place. Whether the rename outlives a power cut is the
/// separate question [`Self::confirm_durability`] answers.
pub struct CommittedWrite {
    directory: PathBuf,
}

impl CommittedWrite {
    pub fn confirm_durability(self) -> Result<(), DocumentError> {
        File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .map_err(DocumentError::DurabilityUnconfirmed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("the path has no parent directory")]
    NoParentDirectory,

    #[error("the containing directory is not usable")]
    Directory(#[source] PrivateDirectoryError),

    #[error("failed to {action} file")]
    File {
        action: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("the file is not usable")]
    Unusable(#[source] PrivateFileError),

    #[error("the file holds {actual_bytes} bytes; maximum is {maximum_bytes} bytes")]
    TooLarge {
        actual_bytes: u64,
        maximum_bytes: u64,
    },

    #[error("failed to serialize the document")]
    Serialize(#[source] serde_json::Error),

    #[error("the document serializes to {actual_bytes} bytes; maximum is {maximum_bytes} bytes")]
    SerializedTooLarge {
        actual_bytes: u64,
        maximum_bytes: u64,
    },

    #[error("failed to parse the file")]
    Parse(#[source] serde_json::Error),

    #[error("the document was written, but its durability could not be confirmed")]
    DurabilityUnconfirmed(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    const CEILING: u64 = 1024;

    /// A document under a directory that lives as long as the test holds the
    /// pair.
    fn document() -> (tempfile::TempDir, PrivateDocument) {
        let home = tempfile::tempdir().expect("a temporary directory");
        let document = PrivateDocument::new(home.path().join("state").join("thing.json"), CEILING);
        (home, document)
    }

    fn write(document: &PrivateDocument, value: &Vec<u32>) {
        document
            .prepare(value)
            .expect("the document serializes")
            .commit()
            .expect("the write commits")
            .confirm_durability()
            .expect("the rename is durable");
    }

    #[test]
    fn an_unwritten_document_reads_as_nothing() {
        let (_home, document) = document();
        let read: Option<Vec<u32>> = document.read().expect("the read succeeds");
        assert!(read.is_none());
    }

    #[test]
    fn a_committed_document_reads_back() {
        let (_home, document) = document();
        write(&document, &vec![1, 2, 3]);
        assert_eq!(
            document.read().expect("the read succeeds"),
            Some(vec![1, 2, 3])
        );
    }

    #[test]
    fn a_commit_replaces_what_was_there() {
        let (_home, document) = document();
        write(&document, &vec![1]);
        write(&document, &vec![2]);
        assert_eq!(document.read().expect("the read succeeds"), Some(vec![2]));
    }

    #[test]
    fn a_prepared_write_that_is_dropped_changes_nothing() {
        let (_home, document) = document();
        write(&document, &vec![1]);
        drop(document.prepare(&vec![2]).expect("the document serializes"));
        assert_eq!(document.read().expect("the read succeeds"), Some(vec![1]));
    }

    #[test]
    fn a_document_over_the_ceiling_is_refused_before_it_is_written() {
        let (_home, document) = document();
        let oversized: Vec<u32> = (0..CEILING as u32).collect();
        assert!(matches!(
            document.prepare(&oversized),
            Err(DocumentError::SerializedTooLarge { .. })
        ));
        assert!(!document.path().exists());
    }

    #[test]
    fn a_file_left_readable_by_others_is_refused() {
        let (_home, document) = document();
        write(&document, &vec![1]);
        std::fs::set_permissions(document.path(), Permissions::from_mode(0o644))
            .expect("the mode changes");
        assert!(matches!(
            document.read::<Vec<u32>>(),
            Err(DocumentError::Unusable(..))
        ));
    }

    #[test]
    fn removing_says_whether_anything_was_there() {
        let (_home, document) = document();
        write(&document, &vec![1]);
        assert!(document.remove().expect("the delete succeeds"));
        assert!(!document.remove().expect("the delete succeeds"));
    }
}
