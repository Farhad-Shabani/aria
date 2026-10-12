use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::private_fs;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use rustix::fs::{Mode, OFlags};

use super::{
    Candidate, Discovery, MAX_CANDIDATES, MAX_SOURCE_BYTES, MAX_TERMS, Source, SourceKind,
    source_bytes,
};

const SCHEMA_VERSION: u32 = 2;
const MAX_DATABASE_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) struct Catalog {
    connection: Connection,
    cancelled: CancelToken,
    _directory: File,
    _database: File,
}

pub(crate) struct CachedScope {
    pub(crate) revision: u64,
    pub(crate) sources: Vec<Source>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("vocabulary catalog filesystem operation failed")]
    Io(#[from] io::Error),
    #[error("vocabulary catalog directory is not private")]
    Directory(#[from] private_fs::PrivateDirectoryError),
    #[error("vocabulary catalog file is not private")]
    File(#[from] private_fs::PrivateFileError),
    #[error("vocabulary catalog database operation failed")]
    Database(#[source] rusqlite::Error),
    #[error("vocabulary catalog schema {0} is newer than supported")]
    UnsupportedSchema(u32),
    #[error("vocabulary catalog exceeds its resource limit")]
    TooLarge,
    #[error("vocabulary catalog contains invalid data")]
    InvalidData,
    #[error("vocabulary catalog file changed while opening")]
    Replaced,
    #[error("vocabulary catalog refresh was cancelled")]
    Cancelled,
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::SqliteFailure(cause, _)
                if cause.code == rusqlite::ErrorCode::DiskFull =>
            {
                Self::TooLarge
            }
            rusqlite::Error::SqliteFailure(cause, _)
                if cause.code == rusqlite::ErrorCode::OperationInterrupted =>
            {
                Self::Cancelled
            }
            _ => Self::Database(error),
        }
    }
}

impl Catalog {
    pub(crate) fn open(directory: &Path, cancelled: CancelToken) -> Result<Self, Error> {
        check_cancelled(&cancelled)?;
        private_fs::prepare_directory(directory)?;
        check_cancelled(&cancelled)?;
        let directory = directory.canonicalize()?;
        check_cancelled(&cancelled)?;
        let directory_handle = super::discover::open_directory(&directory)?;
        check_cancelled(&cancelled)?;
        let metadata = directory_handle.metadata()?;
        check_cancelled(&cancelled)?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::getuid().as_raw()
            || metadata.mode() & 0o777 != 0o700
        {
            return Err(Error::InvalidData);
        }
        let flags =
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let descriptor = rustix::fs::openat(
            &directory_handle,
            "catalog.db",
            flags,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(io::Error::from)?;
        check_cancelled(&cancelled)?;
        let database = File::from(descriptor);
        let before = private_fs::verify_file(&database)?;
        check_cancelled(&cancelled)?;
        if before.nlink() != 1 {
            return Err(Error::InvalidData);
        }
        if before.len() > MAX_DATABASE_BYTES {
            return Err(Error::TooLarge);
        }
        for sidecar in ["catalog.db-journal", "catalog.db-wal", "catalog.db-shm"] {
            check_cancelled(&cancelled)?;
            match rustix::fs::openat(
                &directory_handle,
                sidecar,
                OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(descriptor) => {
                    let metadata = private_fs::verify_file(&File::from(descriptor))?;
                    check_cancelled(&cancelled)?;
                    if metadata.nlink() != 1 {
                        return Err(Error::InvalidData);
                    }
                    if metadata.len() > MAX_DATABASE_BYTES {
                        return Err(Error::TooLarge);
                    }
                }
                Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(Error::Io(error.into())),
            }
        }
        check_cancelled(&cancelled)?;
        let path = directory.join("catalog.db");
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        check_cancelled(&cancelled)?;
        let progress = cancelled.clone();
        connection.progress_handler(1_000, Some(move || progress.is_cancelled()))?;
        let after = std::fs::symlink_metadata(&path)?;
        check_cancelled(&cancelled)?;
        if before.dev() != after.dev() || before.ino() != after.ino() || !after.is_file() {
            return Err(Error::Replaced);
        }
        connection.busy_timeout(Duration::from_millis(250))?;
        let schema: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema > SCHEMA_VERSION {
            return Err(Error::UnsupportedSchema(schema));
        }
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        let page_size: u32 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
        connection.pragma_update(
            None,
            "max_page_count",
            (MAX_DATABASE_BYTES / u64::from(page_size)) as u32,
        )?;
        check_cancelled(&cancelled)?;
        if schema < SCHEMA_VERSION {
            let transaction = rusqlite::Transaction::new_unchecked(
                &connection,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            transaction.execute_batch(
                "DROP TABLE IF EXISTS terms;
                 DROP TABLE IF EXISTS sources;
                 DROP TABLE IF EXISTS indexed_scopes;
                 DROP TABLE IF EXISTS projects;
                 CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    root BLOB UNIQUE NOT NULL CHECK(length(root) BETWEEN 1 AND 4096),
                    revision INTEGER NOT NULL CHECK(revision > 0),
                    pipeline_key TEXT NOT NULL CHECK(length(pipeline_key) BETWEEN 1 AND 256),
                    accessed INTEGER NOT NULL,
                    entries INTEGER NOT NULL, hidden INTEGER NOT NULL,
                    symlinks INTEGER NOT NULL, unsupported INTEGER NOT NULL,
                    oversized INTEGER NOT NULL, bytes INTEGER NOT NULL);
                 CREATE TABLE scope_aliases (
                    selected_path BLOB PRIMARY KEY CHECK(length(selected_path) BETWEEN 1 AND 4096),
                    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE);
                 CREATE TABLE sources (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                    relative_path BLOB NOT NULL CHECK(length(relative_path) BETWEEN 1 AND 4096),
                    kind INTEGER NOT NULL CHECK(kind IN (0,1,2)),
                    content_hash BLOB NOT NULL CHECK(length(content_hash)=32),
                    input_bytes INTEGER NOT NULL CHECK(input_bytes BETWEEN 0 AND 65536),
                    UNIQUE(project_id,relative_path,kind));
                 CREATE TABLE terms (
                    source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
                    rank INTEGER NOT NULL CHECK(rank >= 0),
                    canonical TEXT NOT NULL CHECK(length(canonical) BETWEEN 1 AND 256),
                    span_start INTEGER NOT NULL CHECK(span_start >= 0),
                    span_end INTEGER NOT NULL CHECK(span_end > span_start),
                    PRIMARY KEY(source_id,rank), UNIQUE(source_id,canonical));
                 PRAGMA user_version=2;",
            )?;
            check_cancelled(&cancelled)?;
            transaction.commit()?;
        }
        Ok(Self {
            connection,
            cancelled,
            _directory: directory_handle,
            _database: database,
        })
    }

    pub(crate) fn cancel_on(&mut self, cancelled: CancelToken) -> Result<(), Error> {
        let progress = cancelled.clone();
        self.connection
            .progress_handler(1_000, Some(move || progress.is_cancelled()))?;
        self.cancelled = cancelled;
        Ok(())
    }

    pub(crate) fn load(&self, root: &Path, key: &str) -> Result<Option<CachedScope>, Error> {
        check_cancelled(&self.cancelled)?;
        validate_absolute(root)?;
        validate_key(key)?;
        let transaction = self.connection.unchecked_transaction()?;
        let project = transaction
            .query_row(
                "SELECT id,revision FROM projects WHERE root=?1 AND pipeline_key=?2",
                params![bytes(root), key],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((project, revision)) = project else {
            return Ok(None);
        };
        let mut query = transaction.prepare(
            "SELECT id,relative_path,kind,content_hash,input_bytes FROM sources WHERE project_id=?1 ORDER BY id")?;
        let mut rows = query.query([project])?;
        let mut sources = Vec::new();
        let mut retained = 0;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            check_cancelled(&self.cancelled)?;
            if row
                .get_ref(1)?
                .as_blob()
                .map_err(|_| Error::InvalidData)?
                .len()
                > 4096
                || row
                    .get_ref(3)?
                    .as_blob()
                    .map_err(|_| Error::InvalidData)?
                    .len()
                    != 32
            {
                return Err(Error::InvalidData);
            }
            let id: i64 = row.get(0)?;
            let mut source = Source {
                relative_path: native_path(row.get(1)?),
                kind: match row.get::<_, u8>(2)? {
                    0 => SourceKind::DirectoryName,
                    1 => SourceKind::FileName,
                    2 => SourceKind::Text,
                    _ => return Err(Error::InvalidData),
                },
                fingerprint: row
                    .get::<_, Vec<u8>>(3)?
                    .try_into()
                    .map_err(|_| Error::InvalidData)?,
                input_bytes: usize::try_from(row.get::<_, i64>(4)?)
                    .map_err(|_| Error::InvalidData)?,
                candidates: Vec::new(),
            };
            validate_source(&source)?;
            retained += source_bytes(&source);
            if retained > MAX_SOURCE_BYTES {
                return Err(Error::TooLarge);
            }
            let mut terms = transaction.prepare(
                "SELECT rank,canonical,span_start,span_end FROM terms WHERE source_id=?1 ORDER BY rank")?;
            let mut terms = terms.query([id])?;
            while let Some(row) = terms.next()? {
                check_cancelled(&self.cancelled)?;
                if source.candidates.len() == MAX_CANDIDATES {
                    return Err(Error::TooLarge);
                }
                if row
                    .get_ref(1)?
                    .as_str()
                    .map_err(|_| Error::InvalidData)?
                    .len()
                    > 256
                {
                    return Err(Error::InvalidData);
                }
                if usize::try_from(row.get::<_, i64>(0)?).map_err(|_| Error::InvalidData)?
                    != source.candidates.len()
                {
                    return Err(Error::InvalidData);
                }
                let candidate = Candidate {
                    canonical: row.get(1)?,
                    span: usize::try_from(row.get::<_, i64>(2)?).map_err(|_| Error::InvalidData)?
                        ..usize::try_from(row.get::<_, i64>(3)?).map_err(|_| Error::InvalidData)?,
                };
                validate_candidate(&candidate, source.input_bytes)?;
                count += 1;
                retained += 256 + 2 * candidate.canonical.len();
                if count > MAX_TERMS || retained > MAX_SOURCE_BYTES {
                    return Err(Error::TooLarge);
                }
                source.candidates.push(candidate);
            }
            sources.push(source);
        }
        drop(rows);
        drop(query);
        transaction.execute(
            "UPDATE projects SET accessed=unixepoch() WHERE id=?1",
            [project],
        )?;
        check_cancelled(&self.cancelled)?;
        transaction.commit()?;
        Ok(Some(CachedScope {
            revision: revision
                .try_into()
                .ok()
                .filter(|revision| *revision > 0)
                .ok_or(Error::InvalidData)?,
            sources,
        }))
    }

    pub(crate) fn remember_alias(&self, selected: &Path, root: &Path) -> Result<(), Error> {
        check_cancelled(&self.cancelled)?;
        validate_absolute(selected)?;
        validate_absolute(root)?;
        self.connection.execute(
            "INSERT INTO scope_aliases(selected_path,project_id) SELECT ?1,id FROM projects WHERE root=?2
             ON CONFLICT(selected_path) DO UPDATE SET project_id=excluded.project_id",
            params![bytes(selected), bytes(root)],
        )?;
        Ok(())
    }

    pub(crate) fn refresh_cancellable(
        &mut self,
        discovery: &Discovery,
        key: &str,
        cancel: &CancelToken,
    ) -> Result<u64, Error> {
        check_cancelled(cancel)?;
        validate_absolute(&discovery.root)?;
        validate_absolute(&discovery.selected)?;
        validate_key(key)?;
        let mut retained = 0;
        let mut count = 0;
        for source in &discovery.sources {
            check_cancelled(cancel)?;
            validate_source(source)?;
            retained += source_bytes(source);
            count += source.candidates.len();
            if retained > MAX_SOURCE_BYTES || count > MAX_TERMS {
                return Err(Error::TooLarge);
            }
            for candidate in &source.candidates {
                check_cancelled(cancel)?;
                validate_candidate(candidate, source.input_bytes)?;
            }
        }
        let transaction = self.connection.transaction()?;
        loop {
            check_cancelled(cancel)?;
            let pages: u32 =
                transaction.pragma_query_value(None, "page_count", |row| row.get(0))?;
            let free: u32 =
                transaction.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
            let size: u32 = transaction.pragma_query_value(None, "page_size", |row| row.get(0))?;
            if u64::from(pages.checked_sub(free).ok_or(Error::InvalidData)?) * u64::from(size)
                + 2 * retained as u64
                <= MAX_DATABASE_BYTES
            {
                break;
            }
            let oldest: Option<i64> = transaction
                .query_row(
                    "SELECT id FROM projects WHERE root!=?1 ORDER BY accessed,id LIMIT 1",
                    [bytes(&discovery.root)],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(oldest) = oldest else {
                return Err(Error::TooLarge);
            };
            transaction.execute("DELETE FROM projects WHERE id=?1", [oldest])?;
        }
        let coverage = &discovery.coverage;
        transaction.execute(
            "INSERT INTO projects(root,revision,pipeline_key,accessed,entries,hidden,symlinks,unsupported,oversized,bytes)
             VALUES(?1,1,?2,unixepoch(),?3,?4,?5,?6,?7,?8)
             ON CONFLICT(root) DO UPDATE SET revision=revision+1,pipeline_key=excluded.pipeline_key,
             accessed=excluded.accessed,entries=excluded.entries,hidden=excluded.hidden,
             symlinks=excluded.symlinks,unsupported=excluded.unsupported,oversized=excluded.oversized,bytes=excluded.bytes",
            params![bytes(&discovery.root),key,sql_integer(coverage.entries)?,sql_integer(coverage.hidden)?,sql_integer(coverage.symlinks)?,
                sql_integer(coverage.unsupported)?,sql_integer(coverage.oversized)?,sql_integer(coverage.bytes)?],
        )?;
        let (project, revision): (i64, i64) = transaction.query_row(
            "SELECT id,revision FROM projects WHERE root=?1",
            [bytes(&discovery.root)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        transaction.execute("DELETE FROM sources WHERE project_id=?1", [project])?;
        for source in &discovery.sources {
            check_cancelled(cancel)?;
            transaction.execute(
                "INSERT INTO sources(project_id,relative_path,kind,content_hash,input_bytes) VALUES(?1,?2,?3,?4,?5)",
                params![project,bytes(&source.relative_path),source.kind as u8,&source.fingerprint[..],sql_integer(source.input_bytes)?],
            )?;
            let id = transaction.last_insert_rowid();
            for (rank, candidate) in source.candidates.iter().enumerate() {
                check_cancelled(cancel)?;
                transaction.execute(
                    "INSERT INTO terms(source_id,rank,canonical,span_start,span_end) VALUES(?1,?2,?3,?4,?5)",
                    params![id,sql_integer(rank)?,candidate.canonical,sql_integer(candidate.span.start)?,sql_integer(candidate.span.end)?],
                )?;
            }
        }
        transaction.execute(
            "INSERT INTO scope_aliases(selected_path,project_id) VALUES(?1,?2)
             ON CONFLICT(selected_path) DO UPDATE SET project_id=excluded.project_id",
            params![bytes(&discovery.selected), project],
        )?;
        check_cancelled(cancel)?;
        let revision = u64::try_from(revision)
            .ok()
            .filter(|revision| *revision > 0)
            .ok_or(Error::InvalidData)?;
        transaction.commit()?;
        Ok(revision)
    }

    pub(crate) fn remove_overlapping(
        &mut self,
        selected: &Path,
        known_root: Option<&Path>,
        cancel: &CancelToken,
    ) -> Result<Vec<PathBuf>, Error> {
        check_cancelled(cancel)?;
        validate_absolute(selected)?;
        if let Some(root) = known_root {
            validate_absolute(root)?;
        }
        let transaction = self.connection.transaction()?;
        let stored: Option<Vec<u8>> = transaction.query_row(
            "SELECT substr(p.root,1,4097) FROM scope_aliases a JOIN projects p ON p.id=a.project_id WHERE a.selected_path=?1",
            [bytes(selected)], |row| row.get(0),
        ).optional()?;
        let stored = stored.map(native_path);
        if let Some(root) = &stored {
            validate_absolute(root)?;
        }
        let root = known_root.or(stored.as_deref()).unwrap_or(selected);
        let mut query = transaction.prepare("SELECT id,root FROM projects ORDER BY id")?;
        let mut rows = query.query([])?;
        let mut removed = Vec::new();
        let mut retained = 0;
        while let Some(row) = rows.next()? {
            check_cancelled(cancel)?;
            if row
                .get_ref(1)?
                .as_blob()
                .map_err(|_| Error::InvalidData)?
                .len()
                > 4096
            {
                return Err(Error::InvalidData);
            }
            let path = native_path(row.get(1)?);
            validate_absolute(&path)?;
            if path.starts_with(root)
                || root.starts_with(&path)
                || stored
                    .as_deref()
                    .is_some_and(|stored| path.starts_with(stored) || stored.starts_with(&path))
            {
                retained += bytes(&path).len() + 64;
                if retained > MAX_SOURCE_BYTES {
                    return Err(Error::TooLarge);
                }
                removed.push((row.get::<_, i64>(0)?, path));
            }
        }
        drop(rows);
        drop(query);
        for (id, _) in &removed {
            check_cancelled(cancel)?;
            transaction.execute("DELETE FROM projects WHERE id=?1", [id])?;
        }
        check_cancelled(cancel)?;
        transaction.commit()?;
        Ok(removed.into_iter().map(|(_, path)| path).collect())
    }
}

fn sql_integer(value: usize) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| Error::InvalidData)
}

fn check_cancelled(cancel: &CancelToken) -> Result<(), Error> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}
fn native_path(bytes: Vec<u8>) -> PathBuf {
    OsString::from_vec(bytes).into()
}

fn validate_key(key: &str) -> Result<(), Error> {
    if key.is_empty() || key.len() > 256 || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(Error::InvalidData);
    }
    Ok(())
}

fn validate_absolute(path: &Path) -> Result<(), Error> {
    if !path.is_absolute()
        || bytes(path).len() > 4096
        || bytes(path).contains(&0)
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(Error::InvalidData);
    }
    Ok(())
}

fn validate_source(source: &Source) -> Result<(), Error> {
    if source.candidates.len() > MAX_CANDIDATES {
        return Err(Error::TooLarge);
    }
    let path = &source.relative_path;
    if bytes(path).is_empty()
        || bytes(path).len() > 4096
        || bytes(path).contains(&0)
        || path
            .components()
            .any(|part| !matches!(part, Component::CurDir | Component::Normal(_)))
        || source.input_bytes > 65536
    {
        return Err(Error::InvalidData);
    }
    Ok(())
}

fn validate_candidate(candidate: &Candidate, input_bytes: usize) -> Result<(), Error> {
    if candidate.canonical.is_empty()
        || candidate.canonical.len() > 256
        || candidate.canonical.chars().any(char::is_control)
        || candidate.span.start >= candidate.span.end
        || candidate.span.end > input_bytes
        || candidate.span.end - candidate.span.start != candidate.canonical.len()
    {
        return Err(Error::InvalidData);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Coverage;
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn discovery(root: &Path) -> Discovery {
        Discovery {
            selected: root.to_path_buf(),
            root: root.to_path_buf(),
            coverage: Coverage::default(),
            sources: vec![
                Source {
                    relative_path: "notes.txt".into(),
                    kind: SourceKind::FileName,
                    fingerprint: [1; 32],
                    input_bytes: 9,
                    candidates: vec![Candidate {
                        canonical: "notes.txt".into(),
                        span: 0..9,
                    }],
                },
                Source {
                    relative_path: "notes.txt".into(),
                    kind: SourceKind::Text,
                    fingerprint: [2; 32],
                    input_bytes: 0,
                    candidates: vec![],
                },
            ],
        }
    }

    #[test]
    fn identity_complete_evidence_and_empty_generation_survive_restart() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("catalog");
        let mut catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        let mut input = discovery(
            &temp
                .path()
                .join(OsString::from_vec(b"project-\xff".to_vec())),
        );
        input.sources.push(Source {
            relative_path: "other.txt".into(),
            kind: SourceKind::Text,
            fingerprint: [3; 32],
            input_bytes: 12,
            candidates: vec![
                Candidate {
                    canonical: "First".into(),
                    span: 0..5,
                },
                Candidate {
                    canonical: "Second".into(),
                    span: 6..12,
                },
            ],
        });
        assert_eq!(
            catalog
                .refresh_cancellable(&input, "pipeline-a", &CancelToken::never())
                .unwrap(),
            1
        );
        assert!(catalog.load(&input.root, "pipeline-b").unwrap().is_none());
        drop(catalog);
        let mut catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        let loaded = catalog.load(&input.root, "pipeline-a").unwrap().unwrap();
        assert_eq!(loaded.revision, 1);
        assert_eq!(loaded.sources, input.sources);
        input.sources.clear();
        assert_eq!(
            catalog
                .refresh_cancellable(&input, "pipeline-b", &CancelToken::never())
                .unwrap(),
            2
        );
        assert!(catalog.load(&input.root, "pipeline-a").unwrap().is_none());
        assert!(
            catalog
                .load(&input.root, "pipeline-b")
                .unwrap()
                .unwrap()
                .sources
                .is_empty()
        );
    }

    #[test]
    fn constraint_failure_and_mid_transaction_cancellation_preserve_generation() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog =
            Catalog::open(&temp.path().join("catalog"), CancelToken::never()).unwrap();
        let mut input = discovery(&temp.path().join("project"));
        catalog
            .refresh_cancellable(&input, "a", &CancelToken::never())
            .unwrap();
        input.sources.push(input.sources[0].clone());
        assert!(
            catalog
                .refresh_cancellable(&input, "b", &CancelToken::never())
                .is_err()
        );
        assert_eq!(catalog.load(&input.root, "a").unwrap().unwrap().revision, 1);
        input.sources.pop();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls;
        let cancel = CancelToken::from_fn(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 6
        });
        assert!(matches!(
            catalog.refresh_cancellable(&input, "b", &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(catalog.load(&input.root, "a").unwrap().unwrap().revision, 1);
        assert!(catalog.load(&input.root, "b").unwrap().is_none());
    }

    #[test]
    fn evidence_limits_fail_without_replacing_the_generation() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog =
            Catalog::open(&temp.path().join("catalog"), CancelToken::never()).unwrap();
        let mut input = discovery(&temp.path().join("project"));
        catalog
            .refresh_cancellable(&input, "a", &CancelToken::never())
            .unwrap();
        input.sources = (0..=MAX_TERMS / MAX_CANDIDATES)
            .map(|index| Source {
                relative_path: format!("source-{index}.txt").into(),
                kind: SourceKind::Text,
                fingerprint: [1; 32],
                input_bytes: MAX_CANDIDATES * 16,
                candidates: (0..MAX_CANDIDATES)
                    .map(|rank| {
                        let canonical = format!("term-{rank}");
                        let span = rank * 16..rank * 16 + canonical.len();
                        Candidate { canonical, span }
                    })
                    .collect(),
            })
            .collect();
        assert!(matches!(
            catalog.refresh_cancellable(&input, "a", &CancelToken::never()),
            Err(Error::TooLarge)
        ));
        input.sources[0].candidates.clear();
        input.sources[0].relative_path = PathBuf::from("x".repeat(4096));
        input.sources = vec![input.sources[0].clone(); MAX_SOURCE_BYTES / (512 + 8192) + 1];
        assert!(matches!(
            catalog.refresh_cancellable(&input, "a", &CancelToken::never()),
            Err(Error::TooLarge)
        ));
        assert_eq!(catalog.load(&input.root, "a").unwrap().unwrap().revision, 1);
    }

    #[test]
    fn per_source_limit_rejects_refresh_and_excess_stored_rows() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog =
            Catalog::open(&temp.path().join("catalog"), CancelToken::never()).unwrap();
        let mut input = discovery(&temp.path().join("project"));
        input.sources.truncate(1);
        let source = &mut input.sources[0];
        source.kind = SourceKind::Text;
        source.input_bytes = (MAX_CANDIDATES + 1) * 16;
        source.candidates = (0..=MAX_CANDIDATES)
            .map(|rank| {
                let canonical = format!("term-{rank}");
                let span = rank * 16..rank * 16 + canonical.len();
                Candidate { canonical, span }
            })
            .collect();
        assert!(matches!(
            catalog.refresh_cancellable(&input, "a", &CancelToken::never()),
            Err(Error::TooLarge)
        ));
        let last = input.sources[0].candidates.pop().unwrap();
        catalog
            .refresh_cancellable(&input, "a", &CancelToken::never())
            .unwrap();
        assert_eq!(
            catalog.load(&input.root, "a").unwrap().unwrap().sources[0]
                .candidates
                .len(),
            MAX_CANDIDATES
        );
        catalog.connection.execute(
            "INSERT INTO terms(source_id,rank,canonical,span_start,span_end) SELECT id,?1,?2,?3,?4 FROM sources",
            params![sql_integer(MAX_CANDIDATES).unwrap(), last.canonical, sql_integer(last.span.start).unwrap(), sql_integer(last.span.end).unwrap()],
        ).unwrap();
        assert!(matches!(
            catalog.load(&input.root, "a"),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn sql_cancellation_interrupts_reads_and_can_resume() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog =
            Catalog::open(&temp.path().join("catalog"), CancelToken::never()).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        catalog.cancel_on(cancel).unwrap();
        let error = catalog.connection.query_row(
            "WITH RECURSIVE numbers(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM numbers WHERE n<1000000) SELECT sum(n) FROM numbers",
            [], |row| row.get::<_, i64>(0)).unwrap_err();
        assert!(matches!(Error::from(error), Error::Cancelled));
        assert!(matches!(
            catalog.load(temp.path(), "a"),
            Err(Error::Cancelled)
        ));
        catalog.cancel_on(CancelToken::never()).unwrap();
        assert!(catalog.load(temp.path(), "a").unwrap().is_none());
    }

    #[test]
    fn migration_discards_only_derived_tables_and_rejects_newer_schema() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("catalog");
        private_fs::prepare_directory(&directory).unwrap();
        let path = directory.join("catalog.db");
        let connection = Connection::open(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE projects(id INTEGER PRIMARY KEY,root BLOB);
            CREATE TABLE indexed_scopes(project_id INTEGER);
            CREATE TABLE sources(id INTEGER PRIMARY KEY,project_id INTEGER);
            CREATE TABLE terms(source_id INTEGER);
            CREATE TABLE personal(value TEXT);
            INSERT INTO projects VALUES(1,x'2f6f6c64');
            INSERT INTO personal VALUES('keep'); PRAGMA user_version=1;",
            )
            .unwrap();
        drop(connection);
        let catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        assert!(catalog.load(Path::new("/old"), "a").unwrap().is_none());
        assert_eq!(
            catalog
                .connection
                .query_row("SELECT value FROM personal", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        catalog
            .connection
            .pragma_update(None, "user_version", 99)
            .unwrap();
        drop(catalog);
        assert!(matches!(
            Catalog::open(&directory, CancelToken::never()),
            Err(Error::UnsupportedSchema(99))
        ));
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            99
        );
    }

    #[test]
    fn cancellation_during_migration_rolls_back_the_old_schema() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("catalog");
        private_fs::prepare_directory(&directory).unwrap();
        let path = directory.join("catalog.db");
        let connection = Connection::open(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE projects(id INTEGER PRIMARY KEY,root BLOB);
             CREATE TABLE indexed_scopes(project_id INTEGER);
             CREATE TABLE sources(id INTEGER PRIMARY KEY,project_id INTEGER);
             CREATE TABLE terms(source_id INTEGER);
             INSERT INTO projects VALUES(1,x'2f6f6c64');
             INSERT INTO sources VALUES(1,1);
             INSERT INTO terms VALUES(1);
             PRAGMA user_version=1;",
            )
            .unwrap();
        drop(connection);
        let journal = directory.join("catalog.db-journal");
        let cancel = CancelToken::from_fn(move || journal.exists());
        assert!(matches!(
            Catalog::open(&directory, cancel),
            Err(Error::Cancelled)
        ));
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM terms", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM sources", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM projects", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            1
        );
        drop(connection);
        assert!(Catalog::open(&directory, CancelToken::never()).is_ok());
    }

    #[test]
    fn failed_migration_rolls_back_derived_table_invalidation() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("catalog");
        let catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        catalog.connection.execute_batch("PRAGMA user_version=1; INSERT INTO projects(root,revision,pipeline_key,accessed,entries,hidden,symlinks,unsupported,oversized,bytes) VALUES(x'2f6f6c64',1,'a',0,0,0,0,0,0,0);").unwrap();
        drop(catalog);
        assert!(Catalog::open(&directory, CancelToken::never()).is_err());
        let connection = Connection::open(directory.join("catalog.db")).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM projects", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn corrupt_candidates_are_rejected_before_loading_unbounded_strings() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog =
            Catalog::open(&temp.path().join("catalog"), CancelToken::never()).unwrap();
        let input = discovery(&temp.path().join("project"));
        catalog
            .refresh_cancellable(&input, "a", &CancelToken::never())
            .unwrap();
        catalog.connection.execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE terms SET canonical=printf('%1000000s','x');").unwrap();
        assert!(matches!(
            catalog.load(&input.root, "a"),
            Err(Error::InvalidData)
        ));
        catalog
            .connection
            .execute_batch("UPDATE terms SET canonical='notes.txt',span_end=10;")
            .unwrap();
        assert!(matches!(
            catalog.load(&input.root, "a"),
            Err(Error::InvalidData)
        ));
    }

    #[test]
    fn forgotten_deleted_alias_removes_all_overlaps_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("catalog");
        let mut catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        let parent = temp.path().join("parent");
        let child = parent.join("child");
        let grandchild = child.join("grandchild");
        let sibling = parent.join("childish");
        for root in [&parent, &child, &grandchild, &sibling] {
            catalog
                .refresh_cancellable(&discovery(root), "a", &CancelToken::never())
                .unwrap();
        }
        fs::create_dir_all(&child).unwrap();
        let alias = temp.path().join("alias");
        symlink(&child, &alias).unwrap();
        catalog.remember_alias(&alias, &child).unwrap();
        fs::remove_file(&alias).unwrap();
        drop(catalog);
        let mut catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            catalog.remove_overlapping(&alias, None, &cancel),
            Err(Error::Cancelled)
        ));
        let removed = catalog
            .remove_overlapping(&alias, None, &CancelToken::never())
            .unwrap();
        assert_eq!(
            removed,
            vec![parent.clone(), child.clone(), grandchild.clone()]
        );
        drop(catalog);
        let catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        for root in [&parent, &child, &grandchild] {
            assert!(catalog.load(root, "a").unwrap().is_none());
        }
        assert!(catalog.load(&sibling, "a").unwrap().is_some());
    }

    #[test]
    fn forgetting_a_retargeted_alias_removes_both_canonical_scopes() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("catalog");
        let old_root = temp.path().join("old");
        let new_root = temp.path().join("new");
        let alias = temp.path().join("alias");
        fs::create_dir(&old_root).unwrap();
        fs::create_dir(&new_root).unwrap();
        let old_root = old_root.canonicalize().unwrap();
        let new_root = new_root.canonicalize().unwrap();
        symlink(&old_root, &alias).unwrap();
        let mut catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        for root in [&old_root, &new_root] {
            catalog
                .refresh_cancellable(&discovery(root), "a", &CancelToken::never())
                .unwrap();
        }
        catalog.remember_alias(&alias, &old_root).unwrap();
        drop(catalog);
        fs::remove_file(&alias).unwrap();
        symlink(&new_root, &alias).unwrap();
        let mut catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        let resolved = alias.canonicalize().unwrap();
        assert_eq!(
            catalog
                .remove_overlapping(&alias, Some(&resolved), &CancelToken::never())
                .unwrap(),
            vec![old_root.clone(), new_root.clone()]
        );
        drop(catalog);
        let catalog = Catalog::open(&directory, CancelToken::never()).unwrap();
        assert!(catalog.load(&old_root, "a").unwrap().is_none());
        assert!(catalog.load(&new_root, "a").unwrap().is_none());
    }

    #[test]
    fn private_storage_rejects_database_and_sidecar_links_and_public_files() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("catalog");
        drop(Catalog::open(&directory, CancelToken::never()).unwrap());
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        let path = directory.join("catalog.db");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        symlink(&path, directory.join("catalog.db-wal")).unwrap();
        assert!(Catalog::open(&directory, CancelToken::never()).is_err());
        fs::remove_file(directory.join("catalog.db-wal")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Catalog::open(&directory, CancelToken::never()),
            Err(Error::File(_))
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(&path, directory.join("original.db")).unwrap();
        symlink(directory.join("original.db"), &path).unwrap();
        assert!(Catalog::open(&directory, CancelToken::never()).is_err());
    }
}
