//! A secret kept apart from the documents that name it: on macOS in the
//! default Keychain, elsewhere in a private file of its own.

use std::path::PathBuf;
#[cfg(all(target_os = "macos", any(test, feature = "test-support")))]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(any(not(target_os = "macos"), test, feature = "test-support"))]
use aria_primitives::private_document::{DocumentError, PrivateDocument};

#[cfg(target_os = "macos")]
mod keychain;

#[cfg(any(not(target_os = "macos"), test, feature = "test-support"))]
const MAX_FILE_BYTES: u64 = 16 * 1024;

#[cfg(all(target_os = "macos", any(test, feature = "test-support")))]
static IN_FILES: AtomicBool = AtomicBool::new(false);

pub struct Secret {
    store: Store,
}

enum Store {
    #[cfg(target_os = "macos")]
    Keychain(keychain::Item),
    #[cfg(any(not(target_os = "macos"), test, feature = "test-support"))]
    File(PrivateDocument),
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[cfg(target_os = "macos")]
    #[error("the Keychain refused the secret (status {status})")]
    Keychain { status: i32 },
    #[cfg(target_os = "macos")]
    #[error("the secret in the Keychain is not text")]
    Malformed,
    #[cfg(any(not(target_os = "macos"), test, feature = "test-support"))]
    #[error("the secret's file is not usable")]
    File(#[from] DocumentError),
}

impl Secret {
    /// On macOS the Keychain item `service` and `account` name, elsewhere
    /// `file`.
    pub fn new(service: &'static str, account: &'static str, file: PathBuf) -> Self {
        Self {
            store: platform_store(service, account, file),
        }
    }

    /// `None` only when nothing is stored. A Keychain that is locked or
    /// refuses is an error, so a caller never mistakes it for a lost secret.
    pub fn read(&self) -> Result<Option<String>, SecretError> {
        match &self.store {
            #[cfg(target_os = "macos")]
            Store::Keychain(item) => item.read(),
            #[cfg(any(not(target_os = "macos"), test, feature = "test-support"))]
            Store::File(document) => Ok(document.read::<String>()?),
        }
    }

    pub fn write(&self, value: &str) -> Result<(), SecretError> {
        match &self.store {
            #[cfg(target_os = "macos")]
            Store::Keychain(item) => item.write(value),
            #[cfg(any(not(target_os = "macos"), test, feature = "test-support"))]
            Store::File(document) => {
                Ok(document.prepare(&value)?.commit()?.confirm_durability()?)
            }
        }
    }

    pub fn remove(&self) -> Result<bool, SecretError> {
        match &self.store {
            #[cfg(target_os = "macos")]
            Store::Keychain(item) => item.remove(),
            #[cfg(any(not(target_os = "macos"), test, feature = "test-support"))]
            Store::File(document) => Ok(document.remove()?),
        }
    }
}

/// Keeps every later secret of this process in its file. Tests make secrets
/// in temporary homes by the hundred, and this keeps them out of the
/// developer's Keychain. A switch set at run time, unlike a build feature,
/// leaves every binary that never calls it on the Keychain.
#[cfg(any(test, feature = "test-support"))]
pub fn keep_in_files_for_tests() {
    #[cfg(target_os = "macos")]
    IN_FILES.store(true, Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
fn platform_store(service: &'static str, account: &'static str, file: PathBuf) -> Store {
    #[cfg(any(test, feature = "test-support"))]
    if IN_FILES.load(Ordering::Relaxed) {
        return Store::File(PrivateDocument::new(file, MAX_FILE_BYTES));
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = file;
    Store::Keychain(keychain::Item::new(service, account))
}

#[cfg(not(target_os = "macos"))]
fn platform_store(_service: &'static str, _account: &'static str, file: PathBuf) -> Store {
    Store::File(PrivateDocument::new(file, MAX_FILE_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_in_a_file_is_written_replaced_and_removed() {
        keep_in_files_for_tests();
        let home = tempfile::tempdir().unwrap();
        let secret = Secret::new("test", "test", home.path().join("aria").join("test.key"));

        assert_eq!(secret.read().unwrap(), None);
        secret.write("first").unwrap();
        secret.write("second").unwrap();
        assert_eq!(secret.read().unwrap().as_deref(), Some("second"));
        assert!(secret.remove().unwrap());
        assert_eq!(secret.read().unwrap(), None);
        assert!(!secret.remove().unwrap());
    }

    #[test]
    fn an_unreadable_secret_is_an_error_not_an_absence() {
        keep_in_files_for_tests();
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("aria").join("test.key");
        std::fs::create_dir_all(&path).unwrap();

        assert!(Secret::new("test", "test", path).read().is_err());
    }
}
