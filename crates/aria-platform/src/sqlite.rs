//! Database files are checked with `statat` and never held open here.
//! Closing any descriptor on a database releases every POSIX lock this process
//! holds on it, including the locks of another live connection.

use std::ffi::OsStr;
use std::fs::{File, TryLockError};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::time::Duration;

use aria_primitives::cancel::CancelToken;
use aria_primitives::private_fs::{
    self, PRIVATE_FILE_MODE, PrivateDirectoryError, PrivateFileError,
};
use rusqlite::{Connection, OpenFlags};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};

const SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];
// A rollback journal holds original pages plus headers, so it can briefly
// outgrow the database it protects.
const SIDECAR_ALLOWANCE: u64 = 1024 * 1024;

pub struct Policy {
    pub max_bytes: u64,
    pub busy_timeout: Duration,
    pub ownership: Ownership,
    pub drifted_mode: DriftedMode,
}

pub enum Ownership {
    Shared,
    /// One process at a time. `lock` names a file beside the database that
    /// the holder keeps locked.
    Exclusive {
        lock: &'static str,
    },
}

/// What to do with an existing database whose mode is not 0600.
pub enum DriftedMode {
    Refuse,
    Repair,
}

pub struct PrivateDatabase {
    pub connection: Connection,
    pub handles: Handles,
}

/// Keep these until the connection is dropped: the lock must outlive SQLite's
/// last use of the file.
pub struct Handles {
    _directory: File,
    _ownership: Option<File>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the database path has no directory or file name")]
    Path,
    #[error("the database directory is not private")]
    Directory(#[from] PrivateDirectoryError),
    #[error("a database file is not private")]
    File(#[from] PrivateFileError),
    #[error("a database file has more than one link")]
    Linked,
    #[error("a database file exceeds its size limit")]
    TooLarge,
    #[error("the database changed while it was opening")]
    Replaced,
    #[error("another process owns the database")]
    Busy,
    #[error("opening the database was cancelled")]
    Cancelled,
    #[error("failed to {action}")]
    Io {
        action: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("the database could not be opened")]
    Database(#[from] rusqlite::Error),
}

/// Sets the busy timeout, `max_page_count` from the cap, and `trusted_schema`
/// off. The caller owns the schema, its version, and every other pragma.
pub fn open(path: &Path, policy: &Policy, cancel: &CancelToken) -> Result<PrivateDatabase, Error> {
    let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(Error::Path);
    };
    check_cancelled(cancel)?;
    private_fs::prepare_directory(directory)?;
    let directory = directory
        .canonicalize()
        .map_err(io("resolve the database directory"))?;
    let directory_handle = open_directory(&directory)?;
    let ownership = match policy.ownership {
        Ownership::Shared => None,
        Ownership::Exclusive { lock } => Some(take_lock(&directory_handle, lock)?),
    };
    check_cancelled(cancel)?;

    create_if_missing(&directory_handle, name)?;
    let before = inspect(&directory_handle, name)?.ok_or(Error::Replaced)?;
    check_private(
        &directory_handle,
        name,
        &before,
        policy.max_bytes,
        &policy.drifted_mode,
    )?;
    for suffix in SIDECAR_SUFFIXES {
        check_cancelled(cancel)?;
        let mut sidecar = name.to_owned();
        sidecar.push(suffix);
        if let Some(stat) = inspect(&directory_handle, &sidecar)? {
            check_private(
                &directory_handle,
                &sidecar,
                &stat,
                policy.max_bytes + SIDECAR_ALLOWANCE,
                &DriftedMode::Refuse,
            )?;
        }
    }
    check_cancelled(cancel)?;

    let connection = Connection::open_with_flags(
        directory.join(name),
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let after = inspect(&directory_handle, name)?.ok_or(Error::Replaced)?;
    if !is_regular_file(&after) || before.st_dev != after.st_dev || before.st_ino != after.st_ino {
        return Err(Error::Replaced);
    }
    connection.busy_timeout(policy.busy_timeout)?;
    connection.pragma_update(None, "trusted_schema", false)?;
    let page_size: u32 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let max_pages =
        i64::try_from(policy.max_bytes / u64::from(page_size)).map_err(|_| Error::TooLarge)?;
    connection.pragma_update(None, "max_page_count", max_pages)?;
    check_cancelled(cancel)?;
    Ok(PrivateDatabase {
        connection,
        handles: Handles {
            _directory: directory_handle,
            _ownership: ownership,
        },
    })
}

/// Opens a directory private to the current user, walking down from `/` one
/// component at a time so no link on the way is followed.
pub fn open_directory(path: &Path) -> Result<File, Error> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut current = File::from(
        rustix::fs::open("/", flags, Mode::empty()).map_err(io("open the database directory"))?,
    );
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = File::from(
                    rustix::fs::openat(&current, name, flags, Mode::empty())
                        .map_err(io("open the database directory"))?,
                );
            }
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Err(Error::Path);
            }
        }
    }
    let metadata = current
        .metadata()
        .map_err(io("inspect the database directory"))?;
    if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o777 != 0o700 {
        return Err(Error::Replaced);
    }
    Ok(current)
}

fn take_lock(directory: &File, name: &str) -> Result<File, Error> {
    let lock = File::from(
        rustix::fs::openat(
            directory,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(io("open the database lock"))?,
    );
    if private_fs::verify_file(&lock)?.nlink() != 1 {
        return Err(Error::Linked);
    }
    lock.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => Error::Busy,
        TryLockError::Error(source) => io("lock the database")(source),
    })?;
    Ok(lock)
}

/// Uses `O_EXCL`, so the descriptor closed here never belongs to a file
/// another connection has locked.
fn create_if_missing(directory: &File, name: &OsStr) -> Result<(), Error> {
    match rustix::fs::openat(
        directory,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(_) | Err(rustix::io::Errno::EXIST) => Ok(()),
        Err(error) => Err(io("create the database")(error)),
    }
}

fn inspect(directory: &File, name: &OsStr) -> Result<Option<Stat>, Error> {
    match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(io("inspect a database file")(error)),
    }
}

/// Repairs a drifted mode only once every other check has passed.
fn check_private(
    directory: &File,
    name: &OsStr,
    stat: &Stat,
    max_bytes: u64,
    drifted_mode: &DriftedMode,
) -> Result<(), Error> {
    if !is_regular_file(stat) {
        return Err(PrivateFileError::NotRegularFile.into());
    }
    let expected_uid = rustix::process::getuid().as_raw();
    if stat.st_uid != expected_uid {
        return Err(PrivateFileError::WrongOwner {
            expected_uid,
            actual_uid: stat.st_uid,
        }
        .into());
    }
    if u64::from(stat.st_nlink) != 1 {
        return Err(Error::Linked);
    }
    if u64::try_from(stat.st_size).map_or(true, |len| len > max_bytes) {
        return Err(Error::TooLarge);
    }
    let actual_mode = u32::from(stat.st_mode) & 0o777;
    if actual_mode == PRIVATE_FILE_MODE {
        return Ok(());
    }
    match drifted_mode {
        DriftedMode::Refuse => Err(PrivateFileError::NotPrivate {
            expected_mode: PRIVATE_FILE_MODE,
            actual_mode,
        }
        .into()),
        // `chmodat` follows a link, but only this user can swap one into the
        // 0700 directory.
        DriftedMode::Repair => {
            rustix::fs::chmodat(directory, name, Mode::RUSR | Mode::WUSR, AtFlags::empty())
                .map_err(|error| PrivateFileError::SetPermissions(error.into()))?;
            Ok(())
        }
    }
}

fn is_regular_file(stat: &Stat) -> bool {
    FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
}

fn io<E: Into<io::Error>>(action: &'static str) -> impl FnOnce(E) -> Error {
    move |source| Error::Io {
        action,
        source: source.into(),
    }
}

fn check_cancelled(cancel: &CancelToken) -> Result<(), Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    const POLICY: Policy = Policy {
        max_bytes: 1024 * 1024,
        busy_timeout: Duration::from_millis(50),
        ownership: Ownership::Shared,
        drifted_mode: DriftedMode::Refuse,
    };

    #[test]
    fn a_database_opens_with_its_schema_untrusted() {
        let temp = tempfile::tempdir().unwrap();
        let opened = open(&temp.path().join("data.db"), &POLICY, &CancelToken::never()).unwrap();
        let trusted: bool = opened
            .connection
            .pragma_query_value(None, "trusted_schema", |row| row.get(0))
            .unwrap();
        assert!(!trusted);
    }

    #[test]
    fn a_second_link_or_an_oversized_file_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.db");
        drop(open(&path, &POLICY, &CancelToken::never()).unwrap());

        fs::hard_link(&path, temp.path().join("second")).unwrap();
        assert!(matches!(
            open(&path, &POLICY, &CancelToken::never()),
            Err(Error::Linked)
        ));
        fs::remove_file(temp.path().join("second")).unwrap();

        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(POLICY.max_bytes + 1)
            .unwrap();
        assert!(matches!(
            open(&path, &POLICY, &CancelToken::never()),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn a_linked_directory_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        private_fs::prepare_directory(&real).unwrap();
        symlink(&real, temp.path().join("store")).unwrap();
        assert!(
            open(
                &temp.path().join("store/data.db"),
                &POLICY,
                &CancelToken::never()
            )
            .is_err()
        );
        assert!(!real.join("data.db").exists());
    }

    #[test]
    fn a_cancelled_open_creates_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store/data.db");
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            open(&path, &POLICY, &cancel),
            Err(Error::Cancelled)
        ));
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn an_open_database_has_no_descriptor_but_sqlites() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.db");
        let opened = open(&path, &POLICY, &CancelToken::never()).unwrap();
        opened
            .connection
            .execute_batch("CREATE TABLE t(x)")
            .unwrap();
        let database = fs::metadata(&path).unwrap();
        let descriptors = fs::read_dir("/dev/fd")
            .unwrap()
            .filter_map(|entry| fs::metadata(entry.ok()?.path()).ok())
            // macOS reports a different device for `/dev/fd` entries.
            .filter(|target| target.ino() == database.ino())
            .count();
        assert_eq!(descriptors, 1);
    }
}
