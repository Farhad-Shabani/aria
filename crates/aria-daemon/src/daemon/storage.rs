//! The storage key the daemon and `aria memory` share.

use std::path::Path;

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
