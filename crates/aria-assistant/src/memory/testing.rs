use super::DB_FILE;
use super::store::Store;

pub(super) fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join(DB_FILE)).expect("opens");
    (dir, store)
}
