//! The facts, learned word pairs, the owner's chat turns, and consolidation
//! watermarks, in one SQLite file.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use aria_kit::primitives::private_fs::{self, PRIVATE_FILE_MODE, PrivateDirectoryError};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

const SCHEMA_VERSION: i64 = 4;

/// Claims older than this can be retried, even if their original run is still active.
const STALE_CLAIM_SECONDS: i64 = 600;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
    id INTEGER PRIMARY KEY,
    text TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    confidence REAL NOT NULL,
    source_quote TEXT NOT NULL DEFAULT '',
    source_session TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    invalidated_at TEXT,
    superseded_by INTEGER REFERENCES memories(id),
    embedding BLOB,
    retrieval_count INTEGER NOT NULL DEFAULT 0,
    last_retrieved_at TEXT
);
CREATE TABLE IF NOT EXISTS consolidations (
    session_id TEXT PRIMARY KEY,
    consolidated_at TEXT NOT NULL,
    history_len INTEGER NOT NULL,
    prev_len INTEGER NOT NULL DEFAULT 0,
    candidates INTEGER NOT NULL,
    added INTEGER NOT NULL,
    noops INTEGER NOT NULL,
    pending INTEGER NOT NULL,
    claimed_at TEXT
);
CREATE TABLE IF NOT EXISTS derived_pairs (
    id INTEGER PRIMARY KEY,
    misheard TEXT NOT NULL UNIQUE,
    correct TEXT NOT NULL,
    source TEXT NOT NULL,
    source_session TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS turns (
    id INTEGER PRIMARY KEY,
    session TEXT NOT NULL,
    text TEXT NOT NULL,
    speaker TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS turns_session ON turns(session, id);
";

/// Every stored timestamp is local wall time without a zone, in older databases too.
const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%S', 'now', 'localtime')";

pub(crate) const KINDS: [&str; 3] = ["identity", "preference", "correction"];
pub(crate) const STATUS_ACTIVE: &str = "active";
pub(crate) const STATUS_PENDING: &str = "pending";

#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub id: i64,
    pub text: String,
    pub kind: String,
    pub status: String,
    pub confidence: f64,
    pub source_quote: String,
    pub source_session: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedPair {
    pub id: i64,
    pub misheard: String,
    pub correct: String,
    pub source: String,
}

/// One thing the owner said to the agent, through the own-voice gate.
/// Nothing else is kept: only these feed extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Turn {
    pub(crate) text: String,
    pub(crate) speaker: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("failed to prepare the memory directory")]
    Directory(#[source] PrivateDirectoryError),
    #[error("failed to secure the memory database")]
    Permissions(#[source] io::Error),
    #[error("memory database error")]
    Sqlite(#[from] rusqlite::Error),
    #[error("memory database schema version {0} is not one this build reads")]
    UnsupportedSchema(i64),
    #[error("failed to write the memory profile")]
    Profile(#[source] io::Error),
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the database, readable by its owner alone: it
    /// holds what the user said.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            private_fs::prepare_directory(parent).map_err(StoreError::Directory)?;
        }
        let conn = Connection::open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .map_err(StoreError::Permissions)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let transaction = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(StoreError::UnsupportedSchema(version));
        }
        transaction.execute_batch(SCHEMA)?;
        if matches!(version, 1 | 2) {
            let columns = transaction
                .prepare("PRAGMA table_info(memories)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            for (name, definition) in [
                ("embedding", "embedding BLOB"),
                (
                    "retrieval_count",
                    "retrieval_count INTEGER NOT NULL DEFAULT 0",
                ),
                ("last_retrieved_at", "last_retrieved_at TEXT"),
            ] {
                if !columns.iter().any(|column| column == name) {
                    transaction
                        .execute_batch(&format!("ALTER TABLE memories ADD COLUMN {definition}"))?;
                }
            }
        }
        if version < SCHEMA_VERSION {
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        transaction.commit()?;
        Ok(Self { conn })
    }

    pub(crate) fn add_fact(
        &self,
        text: &str,
        kind: &str,
        status: &str,
        confidence: f64,
        source_quote: &str,
        source_session: &str,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            &format!(
                "INSERT INTO memories \
                 (text, kind, status, confidence, source_quote, source_session, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, {NOW})"
            ),
            params![text, kind, status, confidence, source_quote, source_session],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> Result<Option<Fact>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, text, kind, status, confidence, source_quote, source_session, \
                 created_at FROM memories WHERE id = ?1",
                [id],
                fact_row,
            )
            .optional()?)
    }

    pub fn active_facts(&self) -> Result<Vec<Fact>, StoreError> {
        self.by_status(STATUS_ACTIVE)
    }

    pub fn pending_facts(&self) -> Result<Vec<Fact>, StoreError> {
        self.by_status(STATUS_PENDING)
    }

    fn by_status(&self, status: &str) -> Result<Vec<Fact>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT id, text, kind, status, confidence, source_quote, source_session, \
             created_at FROM memories \
             WHERE status = ?1 AND invalidated_at IS NULL ORDER BY id",
        )?;
        let facts = statement
            .query_map([status], fact_row)?
            .collect::<Result<_, _>>()?;
        Ok(facts)
    }

    /// Moves a pending fact into the profile. `false` when it was not pending.
    pub fn approve(&self, id: i64) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE memories SET status = ?1 WHERE id = ?2 AND status = ?3",
            params![STATUS_ACTIVE, id, STATUS_PENDING],
        )?;
        Ok(changed > 0)
    }

    /// Deletes for good. Only the user forgets; nothing else calls this.
    pub fn forget(&self, id: i64) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM memories WHERE id = ?1", [id])?;
        Ok(changed > 0)
    }

    pub(crate) fn add_pair(
        &self,
        misheard: &str,
        correct: &str,
        source: &str,
        source_session: &str,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            &format!(
                "INSERT INTO derived_pairs \
                 (misheard, correct, source, source_session, created_at) \
                 VALUES (?1, ?2, ?3, ?4, {NOW}) \
                 ON CONFLICT(misheard) DO UPDATE SET correct = excluded.correct, \
                 source = excluded.source, source_session = excluded.source_session, \
                 created_at = excluded.created_at"
            ),
            params![misheard, correct, source, source_session],
        )?;
        // The last insert rowid is stale on the update branch of an upsert.
        Ok(self.conn.query_row(
            "SELECT id FROM derived_pairs WHERE misheard = ?1",
            [misheard],
            |row| row.get(0),
        )?)
    }

    pub fn list_pairs(&self) -> Result<Vec<DerivedPair>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT id, misheard, correct, source FROM derived_pairs ORDER BY id")?;
        let pairs = statement
            .query_map([], |row| {
                Ok(DerivedPair {
                    id: row.get(0)?,
                    misheard: row.get(1)?,
                    correct: row.get(2)?,
                    source: row.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(pairs)
    }

    pub fn forget_pair(&self, id: i64) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM derived_pairs WHERE id = ?1", [id])?;
        Ok(changed > 0)
    }

    pub(crate) fn add_turn(
        &self,
        session: &str,
        text: &str,
        speaker: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            &format!(
                "INSERT INTO turns (session, text, speaker, created_at) \
                 VALUES (?1, ?2, ?3, {NOW})"
            ),
            params![session, text, speaker],
        )?;
        Ok(())
    }

    /// Every session with its turn count, oldest session first.
    pub(crate) fn sessions(&self) -> Result<Vec<(String, u32)>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT session, COUNT(*) FROM turns GROUP BY session ORDER BY MIN(id)")?;
        let sessions = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(sessions)
    }

    pub(crate) fn turns(
        &self,
        session: &str,
        start: u32,
        end: u32,
    ) -> Result<Vec<Turn>, StoreError> {
        assert!(start <= end, "turn range must be ordered");
        let mut statement = self.conn.prepare(
            "SELECT text, speaker FROM turns WHERE session = ?1 ORDER BY id LIMIT ?2 OFFSET ?3",
        )?;
        let turns = statement
            .query_map(params![session, end - start, start], |row| {
                Ok(Turn {
                    text: row.get(0)?,
                    speaker: row.get(1)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(turns)
    }

    /// How many of a session's turns a finished run has covered. A claim in
    /// flight counts for nothing yet: its window is only safe from where it
    /// started.
    pub(crate) fn consolidated_history_len(&self, session: &str) -> Result<u32, StoreError> {
        let row: Option<(u32, u32, i64)> = self
            .conn
            .query_row(
                "SELECT history_len, prev_len, candidates FROM consolidations \
                 WHERE session_id = ?1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(match row {
            Some((history_len, _, candidates)) if candidates >= 0 => history_len,
            Some((_, prev_len, _)) => prev_len,
            None => 0,
        })
    }

    /// Claims the window from the current watermark to `history_len` before
    /// extraction runs, and returns where it starts. `None` when there is
    /// nothing new or another run holds a fresh claim. A stale claim is
    /// taken over from its own start: re-extracting a window costs one LLM
    /// call, and duplicates are settled by the extractor's NOOP.
    pub(crate) fn claim_consolidation(
        &self,
        session: &str,
        history_len: u32,
    ) -> Result<Option<u32>, StoreError> {
        // Rolls back on drop, so a commit that fails leaves nothing open.
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(start) = claim_window(&transaction, session, history_len)? else {
            return Ok(None);
        };
        transaction.commit()?;
        Ok(Some(start))
    }

    pub(crate) fn record_consolidation(
        &self,
        session: &str,
        history_len: u32,
        candidates: u32,
        added: u32,
        noops: u32,
        pending: u32,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            &format!(
                "INSERT OR REPLACE INTO consolidations \
                 (session_id, consolidated_at, history_len, prev_len, candidates, added, \
                 noops, pending, claimed_at) \
                 VALUES (?1, {NOW}, ?2, ?2, ?3, ?4, ?5, ?6, NULL)"
            ),
            params![session, history_len, candidates, added, noops, pending],
        )?;
        Ok(())
    }

    /// Releases an unfinished claim matching the session and end watermark
    /// so the window can be retried.
    pub(crate) fn release_consolidation(
        &self,
        session: &str,
        claimed_len: u32,
        revert_to: u32,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE consolidations SET history_len = ?1, prev_len = ?1, candidates = 0, \
             added = 0, noops = 0, pending = 0, claimed_at = NULL \
             WHERE session_id = ?2 AND history_len = ?3 AND candidates = -1",
            params![revert_to, session, claimed_len],
        )?;
        Ok(())
    }
}

fn claim_window(
    conn: &Connection,
    session: &str,
    history_len: u32,
) -> Result<Option<u32>, StoreError> {
    // `claimed_at` is local wall time; the `'utc'` modifier converts it so it
    // compares with `'now'`.
    let row: Option<(u32, u32, i64, bool)> = conn
        .query_row(
            "SELECT history_len, prev_len, candidates, \
             claimed_at IS NOT NULL AND \
             strftime('%s', 'now') - strftime('%s', claimed_at, 'utc') < ?2 \
             FROM consolidations WHERE session_id = ?1",
            params![session, STALE_CLAIM_SECONDS],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let start = match row {
        Some((_, _, -1, true)) => return Ok(None),
        Some((_, prev_len, -1, false)) => prev_len,
        Some((history_len, _, _, _)) => history_len,
        None => 0,
    };
    if history_len <= start {
        return Ok(None);
    }
    conn.execute(
        &format!(
            "INSERT INTO consolidations \
             (session_id, consolidated_at, history_len, prev_len, candidates, added, \
             noops, pending, claimed_at) \
             VALUES (?1, {NOW}, ?2, ?3, -1, 0, 0, 0, {NOW}) \
             ON CONFLICT(session_id) DO UPDATE SET \
             consolidated_at = excluded.consolidated_at, \
             history_len = excluded.history_len, prev_len = excluded.prev_len, \
             candidates = -1, added = 0, noops = 0, pending = 0, \
             claimed_at = excluded.claimed_at"
        ),
        params![session, history_len, start],
    )?;
    Ok(Some(start))
}

fn fact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Fact> {
    Ok(Fact {
        id: row.get(0)?,
        text: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        confidence: row.get(4)?,
        source_quote: row.get(5)?,
        source_session: row.get(6)?,
        created_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("memory.db")).expect("opens");
        (dir, store)
    }

    fn add(store: &Store, text: &str, status: &str) -> i64 {
        store
            .add_fact(text, "identity", status, 0.9, "quote", "s1")
            .expect("adds")
    }

    fn version(store: &Store) -> i64 {
        store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version")
    }

    fn with_version(version: i64) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("memory.db");
        Connection::open(&path)
            .expect("opens")
            .pragma_update(None, "user_version", version)
            .expect("sets the version");
        (dir, path)
    }

    #[test]
    fn a_fact_round_trips() {
        let (_dir, store) = store();
        let id = add(&store, "User is Sam", STATUS_ACTIVE);
        let fact = store.get(id).expect("reads").expect("exists");
        assert_eq!(fact.text, "User is Sam");
        assert_eq!(fact.kind, "identity");
        assert_eq!(fact.source_quote, "quote");
        assert!(!fact.created_at.is_empty());
        assert!(store.get(id + 1).expect("reads").is_none());
    }

    #[test]
    fn the_file_is_private_to_its_owner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("memory.db");
        Store::open(&path).expect("opens");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, PRIVATE_FILE_MODE);
    }

    #[test]
    fn active_excludes_pending_until_approved() {
        let (_dir, store) = store();
        let active = add(&store, "a", STATUS_ACTIVE);
        let pending = add(&store, "p", STATUS_PENDING);
        let ids = |facts: Vec<Fact>| facts.into_iter().map(|f| f.id).collect::<Vec<_>>();

        assert_eq!(ids(store.active_facts().expect("reads")), [active]);
        assert_eq!(ids(store.pending_facts().expect("reads")), [pending]);
        assert!(store.approve(pending).expect("approves"));
        assert!(!store.approve(pending).expect("approves"));
        assert!(!store.approve(999).expect("approves"));
        assert_eq!(ids(store.active_facts().expect("reads")), [active, pending]);
    }

    #[test]
    fn forgetting_deletes_for_good() {
        let (_dir, store) = store();
        let id = add(&store, "a", STATUS_ACTIVE);
        assert!(store.forget(id).expect("forgets"));
        assert!(!store.forget(id).expect("forgets"));
        assert!(store.get(id).expect("reads").is_none());
    }

    #[test]
    fn facts_persist_across_reopen() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("memory.db");
        let id = add(&Store::open(&path).expect("opens"), "a", STATUS_ACTIVE);
        let reopened = Store::open(&path).expect("opens");
        assert!(reopened.get(id).expect("reads").is_some());
        assert_eq!(version(&reopened), SCHEMA_VERSION);
    }

    #[test]
    fn a_v3_file_opens_and_becomes_v4() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("memory.db");
        {
            let conn = Connection::open(&path).expect("opens");
            let v3 = SCHEMA
                .split("CREATE TABLE IF NOT EXISTS turns")
                .next()
                .expect("v3 tables");
            conn.execute_batch(v3).expect("v3 schema");
            conn.execute(
                "INSERT INTO derived_pairs (misheard, correct, source, created_at) \
                 VALUES ('d b two', 'db2', 'llm', '2026-07-18T21:14:00')",
                [],
            )
            .expect("seeds");
            conn.pragma_update(None, "user_version", 3).expect("v3");
        }
        let store = Store::open(&path).expect("opens");
        assert_eq!(version(&store), 4);
        assert_eq!(store.list_pairs().expect("reads").len(), 1);
        store
            .add_turn("s", "hi", "sam")
            .expect("turns table exists");
    }

    #[test]
    fn other_schema_versions_are_refused() {
        for other in [-1, 5] {
            let (_dir, path) = with_version(other);
            assert!(
                matches!(Store::open(&path), Err(StoreError::UnsupportedSchema(v)) if v == other)
            );
        }
    }

    #[test]
    fn older_databases_gain_missing_columns_and_keep_existing_data() {
        for legacy_version in [1, 2] {
            let (_dir, path) = with_version(legacy_version);
            let conn = Connection::open(&path).expect("opens");
            let mut schema = SCHEMA
                .split("CREATE TABLE IF NOT EXISTS consolidations")
                .next()
                .expect("memories table")
                .replace("    retrieval_count INTEGER NOT NULL DEFAULT 0,\n", "")
                .replace("    last_retrieved_at TEXT\n", "")
                .replace("    embedding BLOB,\n", "    embedding BLOB\n");
            if legacy_version == 1 {
                schema = schema.replace("    embedding BLOB\n", "").replace(
                    "superseded_by INTEGER REFERENCES memories(id),",
                    "superseded_by INTEGER REFERENCES memories(id)",
                );
            }
            conn.execute_batch(&schema).expect("legacy schema");
            conn.execute(
                "INSERT INTO memories (text, kind, confidence, created_at) \
                 VALUES ('User is Sam', 'identity', 0.9, '2026-07-18T21:14:00')",
                [],
            )
            .expect("seeds fact");
            if legacy_version == 2 {
                conn.execute("UPDATE memories SET embedding = X'0102'", [])
                    .expect("seeds embedding");
            }
            drop(conn);

            let store = Store::open(&path).expect("migrates");
            assert_eq!(version(&store), SCHEMA_VERSION);
            assert_eq!(
                store.get(1).expect("reads").expect("fact").text,
                "User is Sam"
            );
            let (embedding, count, retrieved): (Option<Vec<u8>>, i64, Option<String>) = store
                .conn
                .query_row(
                    "SELECT embedding, retrieval_count, last_retrieved_at FROM memories WHERE id = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("reads migrated columns");
            assert_eq!(embedding, (legacy_version == 2).then(|| vec![1, 2]));
            assert_eq!(count, 0);
            assert_eq!(retrieved, None);
            store
                .add_turn("s", "hi", "sam")
                .expect("turns table exists");
            drop(store);
            Store::open(&path).expect("reopens migrated file");
        }
    }

    #[test]
    fn a_pair_upserts_on_its_misheard_form() {
        let (_dir, store) = store();
        let first = store.add_pair("d b two", "db2", "llm", "s1").expect("adds");
        let again = store
            .add_pair("d b two", "db2-replica", "correction", "s2")
            .expect("adds");
        assert_eq!(first, again);
        let pairs = store.list_pairs().expect("reads");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].correct, "db2-replica");
        assert_eq!(pairs[0].source, "correction");
        assert!(store.forget_pair(first).expect("forgets"));
        assert!(!store.forget_pair(first).expect("forgets"));
    }

    #[test]
    fn turns_are_kept_per_session_in_order() {
        let (_dir, store) = store();
        store.add_turn("b", "second session", "sam").expect("adds");
        store.add_turn("a", "hello", "sam").expect("adds");
        store.add_turn("a", "again", "sam").expect("adds");

        let turns = store.turns("a", 0, 2).expect("reads");
        assert_eq!(
            turns,
            [
                Turn {
                    text: "hello".to_owned(),
                    speaker: "sam".to_owned(),
                },
                Turn {
                    text: "again".to_owned(),
                    speaker: "sam".to_owned(),
                },
            ]
        );
        assert_eq!(
            store.sessions().expect("reads"),
            [("b".to_owned(), 1), ("a".to_owned(), 2)]
        );
    }

    #[test]
    fn a_turn_window_reads_only_the_claimed_positions() {
        let (_dir, store) = store();
        store.add_turn("s", "old", "sam").expect("adds");
        store.add_turn("other", "unrelated", "sam").expect("adds");
        store.add_turn("s", "claimed", "sam").expect("adds");
        store.add_turn("s", "later", "sam").expect("adds");
        store
            .conn
            .execute("UPDATE turns SET text = X'00' WHERE text != 'claimed'", [])
            .expect("makes unclaimed text unreadable");
        assert_eq!(
            store.turns("s", 1, 2).expect("reads only the window"),
            [Turn {
                text: "claimed".to_owned(),
                speaker: "sam".to_owned(),
            }]
        );
        assert!(store.turns("s", 2, 2).expect("empty window").is_empty());
    }

    #[test]
    fn a_finished_run_moves_the_watermark() {
        let (_dir, store) = store();
        assert_eq!(store.consolidated_history_len("s").expect("reads"), 0);
        store
            .record_consolidation("s", 4, 2, 1, 1, 0)
            .expect("records");
        assert_eq!(store.consolidated_history_len("s").expect("reads"), 4);
    }

    #[test]
    fn a_claim_blocks_a_second_claim_until_released() {
        let (_dir, store) = store();
        assert_eq!(store.claim_consolidation("s", 4).expect("claims"), Some(0));
        assert_eq!(store.claim_consolidation("s", 6).expect("claims"), None);
        // The window in flight counts for nothing until it finishes.
        assert_eq!(store.consolidated_history_len("s").expect("reads"), 0);

        store.release_consolidation("s", 4, 0).expect("releases");
        assert_eq!(store.claim_consolidation("s", 6).expect("claims"), Some(0));
    }

    #[test]
    fn a_claim_starts_at_the_last_finished_watermark() {
        let (_dir, store) = store();
        store
            .record_consolidation("s", 4, 0, 0, 0, 0)
            .expect("records");
        assert_eq!(store.claim_consolidation("s", 4).expect("claims"), None);
        assert_eq!(store.claim_consolidation("s", 7).expect("claims"), Some(4));
    }

    #[test]
    fn a_release_ignores_a_claim_it_did_not_make() {
        let (_dir, store) = store();
        store.claim_consolidation("s", 4).expect("claims");
        store
            .record_consolidation("s", 4, 1, 1, 0, 0)
            .expect("records");
        store.release_consolidation("s", 4, 0).expect("releases");
        assert_eq!(store.consolidated_history_len("s").expect("reads"), 4);
    }

    #[test]
    fn a_stale_claim_is_taken_over_from_its_start() {
        let (_dir, store) = store();
        store
            .record_consolidation("s", 2, 0, 0, 0, 0)
            .expect("records");
        assert_eq!(store.claim_consolidation("s", 5).expect("claims"), Some(2));
        store
            .conn
            .execute(
                "UPDATE consolidations SET claimed_at = \
                 strftime('%Y-%m-%dT%H:%M:%S', 'now', 'localtime', '-601 seconds')",
                [],
            )
            .expect("ages the claim");
        assert_eq!(store.claim_consolidation("s", 6).expect("claims"), Some(2));
    }

    #[test]
    fn a_claim_in_local_time_reads_as_fresh() {
        let (_dir, store) = store();
        store.claim_consolidation("s", 5).expect("claims");
        let local_timestamp: String = store
            .conn
            .query_row(
                "SELECT strftime('%Y-%m-%dT%H:%M:%S', 'now', 'localtime', '-60 seconds')",
                [],
                |row| row.get(0),
            )
            .expect("a timestamp");
        store
            .conn
            .execute(
                "UPDATE consolidations SET claimed_at = ?1",
                [&local_timestamp],
            )
            .expect("writes the claim");
        assert_eq!(store.claim_consolidation("s", 6).expect("claims"), None);
    }
}
