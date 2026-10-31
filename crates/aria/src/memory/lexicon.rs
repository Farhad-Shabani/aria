use super::{DB_FILE, LEXICON_FILE};
use crate::vocabulary::PersonalRules;
#[cfg(test)]
use crate::vocabulary::Vocabulary;
use aria_kit::cancel::CancelToken;
use rusqlite::{Connection, OpenFlags};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MAX_LEXICON_BYTES: u64 = 1024 * 1024;
const MAX_PAIRS: usize = 100_000;

#[derive(Debug, thiserror::Error)]
pub(crate) enum LexiconError {
    #[error("personal lexicon could not be read")]
    Io(#[from] std::io::Error),
    #[error("personal correction pairs could not be read")]
    Database(#[from] rusqlite::Error),
    #[error("personal lexicon exceeds its input limit")]
    TooLarge,
    #[error("personal lexicon preparation was cancelled")]
    Cancelled,
}

/// One term per line, `#` comments. Missing or unreadable reads as empty.
pub fn load_terms(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

pub(crate) struct Lexicon {
    terms_path: PathBuf,
    db_path: PathBuf,
}

impl Lexicon {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            terms_path: dir.join(LEXICON_FILE),
            db_path: dir.join(DB_FILE),
        }
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Result<PersonalRules, LexiconError> {
        self.snapshot_cancellable(&CancelToken::never())
    }

    pub(crate) fn snapshot_cancellable(
        &self,
        cancel: &CancelToken,
    ) -> Result<PersonalRules, LexiconError> {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(&self.terms_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersonalRules::default());
            }
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "personal lexicon must be a regular file",
            )
            .into());
        }
        let mut text = String::new();
        file.take(MAX_LEXICON_BYTES + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_LEXICON_BYTES {
            return Err(LexiconError::TooLarge);
        }
        let terms: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect();
        if terms.len() > MAX_PAIRS
            || terms
                .iter()
                .any(|term| term.len() > 256 || term.chars().any(char::is_control))
        {
            return Err(LexiconError::TooLarge);
        }
        let learned = match std::fs::metadata(&self.db_path) {
            Ok(_) => read_pairs(&self.db_path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        if learned.len() > MAX_PAIRS {
            return Err(LexiconError::TooLarge);
        }
        PersonalRules::from_stored(terms, learned, cancel).map_err(|error| match error {
            crate::vocabulary::MatchError::Cancelled => LexiconError::Cancelled,
            _ => LexiconError::TooLarge,
        })
    }

    #[cfg(test)]
    pub(crate) fn correct(&self, text: &str) -> Option<String> {
        match self
            .snapshot()
            .and_then(|rules| Vocabulary::compile(&[], rules).map_err(|_| LexiconError::TooLarge))
        {
            Ok(matcher) => matcher.correct(text),
            Err(error) => {
                tracing::warn!(%error, "personal correction unavailable");
                None
            }
        }
    }
}

/// Read-only, and only of a file that exists: a read never creates a
/// database.
fn read_pairs(path: &Path) -> Result<Vec<(String, String)>, LexiconError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
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
        if pair.0.len() > 256 || pair.1.len() > 256 || bytes > MAX_LEXICON_BYTES as usize {
            return Err(LexiconError::TooLarge);
        }
        pairs.push(pair);
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::Store;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn terms_skip_comments_and_blanks_and_a_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("lexicon.txt");
        std::fs::write(&path, "# mine\n\n  db2  \nvex16\n").expect("writes");
        assert_eq!(load_terms(&path), strings(&["db2", "vex16"]));
        assert!(load_terms(&dir.path().join("none.txt")).is_empty());
    }

    #[test]
    fn the_corrector_follows_the_file_and_the_database() {
        let dir = tempfile::tempdir().expect("temp dir");
        let lexicon = Lexicon::new(dir.path());
        assert_eq!(lexicon.correct("d b two"), None);
        assert!(!dir.path().join(DB_FILE).exists());

        let terms = dir.path().join(LEXICON_FILE);
        std::fs::write(&terms, "db2\n").expect("writes");
        assert_eq!(lexicon.correct("d b two").as_deref(), Some("db2"));
        assert_eq!(lexicon.correct("hello"), None);
        assert!(!dir.path().join(DB_FILE).exists());

        let store = Store::open(&dir.path().join(DB_FILE)).expect("opens");
        store
            .add_pair("d b two", "db2-replica", "llm", "s")
            .expect("adds");
        assert_eq!(lexicon.correct("d b two").as_deref(), Some("db2-replica"));

        std::fs::remove_file(&terms).expect("removes");
        assert_eq!(lexicon.correct("d b two"), None);
    }

    #[test]
    fn snapshots_remain_stable_and_refresh_observes_wal_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LEXICON_FILE), "db2\n").unwrap();
        let lexicon = Lexicon::new(dir.path());
        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
        let writer = Connection::open(dir.path().join(DB_FILE)).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        let old = Vocabulary::compile(&[], lexicon.snapshot().unwrap()).unwrap();
        store.add_pair("d b two", "db9", "llm", "s").unwrap();
        let refreshed = Vocabulary::compile(&[], lexicon.snapshot().unwrap()).unwrap();
        assert_eq!(old.correct("d b two").as_deref(), Some("db2"));
        assert_eq!(refreshed.correct("d b two").as_deref(), Some("db9"));
    }

    #[test]
    fn invalid_personal_inputs_fail_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEXICON_FILE);
        let lexicon = Lexicon::new(dir.path());
        std::fs::write(&path, [255]).unwrap();
        assert!(matches!(lexicon.snapshot(), Err(LexiconError::Io(_))));
        std::fs::write(&path, "a".repeat(257)).unwrap();
        assert!(matches!(lexicon.snapshot(), Err(LexiconError::TooLarge)));
    }
}
