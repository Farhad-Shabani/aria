//! The storage key the daemon and `aria memory` share.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use aria_platform::sqlite::{AtRest, AtRestError};
#[cfg(not(test))]
use aria_voice::history;
#[cfg(not(test))]
use aria_voice::vocabulary::{self, catalog};

/// Every loader checks every database, so none replaces a lost key that
/// another one's data still needs. `memory` is the configured memory
/// directory.
#[cfg(not(test))]
pub fn load_storage_key(home: &Path, memory: &Path) -> Result<AtRest, AtRestError> {
    AtRest::load(&[
        home.join(history::DIRECTORY).join(history::DB_FILE),
        memory.join(aria_assistant::memory::DB_FILE),
        home.join(vocabulary::DIRECTORY).join(catalog::DB_FILE),
    ])
}

/// A unit-test daemon must never touch the developer's Keychain.
#[cfg(test)]
pub fn load_storage_key(_home: &Path, _memory: &Path) -> Result<AtRest, AtRestError> {
    Ok(AtRest::for_tests())
}

/// The daemon's storage key, loaded when first needed. A failed load is
/// tried again on the next ask, so a Keychain locked at startup stops
/// blocking saved data once it unlocks.
pub(super) struct StorageKey {
    home: PathBuf,
    memory: PathBuf,
    loaded: OnceLock<Arc<AtRest>>,
}

impl StorageKey {
    pub(super) fn new(home: PathBuf, memory: PathBuf) -> Self {
        Self {
            home,
            memory,
            loaded: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn for_tests() -> Self {
        Self::new(PathBuf::new(), PathBuf::new())
    }

    pub(super) fn load(&self) -> Result<Arc<AtRest>, AtRestError> {
        self.load_with(|| load_storage_key(&self.home, &self.memory))
    }

    /// Two first callers may both reach the Keychain. Creating the key
    /// survives that race, so whichever result lands first is kept.
    fn load_with(
        &self,
        load: impl FnOnce() -> Result<AtRest, AtRestError>,
    ) -> Result<Arc<AtRest>, AtRestError> {
        if let Some(loaded) = self.loaded.get() {
            return Ok(Arc::clone(loaded));
        }
        let loaded = Arc::new(load()?);
        Ok(Arc::clone(self.loaded.get_or_init(|| loaded)))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn a_failed_key_load_is_tried_again() {
        let key = StorageKey::for_tests();
        let calls = AtomicUsize::new(0);
        let load = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(AtRest::for_tests())
        };

        assert!(key.load_with(|| Err(AtRestError::Missing)).is_err());
        assert!(key.load_with(load).is_ok());
        assert!(key.load_with(load).is_ok());

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
