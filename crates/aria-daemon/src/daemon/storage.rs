//! The storage key the daemon and `aria memory` share.

use std::path::Path;

use aria_platform::sqlite::{AtRest, AtRestError};
use aria_voice::history;
use aria_voice::vocabulary::{catalog, dictionary};

/// Every loader checks every database, so none replaces a lost key that
/// another one's data still needs. `memory` is the configured memory
/// directory.
pub fn load_storage_key(home: &Path, memory: &Path) -> Result<AtRest, AtRestError> {
    AtRest::load(&[
        home.join(history::DIRECTORY).join(history::DB_FILE),
        memory.join(aria_assistant::memory::DB_FILE),
        home.join(dictionary::DIRECTORY).join(catalog::DB_FILE),
    ])
}
