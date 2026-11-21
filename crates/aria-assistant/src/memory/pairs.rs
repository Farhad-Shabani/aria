use super::DB_FILE;
use super::store::DATABASE_POLICY;
use aria_kit::cancel::CancelToken;
use aria_platform::sqlite::{self, AtRest, Database};
use std::path::Path;

const MAX_PAIRS: usize = 100_000;
const MAX_PAIR_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PairsError {
    #[error("personal correction pairs could not be read")]
    Io(#[from] std::io::Error),
    #[error("personal correction pairs could not be read")]
    Database(#[from] rusqlite::Error),
    #[error("the memory database could not be opened")]
    Open(#[source] sqlite::Error),
    #[error("personal correction pairs exceed their input limit")]
    TooLarge,
}

/// The pairs memory learned, as `(misheard, correct)`, oldest first. A
/// memory directory without a database has none, and reading never
/// creates one.
pub fn learned_pairs(
    dir: &Path,
    at_rest: &AtRest,
    cancel: &CancelToken,
) -> Result<Vec<(String, String)>, PairsError> {
    let path = dir.join(DB_FILE);
    match std::fs::metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    let conn =
        Database::open(&path, &DATABASE_POLICY, at_rest, cancel).map_err(PairsError::Open)?;
    // A refresh would rather skip learned pairs than wait out a writer.
    conn.busy_timeout(std::time::Duration::from_millis(50))?;
    let mut statement = conn.prepare(
        "SELECT substr(misheard, 1, 257), substr(correct, 1, 257)
         FROM derived_pairs ORDER BY id LIMIT 100001",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut pairs = Vec::new();
    let mut bytes = 0;
    for row in rows {
        let pair = row?;
        bytes += pair.0.len() + pair.1.len();
        if pair.0.len() > 256 || pair.1.len() > 256 || bytes > MAX_PAIR_BYTES {
            return Err(PairsError::TooLarge);
        }
        pairs.push(pair);
    }
    if pairs.len() > MAX_PAIRS {
        return Err(PairsError::TooLarge);
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::Store;

    fn read(dir: &Path) -> Vec<(String, String)> {
        learned_pairs(dir, &AtRest::for_tests(), &CancelToken::never()).expect("reads")
    }

    #[test]
    fn learned_pairs_follow_the_database_without_creating_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(read(dir.path()).is_empty());
        assert!(!dir.path().join(DB_FILE).exists());

        let store = Store::open(&dir.path().join(DB_FILE), &AtRest::for_tests()).expect("opens");
        store
            .add_pair("d b two", "db2-replica", "llm", "s")
            .expect("adds");
        assert_eq!(
            read(dir.path()),
            [("d b two".to_owned(), "db2-replica".to_owned())]
        );
    }

    #[test]
    fn a_read_observes_wal_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join(DB_FILE), &AtRest::for_tests()).unwrap();
        let writer = crate::memory::testing::connection(&dir.path().join(DB_FILE));
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        assert!(read(dir.path()).is_empty());
        store.add_pair("d b two", "db9", "llm", "s").unwrap();
        assert_eq!(read(dir.path()), [("d b two".to_owned(), "db9".to_owned())]);
    }
}
