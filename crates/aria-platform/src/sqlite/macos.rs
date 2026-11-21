//! SQLCipher keyed by a storage key the login Keychain keeps. The item's
//! access list trusts only the Aria binary that created it, so another app
//! asking for it gets a macOS prompt.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use rustix::fs::{AtFlags, Mode, OFlags};
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::random::SecRandom;
use security_framework_sys::base::{errSecDuplicateItem, errSecItemNotFound};

use super::{Error, SIDECAR_SUFFIXES, create_if_missing, io};

const SERVICE: &str = "Aria storage key";
const ACCOUNT: &str = "databases";
const KEY_BYTES: usize = 32;
const SEALING_SUFFIX: &str = ".sealing";
const PLAINTEXT_HEADER: &[u8] = b"SQLite format 3\0";

#[derive(Debug, thiserror::Error)]
pub enum AtRestError {
    #[error("the Keychain refused the storage key (status {status})")]
    Keychain { status: i32 },
    #[error("the storage key in the Keychain is malformed")]
    Malformed,
    #[error("no randomness was available for a new storage key")]
    Random(#[source] std::io::Error),
    #[error("the Keychain no longer holds the storage key Aria's saved data is encrypted with")]
    Missing,
    #[error("could not check whether a saved database is encrypted")]
    Probe(#[source] std::io::Error),
}

/// The storage key every saved database is encrypted with.
pub struct AtRest {
    key: [u8; KEY_BYTES],
}

impl AtRest {
    /// Reads the storage key from the Keychain, creating it while none of
    /// `databases` is encrypted yet.
    pub fn load(databases: &[PathBuf]) -> Result<Self, AtRestError> {
        let keychain = SecKeychain::default().map_err(keychain_error)?;
        Self::find_or_create(
            databases,
            || match keychain.find_generic_password(SERVICE, ACCOUNT) {
                Ok((stored, _)) => Self::from_stored(&stored).map(Some),
                Err(error) if error.code() == errSecItemNotFound => Ok(None),
                Err(error) => Err(keychain_error(error)),
            },
            || Self::create(&keychain),
        )
    }

    /// A replacement key would leave the data under the lost one locked for
    /// good, so a key goes missing loudly once a database uses it.
    fn find_or_create(
        databases: &[PathBuf],
        find: impl FnOnce() -> Result<Option<Self>, AtRestError>,
        create: impl FnOnce() -> Result<Self, AtRestError>,
    ) -> Result<Self, AtRestError> {
        if let Some(at_rest) = find()? {
            return Ok(at_rest);
        }
        for database in databases {
            if is_encrypted(database)? {
                return Err(AtRestError::Missing);
            }
        }
        create()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests() -> Self {
        Self::from_key([7; KEY_BYTES])
    }

    #[cfg(any(test, feature = "test-support"))]
    fn from_key(key: [u8; KEY_BYTES]) -> Self {
        Self { key }
    }

    fn create(keychain: &SecKeychain) -> Result<Self, AtRestError> {
        let mut key = [0; KEY_BYTES];
        SecRandom::default()
            .copy_bytes(&mut key)
            .map_err(AtRestError::Random)?;
        match keychain.add_generic_password(SERVICE, ACCOUNT, &key) {
            Ok(()) => Ok(Self { key }),
            // Another Aria process stored one first, and its databases may
            // already use it.
            Err(error) if error.code() == errSecDuplicateItem => {
                let (stored, _) = keychain
                    .find_generic_password(SERVICE, ACCOUNT)
                    .map_err(keychain_error)?;
                Self::from_stored(&stored)
            }
            Err(error) => Err(keychain_error(error)),
        }
    }

    fn from_stored(stored: &[u8]) -> Result<Self, AtRestError> {
        let key = stored.try_into().map_err(|_| AtRestError::Malformed)?;
        Ok(Self { key })
    }

    /// Whether `path` holds an unencrypted database. An empty file is a new
    /// database, not a plaintext one.
    pub(super) fn is_plaintext(&self, path: &Path, len: i64) -> Result<bool, Error> {
        if len == 0 {
            return Ok(false);
        }
        let probe = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        match read_schema(&probe) {
            Ok(()) => Ok(true),
            Err(error) if is_not_a_database(&error) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Replaces the plaintext database `name` with an encrypted copy. The
    /// caller holds the database exclusively, so no connection reads the
    /// old file once it is gone.
    pub(super) fn seal(
        &self,
        directory: &File,
        directory_path: &Path,
        name: &OsStr,
    ) -> Result<(), Error> {
        let mut sealing = name.to_owned();
        sealing.push(SEALING_SUFFIX);
        // A leftover is a seal that never finished; the plaintext is intact.
        remove_if_present(directory, &sealing)?;
        create_if_missing(directory, &sealing)?;
        self.export(&directory_path.join(name), &directory_path.join(&sealing))?;
        for suffix in SIDECAR_SUFFIXES {
            let mut sidecar: OsString = name.to_owned();
            sidecar.push(suffix);
            remove_if_present(directory, &sidecar)?;
        }
        File::from(
            rustix::fs::openat(
                directory,
                &sealing,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io("open the encrypted database"))?,
        )
        .sync_all()
        .map_err(io("flush the encrypted database"))?;
        rustix::fs::renameat(directory, &sealing, directory, name)
            .map_err(io("replace the plaintext database"))?;
        directory
            .sync_all()
            .map_err(io("flush the database directory"))?;
        tracing::info!("encrypted a plaintext database");
        Ok(())
    }

    fn export(&self, plaintext: &Path, sealed: &Path) -> Result<(), Error> {
        let sealed = sealed.to_str().ok_or(Error::Path)?;
        let connection = Connection::open_with_flags(
            plaintext,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        connection.execute(
            "ATTACH DATABASE ?1 AS sealed KEY ?2",
            params![sealed, self.key_literal().as_str()],
        )?;
        connection.query_row("SELECT sqlcipher_export('sealed')", [], |_| Ok(()))?;
        // The export copies the schema and rows, not the header's version.
        connection.pragma_update(Some("sealed"), "user_version", version)?;
        connection.execute_batch("DETACH DATABASE sealed")?;
        Ok(())
    }

    /// Keys a connection a test opened past [`super::Database::open`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn unlock_for_tests(&self, connection: &Connection) {
        self.unlock(connection)
            .expect("the test key opens the database");
    }

    /// Keys `connection`, which must not have read the database yet.
    pub(super) fn unlock(&self, connection: &Connection) -> Result<(), Error> {
        connection.execute_batch(&format!("PRAGMA key = \"{}\"", self.key_literal()))?;
        match read_schema(connection) {
            Ok(()) => Ok(()),
            Err(error) if is_not_a_database(&error) => Err(Error::Locked),
            Err(error) => Err(error.into()),
        }
    }

    /// SQLCipher's raw-key form, which skips its passphrase derivation.
    fn key_literal(&self) -> String {
        let hex: String = self.key.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("x'{hex}'")
    }
}

/// An empty file is a new database, and a missing one was never made.
fn is_encrypted(path: &Path) -> Result<bool, AtRestError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(AtRestError::Probe(error)),
    };
    let mut header = Vec::with_capacity(PLAINTEXT_HEADER.len());
    file.take(PLAINTEXT_HEADER.len() as u64)
        .read_to_end(&mut header)
        .map_err(AtRestError::Probe)?;
    Ok(!header.is_empty() && header != PLAINTEXT_HEADER)
}

fn remove_if_present(directory: &File, name: &OsStr) -> Result<(), Error> {
    match rustix::fs::unlinkat(directory, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(io("remove a database file")(error)),
    }
}

fn read_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
}

fn is_not_a_database(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _) if failure.code == ErrorCode::NotADatabase
    )
}

fn keychain_error(error: security_framework::base::Error) -> AtRestError {
    AtRestError::Keychain {
        status: error.code(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use aria_primitives::cancel::CancelToken;

    use super::super::{Database, DriftedMode, Ownership, Policy};
    use super::*;

    const POLICY: Policy = Policy {
        max_bytes: 1024 * 1024,
        busy_timeout: Duration::from_millis(50),
        ownership: Ownership::Shared { lock: "data.lock" },
        drifted_mode: DriftedMode::Refuse,
    };
    const SECRET: &str = "the owner's private words";

    fn open(path: &Path, at_rest: &AtRest) -> Result<Database, Error> {
        Database::open(path, &POLICY, at_rest, &CancelToken::never())
    }

    fn write_secret(database: &Connection) {
        database
            .execute_batch("CREATE TABLE notes(text); PRAGMA user_version = 7;")
            .unwrap();
        database
            .execute("INSERT INTO notes(text) VALUES(?1)", [SECRET])
            .unwrap();
    }

    fn read_secret(database: &Connection) -> String {
        database
            .query_row("SELECT text FROM notes", [], |row| row.get(0))
            .unwrap()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn a_lost_key_is_reported_and_never_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let databases = [temp.path().join("absent.db"), temp.path().join("data.db")];
        write_secret(&open(&databases[1], &AtRest::for_tests()).unwrap());

        let lost = AtRest::find_or_create(
            &databases,
            || Ok(None),
            || panic!("a lost key must not be replaced"),
        );

        assert!(matches!(lost, Err(AtRestError::Missing)));
    }

    #[test]
    fn a_key_is_created_while_no_database_is_encrypted() {
        let temp = tempfile::tempdir().unwrap();
        let databases = [
            temp.path().join("absent.db"),
            temp.path().join("empty.db"),
            temp.path().join("plain.db"),
        ];
        fs::write(&databases[1], b"").unwrap();
        write_secret(&Connection::open(&databases[2]).unwrap());

        let created = AtRest::find_or_create(&databases, || Ok(None), || Ok(AtRest::for_tests()));

        assert!(created.is_ok());
    }

    #[test]
    fn a_database_on_disk_is_ciphertext() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.db");
        write_secret(&open(&path, &AtRest::for_tests()).unwrap());

        let bytes = fs::read(&path).unwrap();
        assert!(!bytes.starts_with(PLAINTEXT_HEADER));
        assert!(!contains(&bytes, SECRET.as_bytes()));
        assert_eq!(
            read_secret(&open(&path, &AtRest::for_tests()).unwrap()),
            SECRET
        );
    }

    #[test]
    fn a_database_encrypts_with_commoncrypto() {
        let temp = tempfile::tempdir().unwrap();
        let opened = open(&temp.path().join("data.db"), &AtRest::for_tests()).unwrap();
        let provider: String = opened
            .pragma_query_value(None, "cipher_provider", |row| row.get(0))
            .unwrap();
        assert_eq!(provider, "commoncrypto");
    }

    #[test]
    fn another_key_leaves_a_database_locked_and_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.db");
        write_secret(&open(&path, &AtRest::for_tests()).unwrap());
        let before = fs::read(&path).unwrap();

        assert!(matches!(
            open(&path, &AtRest::from_key([9; KEY_BYTES])),
            Err(Error::Locked)
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_plaintext_database_is_sealed_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.db");
        drop(open(&path, &AtRest::for_tests()).unwrap());
        fs::write(&path, b"").unwrap();
        write_secret(&Connection::open(&path).unwrap());
        assert!(fs::read(&path).unwrap().starts_with(PLAINTEXT_HEADER));

        let sealed = open(&path, &AtRest::for_tests()).unwrap();

        assert_eq!(read_secret(&sealed), SECRET);
        let version: i64 = sealed
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 7);
        let bytes = fs::read(&path).unwrap();
        assert!(!bytes.starts_with(PLAINTEXT_HEADER));
        assert!(!contains(&bytes, SECRET.as_bytes()));
        let names: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names
                .iter()
                .all(|name| name == "data.db" || name == "data.lock")
        );
    }

    #[test]
    fn a_plaintext_database_another_connection_holds_is_not_sealed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.db");
        drop(open(&path, &AtRest::for_tests()).unwrap());
        fs::write(&path, b"").unwrap();
        write_secret(&Connection::open(&path).unwrap());
        let holder = File::open(temp.path().join("data.lock")).unwrap();
        holder.lock_shared().unwrap();

        assert!(matches!(
            open(&path, &AtRest::for_tests()),
            Err(Error::Busy)
        ));
        assert!(fs::read(&path).unwrap().starts_with(PLAINTEXT_HEADER));
    }
}
