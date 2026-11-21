use std::path::Path;

use aria_kit::cancel::CancelToken;
use aria_platform::sqlite::{AtRest, Database};

use super::DB_FILE;
use super::store::{DATABASE_POLICY, Store};

pub(super) fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join(DB_FILE), &AtRest::for_tests()).expect("opens");
    (dir, store)
}

/// A second connection to the memory database, for a test that writes past
/// the store.
pub(super) fn connection(path: &Path) -> Database {
    Database::open(
        path,
        &DATABASE_POLICY,
        &AtRest::for_tests(),
        &CancelToken::never(),
    )
    .expect("opens")
}
