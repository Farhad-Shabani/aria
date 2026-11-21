//! Database files are checked with `statat` and never held open here.
//! Closing any descriptor on a database releases every POSIX lock this process
//! holds on it, including the locks of another live connection.

use std::ffi::OsStr;
use std::fs::{File, TryLockError};
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::time::Duration;

use aria_primitives::cancel::CancelToken;
use aria_primitives::private_fs::{
    self, PRIVATE_FILE_MODE, PrivateDirectoryError, PrivateFileError,
};
use rusqlite::{Connection, OpenFlags};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::{AtRest, AtRestError};
#[cfg(target_os = "macos")]
pub use macos::{AtRest, AtRestError};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("no at-rest protection exists for this target");

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

/// `lock` names a file beside the database that every holder keeps locked.
pub enum Ownership {
    /// Any number of connections at once. Each holds `lock` shared, and
    /// encrypting a plaintext database takes it exclusively.
    Shared { lock: &'static str },
    /// One process at a time.
    Exclusive { lock: &'static str },
}

/// What to do with an existing database whose mode is not 0600.
pub enum DriftedMode {
    Refuse,
    Repair,
}

/// Fields drop in order: the lock must outlive SQLite's last use of the file.
pub struct Database {
    connection: Connection,
    _directory: File,
    _ownership: File,
}

impl Deref for Database {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl DerefMut for Database {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the database path has no directory or file name")]
    Path,
    #[error("the database directory could not be prepared")]
    Directory(#[from] PrivateDirectoryError),
    #[error("the database directory is not private")]
    DirectoryNotPrivate,
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
    #[error("the storage key does not open the database")]
    Locked,
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

impl Database {
    /// Keys the connection, then sets the busy timeout, `max_page_count` from
    /// the cap, and `trusted_schema` off. A plaintext database is encrypted
    /// first. The caller owns the schema, its version, and every other pragma.
    pub fn open(
        path: &Path,
        policy: &Policy,
        at_rest: &AtRest,
        cancel: &CancelToken,
    ) -> Result<Self, Error> {
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
            Ownership::Shared { lock } => share_lock(&directory_handle, lock)?,
            Ownership::Exclusive { lock } => take_lock(&directory_handle, lock)?,
        };
        check_cancelled(cancel)?;

        create_if_missing(&directory_handle, name)?;
        let mut before = check_database(&directory_handle, name, policy)?;
        if at_rest.is_plaintext(&directory.join(name), before.st_size)? {
            if let Ownership::Shared { .. } = policy.ownership {
                exclusive(&ownership)?;
            }
            check_cancelled(cancel)?;
            at_rest.seal(&directory_handle, &directory, name)?;
            if let Ownership::Shared { .. } = policy.ownership {
                ownership
                    .lock_shared()
                    .map_err(io("share the database lock"))?;
            }
            before = check_database(&directory_handle, name, policy)?;
        }
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
                    &policy.drifted_mode,
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
        if !is_regular_file(&after)
            || before.st_dev != after.st_dev
            || before.st_ino != after.st_ino
        {
            return Err(Error::Replaced);
        }
        connection.busy_timeout(policy.busy_timeout)?;
        at_rest.unlock(&connection)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        let max_pages = i64::try_from(policy.max_bytes / page_size(&connection)?)
            .map_err(|_| Error::TooLarge)?;
        connection.pragma_update(None, "max_page_count", max_pages)?;
        check_cancelled(cancel)?;
        Ok(Self {
            connection,
            _directory: directory_handle,
            _ownership: ownership,
        })
    }
}

/// A keyed SQLCipher connection reports its page size as text.
pub fn page_size(connection: &Connection) -> rusqlite::Result<u64> {
    let value: rusqlite::types::Value =
        connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let size = match value {
        rusqlite::types::Value::Integer(size) => u64::try_from(size).ok(),
        rusqlite::types::Value::Text(size) => size.parse().ok(),
        _ => None,
    };
    size.filter(|size| *size > 0)
        .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, 0))
}

/// Opens a directory private to the current user, walking down from `/` one
/// component at a time so no link on the way is followed.
pub fn open_directory(path: &Path) -> Result<File, Error> {
    if !path.is_absolute() {
        return Err(Error::Path);
    }
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
    let expected_uid = rustix::process::getuid().as_raw();
    if metadata.uid() != expected_uid {
        return Err(PrivateDirectoryError::WrongOwner {
            expected_uid,
            actual_uid: metadata.uid(),
        }
        .into());
    }
    if metadata.mode() & 0o777 != 0o700 {
        return Err(Error::DirectoryNotPrivate);
    }
    Ok(current)
}

fn take_lock(directory: &File, name: &str) -> Result<File, Error> {
    let lock = open_lock(directory, name)?;
    lock.try_lock().map_err(lock_error)?;
    Ok(lock)
}

/// Blocks only while another connection encrypts the database.
fn share_lock(directory: &File, name: &str) -> Result<File, Error> {
    let lock = open_lock(directory, name)?;
    lock.lock_shared().map_err(io("share the database lock"))?;
    Ok(lock)
}

/// Turns a shared lock exclusive, or fails while another connection holds it.
fn exclusive(lock: &File) -> Result<(), Error> {
    lock.try_lock().map_err(lock_error)
}

fn lock_error(error: TryLockError) -> Error {
    match error {
        TryLockError::WouldBlock => Error::Busy,
        TryLockError::Error(source) => io("lock the database")(source),
    }
}

fn open_lock(directory: &File, name: &str) -> Result<File, Error> {
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
    Ok(lock)
}

fn check_database(directory: &File, name: &OsStr, policy: &Policy) -> Result<Stat, Error> {
    let stat = inspect(directory, name)?.ok_or(Error::Replaced)?;
    check_private(
        directory,
        name,
        &stat,
        policy.max_bytes,
        &policy.drifted_mode,
    )?;
    Ok(stat)
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

fn remove_if_present(directory: &File, name: &OsStr) -> Result<(), Error> {
    match rustix::fs::unlinkat(directory, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(io("remove a database file")(error)),
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
    if stat.st_nlink != 1 {
        return Err(Error::Linked);
    }
    if u64::try_from(stat.st_size).map_or(true, |len| len > max_bytes) {
        return Err(Error::TooLarge);
    }
    #[allow(
        clippy::useless_conversion,
        reason = "st_mode is u16 on macOS and u32 on Linux"
    )]
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
        ownership: Ownership::Shared { lock: "data.lock" },
        drifted_mode: DriftedMode::Refuse,
    };

    fn open(path: &Path, policy: &Policy, cancel: &CancelToken) -> Result<Database, Error> {
        Database::open(path, policy, &AtRest::for_tests(), cancel)
    }

    #[test]
    fn a_database_opens_with_its_schema_untrusted() {
        let temp = tempfile::tempdir().unwrap();
        let opened = open(&temp.path().join("data.db"), &POLICY, &CancelToken::never()).unwrap();
        let trusted: bool = opened
            .pragma_query_value(None, "trusted_schema", |row| row.get(0))
            .unwrap();
        assert!(!trusted);
    }

    #[test]
    fn the_page_cap_keeps_a_database_within_its_size_limit() {
        let temp = tempfile::tempdir().unwrap();
        let policy = Policy {
            max_bytes: 32 * 1024 * 1024,
            ..POLICY
        };
        let opened = open(&temp.path().join("data.db"), &policy, &CancelToken::never()).unwrap();
        let page_size = i64::try_from(page_size(&opened).unwrap()).unwrap();
        let max_pages: i64 = opened
            .pragma_query_value(None, "max_page_count", |row| row.get(0))
            .unwrap();
        assert!(page_size * max_pages <= 32 * 1024 * 1024);
    }

    #[test]
    fn a_rollback_journal_is_private_and_within_its_allowance() {
        let temp = tempfile::tempdir().unwrap();
        let policy = Policy {
            max_bytes: 32 * 1024 * 1024,
            ..POLICY
        };
        let opened = open(&temp.path().join("data.db"), &policy, &CancelToken::never()).unwrap();
        opened.execute_batch("CREATE TABLE entries(text)").unwrap();
        let transaction = opened.unchecked_transaction().unwrap();
        transaction
            .execute("INSERT INTO entries(text) VALUES('private')", [])
            .unwrap();
        let journal = temp.path().join("data.db-journal");
        assert_eq!(fs::metadata(&journal).unwrap().mode() & 0o777, 0o600);
        assert!(fs::metadata(&journal).unwrap().len() <= 32 * 1024 * 1024 + 1024 * 1024);
        transaction.commit().unwrap();
    }

    #[test]
    fn a_database_with_a_second_link_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.db");
        drop(open(&path, &POLICY, &CancelToken::never()).unwrap());
        fs::hard_link(&path, temp.path().join("second")).unwrap();

        assert!(matches!(
            open(&path, &POLICY, &CancelToken::never()),
            Err(Error::Linked)
        ));
    }

    #[test]
    fn a_database_over_its_size_limit_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.db");
        drop(open(&path, &POLICY, &CancelToken::never()).unwrap());
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
    fn a_relative_directory_is_refused() {
        assert!(matches!(
            open_directory(Path::new("store")),
            Err(Error::Path)
        ));
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
        opened.execute_batch("CREATE TABLE t(x)").unwrap();
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
