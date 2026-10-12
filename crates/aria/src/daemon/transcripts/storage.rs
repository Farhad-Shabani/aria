use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aria_kit::primitives::private_fs;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use rustix::fs::{Mode, OFlags};

use crate::audio::RecordingReader;
use crate::history::{
    Application, AudioAvailability, ClearHistoryResult, HistoryEntry, HistoryError, HistoryPage,
    HistoryQuery, HistorySource, HistoryStatus,
};
use crate::rpc::{RpcErrorCode, RpcFailure};

const MAX_ENTRIES: usize = 1_000;
const MAX_ERROR_MESSAGE_BYTES: usize = 256;
const MAX_STORED_ERROR_BYTES: usize = 2 * 1024;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_ENTRY_BYTES: usize = 128 * 1024;
const MAX_AUDIO_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DATABASE_BYTES: u64 = 32 * 1024 * 1024;
const TEXT_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const AUDIO_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const CLEANUP_WARNING: &str = "Some history files could not be removed. Cleanup will be retried.";
const AUDIO_WARNING: &str = "The recording could not be saved. Audio recovery is unavailable.";
const UNREADABLE_WARNING: &str = "Some unreadable history entries were removed.";
const COLUMNS: &str = "id,created,source,app_name,bundle_id,duration,text,complete,status,error,expires,audio_present,deleting,language,own_voice,project,engine";

pub(in crate::daemon) struct Store {
    directory: PathBuf,
    audio_directory: File,
    // The one lock also bounds audio copies. Copies need their own admission
    // if reads stall.
    state: Mutex<State>,
}

struct State {
    connection: Connection,
    pinned: HashSet<u64>,
    warning: Option<String>,
    _database: File,
    _directory: File,
    // Release ownership after closing SQLite.
    _ownership: File,
}

pub(in crate::daemon) struct RetryLease {
    pub(in crate::daemon) entry: HistoryEntry,
    pub(in crate::daemon) audio_path: PathBuf,
    store: Arc<Store>,
}

impl Drop for RetryLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.store.state.lock() {
            state.pinned.remove(&self.entry.id);
        }
        if self.store.maintenance().is_err() {
            tracing::warn!("dictation history cleanup failed after retry");
        }
    }
}

impl Store {
    pub(in crate::daemon) fn open(directory: &Path) -> Result<Arc<Self>, HistoryError> {
        private_fs::prepare_directory(directory).map_err(|_| HistoryError::Storage)?;
        let directory = directory
            .canonicalize()
            .map_err(|_| HistoryError::Storage)?;
        let directory_file = open_directory(&directory)?;
        let ownership = open_private(&directory_file, "history.lock", true)?;
        ownership.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => HistoryError::Busy,
            TryLockError::Error(_) => HistoryError::Storage,
        })?;
        let database = open_private(&directory_file, "history.db", true)?;
        check_size(&database, MAX_DATABASE_BYTES)?;
        for name in ["history.db-journal", "history.db-wal", "history.db-shm"] {
            match open_private(&directory_file, name, false) {
                Ok(file) => check_size(&file, MAX_DATABASE_BYTES + 1024 * 1024)?,
                Err(HistoryError::Missing) => {}
                Err(error) => return Err(error),
            }
        }
        let before = database.metadata().map_err(|_| HistoryError::Storage)?;
        let connection = Connection::open_with_flags(
            directory.join("history.db"),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let after = fs::symlink_metadata(directory.join("history.db"))
            .map_err(|_| HistoryError::Storage)?;
        if !after.is_file() || before.ino() != after.ino() || before.dev() != after.dev() {
            return Err(HistoryError::Storage);
        }
        connection.busy_timeout(Duration::from_millis(250))?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 1 {
            return Err(HistoryError::Unavailable);
        }
        connection.execute_batch(
            "PRAGMA trusted_schema=OFF;
             PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             PRAGMA journal_size_limit=0;
             PRAGMA secure_delete=ON;",
        )?;
        let page_size: u32 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
        connection.pragma_update(
            None,
            "max_page_count",
            sql_integer(MAX_DATABASE_BYTES / u64::from(page_size))?,
        )?;
        connection.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created INTEGER NOT NULL CHECK(created>=0),
                source INTEGER NOT NULL CHECK(source IN (0,1)),
                app_name TEXT, bundle_id TEXT, duration INTEGER,
                text TEXT NOT NULL CHECK(length(CAST(text AS BLOB))<={MAX_ENTRY_BYTES}),
                complete INTEGER NOT NULL CHECK(complete IN (0,1)),
                status INTEGER NOT NULL CHECK(status IN (0,1,2,3)), error TEXT,
                expires INTEGER, audio_present INTEGER NOT NULL DEFAULT 0,
                deleting INTEGER NOT NULL DEFAULT 0,
                language TEXT NOT NULL, own_voice INTEGER NOT NULL,
                project BLOB, engine TEXT NOT NULL);
             PRAGMA user_version=1;"
        ))?;
        private_fs::prepare_directory(&directory.join("audio"))
            .map_err(|_| HistoryError::Storage)?;
        let audio_directory = open_directory(&directory.join("audio"))?;
        let store = Arc::new(Self {
            directory,
            audio_directory,
            state: Mutex::new(State {
                connection,
                pinned: HashSet::new(),
                warning: None,
                _database: database,
                _directory: directory_file,
                _ownership: ownership,
            }),
        });
        store.maintenance()?;
        Ok(store)
    }

    pub(in crate::daemon) fn insert(
        &self,
        mut entry: HistoryEntry,
        audio: Option<&Path>,
    ) -> Result<HistoryEntry, HistoryError> {
        validate_entry(&entry)?;
        let mut state = self.lock()?;
        self.maintain(&mut state)?;
        let copy = if state.warning.is_some() && audio.is_some() {
            Err(HistoryError::Storage)
        } else {
            audio.map(|path| self.copy_audio(path)).transpose()
        };
        let mut copied = match copy {
            Ok(copy) => copy,
            Err(_) => {
                state.warning = Some(AUDIO_WARNING.to_owned());
                entry.warning = Some(AUDIO_WARNING.to_owned());
                None
            }
        };
        // Free the space before committing: a full database cannot evict anything.
        if let Err(error) = self.make_room(
            &mut state,
            entry.text.len(),
            copied.as_ref().map_or(0, |(_, size)| *size),
            true,
        ) {
            if copied.is_none() {
                return Err(error);
            }
            copied = None;
            entry.warning = Some(AUDIO_WARNING.to_owned());
            self.make_room(&mut state, entry.text.len(), 0, true)?;
        }
        let transaction = state.connection.unchecked_transaction()?;
        let app_name = entry.application.as_ref().map(|app| app.name.as_str());
        let bundle_id = entry
            .application
            .as_ref()
            .and_then(|app| app.bundle_id.as_deref());
        transaction.execute(
            "INSERT INTO entries(created,source,app_name,bundle_id,duration,text,complete,status,error,language,own_voice,project,engine)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![sql_integer(entry.created_at_ms)?, source_number(entry.source), app_name, bundle_id,
                entry.duration_ms.map(sql_integer).transpose()?, entry.text, entry.complete, status_number(entry.status),
                entry.error.as_ref().map(encode_error).transpose()?, entry.language, entry.own_voice_only,
                entry.project.as_ref().map(|path| path.as_os_str().as_bytes()), entry.engine],
        )?;
        let id = unsigned(transaction.last_insert_rowid(), 0)?;
        if let Some((copy, _)) = copied {
            let expires = now_ms().saturating_add(AUDIO_TTL_MS);
            let published = copy.persist_noclobber(self.audio_path(id)).is_ok()
                && self.audio_directory.sync_all().is_ok();
            if published {
                transaction.execute(
                    "UPDATE entries SET expires=?1,audio_present=1 WHERE id=?2",
                    params![sql_integer(expires)?, sql_integer(id)?],
                )?;
            } else {
                entry.warning = Some(AUDIO_WARNING.to_owned());
            }
        }
        transaction.commit()?;
        let mut saved = load(&state, id)?;
        saved.warning = entry.warning;
        Ok(saved)
    }

    pub(in crate::daemon) fn list(
        &self,
        query: &HistoryQuery,
    ) -> Result<HistoryPage, HistoryError> {
        if query.query.len() > 256 || query.limit == 0 || query.limit > 100 {
            return Err(HistoryError::InvalidRequest);
        }
        if let Some(id) = query.before_id {
            validate_id(id)?;
        }
        let mut state = self.lock()?;
        self.maintain(&mut state)?;
        let needle = query.query.to_lowercase();
        let (entries, _) = all(&state)?;
        let mut entries = entries
            .into_iter()
            .filter(|entry| query.before_id.is_none_or(|id| entry.id < id))
            .filter(|entry| {
                entry.text.to_lowercase().contains(&needle)
                    || entry
                        .application
                        .as_ref()
                        .is_some_and(|app| app.name.to_lowercase().contains(&needle))
            })
            .take(query.limit + 1)
            .collect::<Vec<_>>();
        let next_before_id = (entries.len() > query.limit).then(|| entries[query.limit - 1].id);
        entries.truncate(query.limit);
        Ok(HistoryPage {
            entries,
            next_before_id,
            warning: state.warning.clone(),
        })
    }

    pub(in crate::daemon) fn get(&self, id: u64) -> Result<HistoryEntry, HistoryError> {
        validate_id(id)?;
        let mut state = self.lock()?;
        self.maintain(&mut state)?;
        load(&state, id)
    }

    pub(in crate::daemon) fn delete(&self, id: u64) -> Result<bool, HistoryError> {
        validate_id(id)?;
        let mut state = self.lock()?;
        if state.pinned.contains(&id) {
            return Err(HistoryError::Busy);
        }
        match load(&state, id) {
            Ok(_) => {}
            Err(HistoryError::Missing) => return Ok(false),
            Err(error) => return Err(error),
        }
        self.mark_delete(&mut state, id)?;
        self.remove_entry(&mut state, id)?;
        Ok(true)
    }

    pub(in crate::daemon) fn clear(&self) -> Result<ClearHistoryResult, HistoryError> {
        let mut state = self.lock()?;
        if !state.pinned.is_empty() {
            return Err(HistoryError::Busy);
        }
        let deleted_entries = state.connection.execute("DELETE FROM entries", [])? as u64;
        state.warning = None;
        if self.remove_orphans(&mut state).is_err() {
            state.warning = Some(CLEANUP_WARNING.to_owned());
        }
        Ok(ClearHistoryResult {
            deleted_entries,
            cleanup_pending: state.warning.is_some(),
        })
    }

    pub(in crate::daemon) fn begin_retry(
        self: &Arc<Self>,
        id: u64,
    ) -> Result<RetryLease, HistoryError> {
        validate_id(id)?;
        let mut state = self.lock()?;
        self.maintain(&mut state)?;
        let mut entry = load(&state, id)?;
        if state.pinned.contains(&id) {
            return Err(HistoryError::Busy);
        }
        if entry.deletion_pending || !matches!(entry.audio, AudioAvailability::Available { .. }) {
            return Err(HistoryError::AudioUnavailable);
        }
        let audio_path = self.audio_path(id);
        let validated = (|| -> Result<(), HistoryError> {
            let file = open_private(&self.audio_directory, &audio_name(id), false)?;
            check_size(&file, MAX_AUDIO_BYTES)?;
            RecordingReader::from_wav_file(&audio_path)
                .map_err(|_| HistoryError::AudioUnavailable)?;
            Ok(())
        })();
        if validated.is_err() {
            self.invalidate_audio(&mut state, id)?;
            return Err(HistoryError::AudioUnavailable);
        }
        state.pinned.insert(id);
        entry.retrying = true;
        Ok(RetryLease {
            entry,
            audio_path,
            store: Arc::clone(self),
        })
    }

    pub(in crate::daemon) fn recover(
        &self,
        id: u64,
        text: &str,
        engine: &str,
        language: &str,
        retain_audio: bool,
    ) -> Result<HistoryEntry, HistoryError> {
        validate_id(id)?;
        if text.len() > MAX_ENTRY_BYTES {
            return Err(HistoryError::TooLarge);
        }
        let mut state = self.lock()?;
        if !state.pinned.contains(&id) {
            return Err(HistoryError::InvalidRequest);
        }
        let entry = load(&state, id)?;
        if entry.deletion_pending {
            return Err(HistoryError::InvalidRequest);
        }
        self.make_room_for_text(&mut state, text.len().saturating_sub(entry.text.len()))?;
        state.connection.execute(
            "UPDATE entries SET text=?1,complete=1,status=3,error=NULL,engine=?3,language=?4,
             expires=CASE WHEN ?5 THEN expires ELSE NULL END WHERE id=?2",
            params![text, sql_integer(id)?, engine, language, retain_audio],
        )?;
        // A cleared expiry keeps failed unlinks pending for maintenance.
        let warning = if !retain_audio && self.remove_audio(&mut state, id).is_err() {
            state.warning = Some(CLEANUP_WARNING.to_owned());
            Some(CLEANUP_WARNING.to_owned())
        } else {
            None
        };
        let mut entry = load(&state, id)?;
        entry.warning = warning;
        Ok(entry)
    }

    pub(in crate::daemon) fn attempt_failed(
        &self,
        id: u64,
        error: HistoryError,
    ) -> Result<(), HistoryError> {
        validate_id(id)?;
        let error = encode_error(&error.into())?;
        let state = self.lock()?;
        if !state.pinned.contains(&id) {
            return Err(HistoryError::InvalidRequest);
        }
        state.connection.execute(
            "UPDATE entries SET error=?1 WHERE id=?2",
            params![error, sql_integer(id)?],
        )?;
        Ok(())
    }

    pub(in crate::daemon) fn maintenance(&self) -> Result<(), HistoryError> {
        let mut state = self.lock()?;
        self.maintain(&mut state)
    }

    fn maintain(&self, state: &mut State) -> Result<(), HistoryError> {
        state.warning = None;
        let now = now_ms();
        let (entries, unreadable) = all(state)?;
        for id in unreadable {
            if state.pinned.contains(&id) {
                continue;
            }
            self.mark_delete(state, id)?;
            state.warning = Some(match self.remove_entry(state, id) {
                Ok(()) => UNREADABLE_WARNING.to_owned(),
                Err(_) => CLEANUP_WARNING.to_owned(),
            });
        }
        for entry in entries {
            if state.pinned.contains(&entry.id) {
                continue;
            }
            if entry.deletion_pending || now.saturating_sub(entry.created_at_ms) >= TEXT_TTL_MS {
                self.mark_delete(state, entry.id)?;
                if self.remove_entry(state, entry.id).is_err() {
                    state.warning = Some(CLEANUP_WARNING.to_owned());
                }
                continue;
            }
            let expired = match entry.audio {
                AudioAvailability::Available { expires_at_ms } => expires_at_ms <= now,
                AudioAvailability::Expired => true,
                AudioAvailability::Unavailable => false,
            };
            if expired || entry.audio == AudioAvailability::Unavailable {
                if self.remove_audio(state, entry.id).is_err() {
                    state.warning = Some(CLEANUP_WARNING.to_owned());
                }
            } else if matches!(entry.audio, AudioAvailability::Available { .. })
                && open_private(&self.audio_directory, &audio_name(entry.id), false).is_err()
            {
                self.invalidate_audio(state, entry.id)?;
            }
        }
        self.remove_orphans(state)?;
        if self.make_room(state, 0, 0, false).is_err() {
            state.warning = Some(CLEANUP_WARNING.to_owned());
        }
        Ok(())
    }

    fn mark_delete(&self, state: &mut State, id: u64) -> Result<(), HistoryError> {
        state.connection.execute(
            "UPDATE entries SET deleting=1 WHERE id=?1",
            [sql_integer(id)?],
        )?;
        Ok(())
    }

    fn remove_entry(&self, state: &mut State, id: u64) -> Result<(), HistoryError> {
        // The row goes first: a failed unlink leaves an orphan the sweep reclaims,
        // where a failed unlink before it would keep the transcript readable.
        state
            .connection
            .execute("DELETE FROM entries WHERE id=?1", [sql_integer(id)?])?;
        if self.unlink_audio(&audio_name(id)).is_err() {
            state.warning = Some(CLEANUP_WARNING.to_owned());
        }
        Ok(())
    }

    fn invalidate_audio(&self, state: &mut State, id: u64) -> Result<(), HistoryError> {
        // Commit the loss of availability first. The unlink can fail on its own.
        state.connection.execute(
            "UPDATE entries SET expires=NULL WHERE id=?1",
            [sql_integer(id)?],
        )?;
        if self.remove_audio(state, id).is_err() {
            state.warning = Some(CLEANUP_WARNING.to_owned());
        }
        Ok(())
    }

    fn remove_audio(&self, state: &mut State, id: u64) -> Result<(), HistoryError> {
        let present: bool = state.connection.query_row(
            "SELECT audio_present FROM entries WHERE id=?1",
            [sql_integer(id)?],
            |row| row.get(0),
        )?;
        if !present {
            return Ok(());
        }
        self.unlink_audio(&audio_name(id))?;
        state.connection.execute(
            "UPDATE entries SET audio_present=0 WHERE id=?1",
            [sql_integer(id)?],
        )?;
        Ok(())
    }

    fn unlink_audio(&self, name: &str) -> Result<(), HistoryError> {
        // Removal reads no content and never follows the final component, so a file
        // whose mode or link count drifted must still be removable.
        match rustix::fs::unlinkat(&self.audio_directory, name, rustix::fs::AtFlags::empty()) {
            Ok(()) => {}
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(_) => return Err(HistoryError::Storage),
        }
        self.audio_directory
            .sync_all()
            .map_err(|_| HistoryError::Storage)?;
        Ok(())
    }

    fn remove_orphans(&self, state: &mut State) -> Result<(), HistoryError> {
        let mut statement = state
            .connection
            .prepare("SELECT id FROM entries WHERE audio_present=1")?;
        let live = statement
            .query_map([], |row| unsigned(row.get(0)?, 0))?
            .map(|id| id.map(audio_name))
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        for item in fs::read_dir(self.directory.join("audio")).map_err(|_| HistoryError::Storage)? {
            let item = item.map_err(|_| HistoryError::Storage)?;
            let name = item.file_name();
            let Some(name) = name.to_str() else { continue };
            let owned_name = name
                .strip_suffix(".wav")
                .is_some_and(|id| id.parse::<u64>().is_ok_and(|id| audio_name(id) == name))
                || name.starts_with(".copy-");
            if owned_name && !live.contains(name) && self.unlink_audio(name).is_err() {
                state.warning = Some(CLEANUP_WARNING.to_owned());
            }
        }
        Ok(())
    }

    fn make_room(
        &self,
        state: &mut State,
        text_bytes: usize,
        audio_bytes: u64,
        reserve_entry: bool,
    ) -> Result<(), HistoryError> {
        self.make_room_for_text(state, text_bytes)?;
        let (mut entries, _) = all(state)?;
        // An insert reserves a slot even when its transcript is empty.
        let limit = if reserve_entry {
            MAX_ENTRIES - 1
        } else {
            MAX_ENTRIES
        };
        while entries.len() > limit {
            let position = entries
                .iter()
                .rposition(|entry| !state.pinned.contains(&entry.id))
                .ok_or(HistoryError::Busy)?;
            let entry = entries.remove(position);
            self.mark_delete(state, entry.id)?;
            self.remove_entry(state, entry.id)?;
        }
        if reserve_entry && audio_bytes == 0 {
            return Ok(());
        }
        let mut retained_audio = 0;
        for entry in &entries {
            match open_private(&self.audio_directory, &audio_name(entry.id), false) {
                Ok(file) => {
                    retained_audio += file.metadata().map_err(|_| HistoryError::Storage)?.len();
                }
                Err(HistoryError::Missing) => {}
                Err(_) => return Err(HistoryError::Storage),
            }
        }
        for entry in entries.iter().rev() {
            if retained_audio.saturating_add(audio_bytes) <= MAX_AUDIO_BYTES {
                break;
            }
            if state.pinned.contains(&entry.id) {
                continue;
            }
            if let Ok(file) = open_private(&self.audio_directory, &audio_name(entry.id), false) {
                let size = file.metadata().map_err(|_| HistoryError::Storage)?.len();
                self.remove_audio(state, entry.id)?;
                retained_audio = retained_audio.saturating_sub(size);
            }
        }
        if retained_audio.saturating_add(audio_bytes) > MAX_AUDIO_BYTES {
            return Err(HistoryError::Busy);
        }
        Ok(())
    }

    fn make_room_for_text(&self, state: &mut State, added: usize) -> Result<(), HistoryError> {
        let (entries, _) = all(state)?;
        let mut bytes = entries.iter().map(|entry| entry.text.len()).sum::<usize>();
        for entry in entries.iter().rev() {
            if bytes.saturating_add(added) <= MAX_TEXT_BYTES {
                break;
            }
            if state.pinned.contains(&entry.id) {
                continue;
            }
            self.mark_delete(state, entry.id)?;
            self.remove_entry(state, entry.id)?;
            bytes -= entry.text.len();
        }
        if bytes.saturating_add(added) > MAX_TEXT_BYTES {
            return Err(HistoryError::TooLarge);
        }
        Ok(())
    }

    fn copy_audio(&self, source: &Path) -> Result<(tempfile::NamedTempFile, u64), HistoryError> {
        let descriptor = rustix::fs::open(
            source,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| HistoryError::AudioUnavailable)?;
        let source = File::from(descriptor);
        let metadata =
            private_fs::verify_file(&source).map_err(|_| HistoryError::AudioUnavailable)?;
        if metadata.nlink() != 1 || metadata.len() > MAX_AUDIO_BYTES {
            return Err(HistoryError::AudioUnavailable);
        }
        let mut copy = tempfile::Builder::new()
            .prefix(".copy-")
            .tempfile_in(self.directory.join("audio"))
            .map_err(|_| HistoryError::Storage)?;
        let bytes = io::copy(&mut source.take(MAX_AUDIO_BYTES + 1), copy.as_file_mut())
            .map_err(|_| HistoryError::Storage)?;
        if bytes > MAX_AUDIO_BYTES || bytes != metadata.len() {
            return Err(HistoryError::AudioUnavailable);
        }
        copy.as_file()
            .sync_all()
            .map_err(|_| HistoryError::Storage)?;
        RecordingReader::from_wav_file(copy.path()).map_err(|_| HistoryError::AudioUnavailable)?;
        Ok((copy, bytes))
    }

    fn audio_path(&self, id: u64) -> PathBuf {
        self.directory.join("audio").join(audio_name(id))
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, HistoryError> {
        self.state.lock().map_err(|_| HistoryError::Storage)
    }
}

fn sql_integer(value: u64) -> Result<i64, HistoryError> {
    i64::try_from(value).map_err(|_| HistoryError::InvalidRequest)
}

fn unsigned(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(column, value))
}

fn validate_id(id: u64) -> Result<(), HistoryError> {
    if id == 0 || id > i64::MAX as u64 {
        return Err(HistoryError::InvalidRequest);
    }
    Ok(())
}

fn validate_entry(entry: &HistoryEntry) -> Result<(), HistoryError> {
    if entry.text.len() > MAX_ENTRY_BYTES {
        return Err(HistoryError::TooLarge);
    }
    if entry.created_at_ms > i64::MAX as u64
        || entry
            .duration_ms
            .is_some_and(|duration| duration > i64::MAX as u64)
        || entry.engine.len() > 128
        || entry.language.len() > 64
        || entry.language.is_empty()
        || entry
            .error
            .as_ref()
            .is_some_and(|error| error.message.len() > MAX_ERROR_MESSAGE_BYTES)
        || entry
            .project
            .as_ref()
            .is_some_and(|path| !path.is_absolute() || path.as_os_str().len() > 4096)
        || entry.application.as_ref().is_some_and(|app| {
            app.name.len() > 256 || app.bundle_id.as_ref().is_some_and(|id| id.len() > 256)
        })
    {
        return Err(HistoryError::InvalidRequest);
    }
    Ok(())
}

fn audio_name(id: u64) -> String {
    format!("{id}.wav")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn open_directory(path: &Path) -> Result<File, HistoryError> {
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| HistoryError::Storage)?;
    let file = File::from(descriptor);
    let metadata = file.metadata().map_err(|_| HistoryError::Storage)?;
    if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o777 != 0o700 {
        return Err(HistoryError::Storage);
    }
    Ok(file)
}

fn open_private(directory: &File, name: &str, create: bool) -> Result<File, HistoryError> {
    let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if create {
        flags |= OFlags::CREATE;
    }
    let descriptor =
        rustix::fs::openat(directory, name, flags, Mode::RUSR | Mode::WUSR).map_err(|error| {
            if error == rustix::io::Errno::NOENT {
                HistoryError::Missing
            } else {
                HistoryError::Storage
            }
        })?;
    let file = File::from(descriptor);
    let metadata = private_fs::verify_file(&file).map_err(|_| HistoryError::Storage)?;
    if metadata.nlink() != 1 {
        return Err(HistoryError::Storage);
    }
    Ok(file)
}

fn check_size(file: &File, limit: u64) -> Result<(), HistoryError> {
    if file.metadata().map_err(|_| HistoryError::Storage)?.len() > limit {
        return Err(HistoryError::TooLarge);
    }
    Ok(())
}

fn source_number(source: HistorySource) -> u8 {
    match source {
        HistorySource::Local => 0,
        HistorySource::Relay => 1,
    }
}

fn status_number(status: HistoryStatus) -> u8 {
    match status {
        HistoryStatus::Success => 0,
        HistoryStatus::TranscriptionFailed => 1,
        HistoryStatus::DeliveryFailed => 2,
        HistoryStatus::Recovered => 3,
    }
}

/// Keeps the wire code number, which outlives renames of `RpcErrorCode` variants.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredError {
    code: i32,
    message: String,
}

fn encode_error(error: &RpcFailure) -> Result<String, HistoryError> {
    if error.message.len() > MAX_ERROR_MESSAGE_BYTES || error.code == RpcErrorCode::Unknown(0) {
        return Err(HistoryError::InvalidRequest);
    }
    let stored = StoredError {
        code: error.code.into(),
        message: error.message.clone(),
    };
    let stored = serde_json::to_string(&stored).map_err(|_| HistoryError::Storage)?;
    if stored.len() > MAX_STORED_ERROR_BYTES {
        return Err(HistoryError::InvalidRequest);
    }
    Ok(stored)
}

fn decode_error(stored: String) -> rusqlite::Result<RpcFailure> {
    if stored.len() > MAX_STORED_ERROR_BYTES {
        return Err(rusqlite::Error::InvalidQuery);
    }
    if stored.trim_start().starts_with('{') {
        let stored: StoredError = serde_json::from_str(&stored).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        if stored.message.len() > MAX_ERROR_MESSAGE_BYTES {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let code =
            RpcErrorCode::try_from(stored.code).map_err(|_| rusqlite::Error::InvalidQuery)?;
        return Ok(RpcFailure::new(code, stored.message));
    }
    if stored.len() > MAX_ERROR_MESSAGE_BYTES {
        return Err(rusqlite::Error::InvalidQuery);
    }
    // A non-JSON row holds plain text from an older daemon.
    Ok(RpcFailure::new(RpcErrorCode::Internal, stored))
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    let expires = row
        .get::<_, Option<i64>>(10)?
        .map(|value| unsigned(value, 10))
        .transpose()?;
    let audio_present: bool = row.get(11)?;
    let status = match row.get::<_, u8>(8)? {
        0 => HistoryStatus::Success,
        1 => HistoryStatus::TranscriptionFailed,
        2 => HistoryStatus::DeliveryFailed,
        3 => HistoryStatus::Recovered,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let audio = match expires {
        Some(expires_at_ms) if expires_at_ms <= now_ms() => AudioAvailability::Expired,
        Some(expires_at_ms) if audio_present => AudioAvailability::Available { expires_at_ms },
        Some(_) | None => AudioAvailability::Unavailable,
    };
    Ok(HistoryEntry {
        id: unsigned(row.get(0)?, 0)?,
        created_at_ms: unsigned(row.get(1)?, 1)?,
        source: match row.get::<_, u8>(2)? {
            0 => HistorySource::Local,
            1 => HistorySource::Relay,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        application: row
            .get::<_, Option<String>>(3)?
            .map(|name| -> rusqlite::Result<Application> {
                Ok(Application {
                    name,
                    bundle_id: row.get(4)?,
                })
            })
            .transpose()?,
        duration_ms: row
            .get::<_, Option<i64>>(5)?
            .map(|value| unsigned(value, 5))
            .transpose()?,
        text: row.get(6)?,
        complete: row.get(7)?,
        status,
        error: row
            .get::<_, Option<String>>(9)?
            .map(decode_error)
            .transpose()?,
        audio,
        retrying: false,
        deletion_pending: row.get(12)?,
        language: row.get(13)?,
        engine: row.get(16)?,
        own_voice_only: row.get(14)?,
        project: row
            .get::<_, Option<Vec<u8>>>(15)?
            .map(|bytes| PathBuf::from(OsString::from_vec(bytes))),
        warning: None,
    })
}

fn load(state: &State, id: u64) -> Result<HistoryEntry, HistoryError> {
    let mut entry = state
        .connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM entries WHERE id=?1"),
            [sql_integer(id)?],
            decode,
        )
        .optional()?
        .ok_or(HistoryError::Missing)?;
    entry.retrying = state.pinned.contains(&id);
    if entry.status == HistoryStatus::Recovered {
        entry.warning = state.warning.clone();
    }
    Ok(entry)
}

// Rows that fail to decode come back as ids, so one damaged row cannot hide
// the rest of the history.
fn all(state: &State) -> Result<(Vec<HistoryEntry>, Vec<u64>), HistoryError> {
    let mut statement = state
        .connection
        .prepare(&format!("SELECT {COLUMNS} FROM entries ORDER BY id DESC"))?;
    let rows = statement.query_map([], |row| Ok((unsigned(row.get(0)?, 0)?, decode(row))))?;
    let mut entries = Vec::new();
    let mut unreadable = Vec::new();
    for row in rows {
        let (id, decoded) = row?;
        match decoded {
            Ok(mut entry) => {
                entry.retrying = state.pinned.contains(&entry.id);
                entries.push(entry);
            }
            Err(error) => {
                tracing::warn!(id, %error, "history entry is unreadable");
                unreadable.push(id);
            }
        }
    }
    Ok((entries, unreadable))
}

impl From<rusqlite::Error> for HistoryError {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(cause, _)
                if matches!(
                    cause.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                Self::Busy
            }
            rusqlite::Error::SqliteFailure(cause, _)
                if cause.code == rusqlite::ErrorCode::DiskFull =>
            {
                Self::TooLarge
            }
            _ => Self::Storage,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn entry(text: &str, status: HistoryStatus) -> HistoryEntry {
        HistoryEntry {
            id: 0,
            created_at_ms: now_ms(),
            source: HistorySource::Local,
            application: Some(Application {
                name: "Editor".into(),
                bundle_id: Some("test.editor".into()),
            }),
            duration_ms: Some(1_000),
            text: text.into(),
            complete: status != HistoryStatus::TranscriptionFailed,
            status,
            error: None,
            audio: AudioAvailability::Unavailable,
            retrying: false,
            deletion_pending: false,
            language: "en".into(),
            engine: "test".into(),
            own_voice_only: true,
            project: Some(PathBuf::from("/project")),
            warning: None,
        }
    }

    fn audio(directory: &Path) -> PathBuf {
        let path = crate::audio::canonical_fixture(directory);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn query(text: &str, before_id: Option<u64>, limit: usize) -> HistoryQuery {
        HistoryQuery {
            query: text.into(),
            before_id,
            limit,
        }
    }

    #[test]
    fn typed_errors_and_existing_plain_errors_survive_reopening() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let failures = [
            RpcFailure::new(
                RpcErrorCode::DeliveryTargetChanged,
                "Delivery target changed.",
            ),
            HistoryError::ProjectUnavailable.into(),
            RpcFailure::new(RpcErrorCode::Unknown(9123), "Unrecognized failure."),
        ];
        let mut saved = Vec::new();
        for failure in failures {
            let mut original = entry("keep transcript", HistoryStatus::TranscriptionFailed);
            original.error = Some(failure.clone());
            let id = store.insert(original, None).unwrap().id;
            saved.push((id, failure));
        }
        for text in ["transcription_failed", "dictation history retry timed out"] {
            let id = store
                .insert(entry("keep transcript", HistoryStatus::Success), None)
                .unwrap()
                .id;
            store
                .lock()
                .unwrap()
                .connection
                .execute(
                    "UPDATE entries SET error=?1 WHERE id=?2",
                    params![text, sql_integer(id).unwrap()],
                )
                .unwrap();
            saved.push((id, RpcFailure::new(RpcErrorCode::Internal, text)));
        }
        drop(store);
        let reopened = Store::open(directory.path()).unwrap();
        for (id, expected) in saved {
            let found = reopened.get(id).unwrap();
            assert_eq!(found.text, "keep transcript");
            assert_eq!(found.error, Some(expected));
        }
    }

    #[test]
    fn maintenance_removes_a_damaged_row_and_keeps_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let kept = store
            .insert(entry("kept", HistoryStatus::Success), None)
            .unwrap();
        let damaged = store
            .insert(entry("damaged", HistoryStatus::Success), None)
            .unwrap();
        store
            .lock()
            .unwrap()
            .connection
            .execute(
                "UPDATE entries SET error='{broken' WHERE id=?1",
                params![sql_integer(damaged.id).unwrap()],
            )
            .unwrap();

        let page = store.list(&query("", None, 10)).unwrap();

        assert_eq!(
            page.entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [kept.id]
        );
        assert_eq!(page.warning.as_deref(), Some(UNREADABLE_WARNING));
        assert!(matches!(store.get(damaged.id), Err(HistoryError::Missing)));
    }

    #[test]
    fn malformed_or_oversized_stored_errors_are_rejected() {
        for error in [
            "{broken".to_owned(),
            "  {broken".to_owned(),
            r#"{"code":0,"message":"missing category"}"#.to_owned(),
            r#"{"code":"not_a_code","message":"broken"}"#.to_owned(),
            serde_json::to_string(&StoredError {
                code: RpcErrorCode::Internal.into(),
                message: "x".repeat(MAX_ERROR_MESSAGE_BYTES + 1),
            })
            .unwrap(),
            "x".repeat(MAX_STORED_ERROR_BYTES + 1),
        ] {
            assert!(decode_error(error).is_err());
        }
        let failure = RpcFailure::new(RpcErrorCode::Internal, "\0".repeat(MAX_ERROR_MESSAGE_BYTES));
        let encoded = encode_error(&failure).unwrap();
        assert_eq!(decode_error(encoded).unwrap(), failure);
    }

    #[test]
    fn history_reopens_with_metadata_and_never_reuses_deleted_ids() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let first = store
            .insert(entry("private words", HistoryStatus::DeliveryFailed), None)
            .unwrap();
        let created = first.created_at_ms;
        drop(store);
        let store = Store::open(directory.path()).unwrap();
        let found = store.get(first.id).unwrap();
        assert_eq!(found.text, "private words");
        assert_eq!(found.created_at_ms, created);
        assert_eq!(
            found.application.unwrap().bundle_id.as_deref(),
            Some("test.editor")
        );
        assert_eq!(found.project.as_deref(), Some(Path::new("/project")));
        assert!(found.own_voice_only);
        assert_eq!(found.engine, "test");
        assert!(store.delete(first.id).unwrap());
        assert!(!store.delete(first.id).unwrap());
        let second = store
            .insert(entry("next", HistoryStatus::Success), None)
            .unwrap();
        assert!(second.id > first.id);
        assert_eq!(
            fs::metadata(directory.path()).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.path().join("history.db"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn history_clear_removes_every_status_and_audio_without_reusing_ids() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let history = directory.path().join("history");
        let store = Store::open(&history).unwrap();
        let mut last_id = 0;
        for status in [
            HistoryStatus::Success,
            HistoryStatus::TranscriptionFailed,
            HistoryStatus::DeliveryFailed,
            HistoryStatus::Recovered,
        ] {
            for origin in [HistorySource::Local, HistorySource::Relay] {
                let mut saved = entry("private words", status);
                saved.source = origin;
                last_id = store.insert(saved, Some(&source)).unwrap().id;
            }
        }
        store
            .mark_delete(&mut store.lock().unwrap(), last_id)
            .unwrap();
        fs::write(history.join("audio/9999.wav"), "orphan").unwrap();
        fs::write(history.join("audio/.copy-interrupted"), "partial").unwrap();
        assert_eq!(
            store.clear().unwrap(),
            ClearHistoryResult {
                deleted_entries: 8,
                cleanup_pending: false,
            }
        );
        assert!(
            store
                .list(&query("", None, 100))
                .unwrap()
                .entries
                .is_empty()
        );
        assert_eq!(fs::read_dir(history.join("audio")).unwrap().count(), 0);
        assert!(source.exists());
        assert!(matches!(Store::open(&history), Err(HistoryError::Busy)));
        assert_eq!(store.clear().unwrap().deleted_entries, 0);
        drop(store);
        let store = Store::open(&history).unwrap();
        let later = store
            .insert(entry("later", HistoryStatus::Success), None)
            .unwrap();
        assert!(later.id > last_id);
        assert_eq!(store.get(later.id).unwrap().text, "later");
    }

    #[test]
    fn history_clear_busy_and_database_failure_leave_entries_intact() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let saved = store
            .insert(entry("retry", HistoryStatus::Success), Some(&source))
            .unwrap();
        let other = store
            .insert(entry("other", HistoryStatus::Success), None)
            .unwrap();
        let lease = store.begin_retry(saved.id).unwrap();
        assert_eq!(store.clear(), Err(HistoryError::Busy));
        assert_eq!(store.get(other.id).unwrap().text, "other");
        assert!(lease.audio_path.exists());
        drop(lease);
        store
            .lock()
            .unwrap()
            .connection
            .pragma_update(None, "query_only", true)
            .unwrap();
        assert_eq!(store.clear(), Err(HistoryError::Storage));
        store
            .lock()
            .unwrap()
            .connection
            .pragma_update(None, "query_only", false)
            .unwrap();
        assert_eq!(store.list(&query("", None, 100)).unwrap().entries.len(), 2);
        assert!(store.audio_path(saved.id).exists());
        assert_eq!(store.clear().unwrap().deleted_entries, 2);
    }

    #[test]
    fn history_clear_reports_audio_failure_and_reopen_finishes_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let history = directory.path().join("history");
        let store = Store::open(&history).unwrap();
        let saved = store
            .insert(entry("private", HistoryStatus::Success), Some(&source))
            .unwrap();
        let audio_directory = history.join("audio");
        fs::set_permissions(&audio_directory, fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(
            store.clear().unwrap(),
            ClearHistoryResult {
                deleted_entries: 1,
                cleanup_pending: true,
            }
        );
        assert!(matches!(store.get(saved.id), Err(HistoryError::Missing)));
        assert!(store.audio_path(saved.id).exists());
        assert!(store.clear().unwrap().cleanup_pending);
        fs::set_permissions(&audio_directory, fs::Permissions::from_mode(0o700)).unwrap();
        drop(store);
        let store = Store::open(&history).unwrap();
        assert!(!store.audio_path(saved.id).exists());
        assert!(!store.clear().unwrap().cleanup_pending);
    }

    #[test]
    fn history_clear_reports_sweep_failure_after_deleting_text() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("history")).unwrap();
        store
            .insert(entry("private", HistoryStatus::Success), None)
            .unwrap();
        let audio_directory = store.directory.join("audio");
        let moved = store.directory.join("moved-audio");
        fs::rename(&audio_directory, &moved).unwrap();
        assert_eq!(
            store.clear().unwrap(),
            ClearHistoryResult {
                deleted_entries: 1,
                cleanup_pending: true,
            }
        );
        fs::rename(&moved, &audio_directory).unwrap();
        store.maintenance().unwrap();
        assert!(
            store
                .list(&query("", None, 100))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn history_ownership_outlives_the_store_handle_until_retry_releases() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let history = directory.path().join("history");
        let store = Store::open(&history).unwrap();
        let failed = store
            .insert(
                entry("partial", HistoryStatus::TranscriptionFailed),
                Some(&source),
            )
            .unwrap();
        let lease = store.begin_retry(failed.id).unwrap();
        let audio_path = lease.audio_path.clone();
        assert!(matches!(Store::open(&history), Err(HistoryError::Busy)));
        assert!(audio_path.exists());
        drop(store);
        assert!(matches!(Store::open(&history), Err(HistoryError::Busy)));
        drop(lease);
        let reopened = Store::open(&history).unwrap();
        assert_eq!(reopened.get(failed.id).unwrap().text, "partial");
        assert!(audio_path.exists());
        assert!(!reopened.get(failed.id).unwrap().retrying);
    }

    #[test]
    fn retry_pins_original_audio_and_recovers_the_same_entry() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let failed = store
            .insert(
                entry("partial", HistoryStatus::TranscriptionFailed),
                Some(&source),
            )
            .unwrap();
        assert!(source.exists());
        assert!(matches!(failed.audio, AudioAvailability::Available { .. }));
        let lease = store.begin_retry(failed.id).unwrap();
        assert!(matches!(
            store.begin_retry(failed.id),
            Err(HistoryError::Busy)
        ));
        assert!(matches!(store.delete(failed.id), Err(HistoryError::Busy)));
        store
            .attempt_failed(failed.id, HistoryError::Cancelled)
            .unwrap();
        assert_eq!(
            store.get(failed.id).unwrap().error,
            Some(HistoryError::Cancelled.into())
        );
        assert_eq!(store.get(failed.id).unwrap().text, "partial");
        let recovered = store
            .recover(failed.id, "whole transcript", "whisper", "en", false)
            .unwrap();
        assert_eq!(recovered.id, failed.id);
        assert_eq!(recovered.created_at_ms, failed.created_at_ms);
        assert_eq!(recovered.status, HistoryStatus::Recovered);
        assert!(recovered.error.is_none());
        assert_eq!(recovered.audio, AudioAvailability::Unavailable);
        assert!(!lease.audio_path.exists());
        drop(lease);
        assert!(!store.get(failed.id).unwrap().retrying);
        assert!(matches!(
            store.begin_retry(failed.id),
            Err(HistoryError::AudioUnavailable)
        ));
        assert!(source.exists());
    }

    #[test]
    fn regeneration_retains_audio_and_original_expiry_until_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let history = directory.path().join("history");
        let store = Store::open(&history).unwrap();
        let original = store
            .insert(entry("first", HistoryStatus::Success), Some(&source))
            .unwrap();
        for (text, engine, language) in [("second", "whisper", "fr"), ("third", "moonshine", "en")]
        {
            let lease = store.begin_retry(original.id).unwrap();
            let recovered = store
                .recover(original.id, text, engine, language, true)
                .unwrap();
            assert_eq!(recovered.audio, original.audio);
            assert_eq!(recovered.text, text);
            assert_eq!(recovered.engine, engine);
            assert_eq!(recovered.language, language);
            assert_eq!(recovered.created_at_ms, original.created_at_ms);
            drop(lease);
        }
        drop(store);
        let store = Store::open(&history).unwrap();
        let lease = store.begin_retry(original.id).unwrap();
        assert_eq!(lease.entry.text, "third");
        let recovered = store
            .recover(original.id, "last", "whisper", "en", false)
            .unwrap();
        assert_eq!(recovered.audio, AudioAvailability::Unavailable);
        drop(lease);
        assert!(matches!(
            store.begin_retry(original.id),
            Err(HistoryError::AudioUnavailable)
        ));
    }

    #[test]
    fn expiration_keeps_text_and_defers_pinned_audio_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let failed = store
            .insert(entry("partial", HistoryStatus::Success), Some(&source))
            .unwrap();
        let lease = store.begin_retry(failed.id).unwrap();
        store
            .lock()
            .unwrap()
            .connection
            .execute(
                "UPDATE entries SET expires=1 WHERE id=?1",
                [sql_integer(failed.id).unwrap()],
            )
            .unwrap();
        store.maintenance().unwrap();
        assert!(lease.audio_path.exists());
        assert_eq!(
            store.get(failed.id).unwrap().audio,
            AudioAvailability::Expired
        );
        let path = lease.audio_path.clone();
        drop(lease);
        assert!(!path.exists());
        assert_eq!(store.get(failed.id).unwrap().text, "partial");
        assert!(matches!(
            store.begin_retry(failed.id),
            Err(HistoryError::AudioUnavailable)
        ));
    }

    #[test]
    fn deletion_removes_the_entry_even_when_audio_cleanup_fails() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let history = directory.path().join("history");
        let store = Store::open(&history).unwrap();
        let failed = store
            .insert(
                entry("partial", HistoryStatus::TranscriptionFailed),
                Some(&source),
            )
            .unwrap();
        let later = store
            .insert(entry("later text", HistoryStatus::Success), None)
            .unwrap();
        let path = store.audio_path(failed.id);
        let audio_directory = path.parent().unwrap().to_path_buf();
        // Denying the directory write access is the one way to fail an unlink the
        // store is otherwise entitled to make.
        fs::set_permissions(&audio_directory, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(store.delete(failed.id).unwrap());
        assert!(matches!(store.get(failed.id), Err(HistoryError::Missing)));
        assert!(store.list(&query("", None, 100)).unwrap().warning.is_some());
        assert!(path.exists());
        fs::set_permissions(&audio_directory, fs::Permissions::from_mode(0o700)).unwrap();
        store.maintenance().unwrap();
        assert!(!path.exists());
        assert_eq!(store.get(later.id).unwrap().text, "later text");
    }

    #[test]
    fn a_drifted_audio_mode_does_not_block_removal() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let failed = store
            .insert(
                entry("partial", HistoryStatus::TranscriptionFailed),
                Some(&source),
            )
            .unwrap();
        let path = store.audio_path(failed.id);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.delete(failed.id).unwrap());
        assert!(matches!(store.get(failed.id), Err(HistoryError::Missing)));
        assert!(!path.exists());
    }

    #[test]
    fn an_interrupted_deletion_finishes_on_the_next_sweep() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("history")).unwrap();
        let pending = store
            .insert(entry("pending", HistoryStatus::Success), None)
            .unwrap();
        {
            let mut state = store.lock().unwrap();
            store.mark_delete(&mut state, pending.id).unwrap();
        }
        assert!(matches!(store.get(pending.id), Err(HistoryError::Missing)));
    }

    #[test]
    fn recovery_commit_survives_audio_cleanup_failure() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let failed = store
            .insert(
                entry("partial", HistoryStatus::TranscriptionFailed),
                Some(&source),
            )
            .unwrap();
        let lease = store.begin_retry(failed.id).unwrap();
        let path = lease.audio_path.clone();
        let audio_directory = path.parent().unwrap().to_path_buf();
        fs::set_permissions(&audio_directory, fs::Permissions::from_mode(0o500)).unwrap();
        let recovered = store
            .recover(failed.id, "recovered text", "whisper", "en", false)
            .unwrap();
        assert_eq!(recovered.status, HistoryStatus::Recovered);
        assert!(recovered.error.is_none());
        assert!(recovered.warning.is_some());
        assert_eq!(recovered.audio, AudioAvailability::Unavailable);
        drop(lease);
        assert_eq!(store.get(failed.id).unwrap().text, "recovered text");
        fs::set_permissions(&audio_directory, fs::Permissions::from_mode(0o700)).unwrap();
        store.maintenance().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn missing_corrupt_and_oversized_audio_never_become_retryable() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("history")).unwrap();
        let missing = directory.path().join("missing.wav");
        let corrupt = directory.path().join("corrupt.wav");
        fs::write(&corrupt, "not wav").unwrap();
        fs::set_permissions(&corrupt, fs::Permissions::from_mode(0o600)).unwrap();
        let oversized = directory.path().join("large.wav");
        let large = File::create(&oversized).unwrap();
        large
            .set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        large.set_len(MAX_AUDIO_BYTES + 1).unwrap();
        for path in [missing, corrupt, oversized] {
            let saved = store
                .insert(
                    entry("partial", HistoryStatus::TranscriptionFailed),
                    Some(&path),
                )
                .unwrap();
            assert_eq!(saved.audio, AudioAvailability::Unavailable);
            assert!(saved.warning.is_some());
            assert!(matches!(
                store.begin_retry(saved.id),
                Err(HistoryError::AudioUnavailable)
            ));
        }
        let source = audio(directory.path());
        let failed = store
            .insert(
                entry("partial", HistoryStatus::TranscriptionFailed),
                Some(&source),
            )
            .unwrap();
        fs::rename(
            store.audio_path(failed.id),
            directory.path().join("removed.wav"),
        )
        .unwrap();
        assert_eq!(
            store.get(failed.id).unwrap().audio,
            AudioAvailability::Unavailable
        );
    }

    #[test]
    fn invalid_retained_audio_stays_unavailable_until_cleanup_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let corrupt = store
            .insert(
                entry("partial", HistoryStatus::TranscriptionFailed),
                Some(&source),
            )
            .unwrap();
        fs::write(store.audio_path(corrupt.id), "corrupted after retention").unwrap();
        assert!(matches!(
            store.begin_retry(corrupt.id),
            Err(HistoryError::AudioUnavailable)
        ));
        assert_eq!(
            store.get(corrupt.id).unwrap().audio,
            AudioAvailability::Unavailable
        );
        assert!(!store.audio_path(corrupt.id).exists());

        let insecure = store
            .insert(
                entry("partial", HistoryStatus::TranscriptionFailed),
                Some(&source),
            )
            .unwrap();
        let path = store.audio_path(insecure.id);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            store.get(insecure.id).unwrap().audio,
            AudioAvailability::Unavailable
        );
        assert!(!path.exists());
        assert!(matches!(
            store.begin_retry(insecure.id),
            Err(HistoryError::AudioUnavailable)
        ));
        assert_eq!(
            store.get(insecure.id).unwrap().audio,
            AudioAvailability::Unavailable
        );
    }

    #[test]
    fn search_is_unicode_literal_and_cursor_order_survives_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let first = store
            .insert(entry("CAFÉ [x]", HistoryStatus::Success), None)
            .unwrap();
        let middle = store
            .insert(entry("unrelated", HistoryStatus::Success), None)
            .unwrap();
        let last = store
            .insert(entry("café [x]", HistoryStatus::Success), None)
            .unwrap();
        let page = store.list(&query("café [x]", None, 1)).unwrap();
        assert_eq!(page.entries[0].id, last.id);
        assert_eq!(page.next_before_id, Some(last.id));
        store.delete(middle.id).unwrap();
        let next = store
            .list(&query("café [x]", page.next_before_id, 1))
            .unwrap();
        assert_eq!(next.entries[0].id, first.id);
        assert!(next.next_before_id.is_none());
        let application_matches = store.list(&query("EDITOR", None, 100)).unwrap();
        assert_eq!(
            application_matches
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [last.id, first.id]
        );
        assert!(
            store
                .list(&query("%", None, 100))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(matches!(
            store.list(&query(&"x".repeat(257), None, 100)),
            Err(HistoryError::InvalidRequest)
        ));
        assert!(matches!(store.get(0), Err(HistoryError::InvalidRequest)));
    }

    #[test]
    fn out_of_range_requests_and_negative_database_metadata_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        assert!(matches!(
            store.get(u64::MAX),
            Err(HistoryError::InvalidRequest)
        ));
        let mut invalid = entry("text", HistoryStatus::Success);
        invalid.duration_ms = Some(u64::MAX);
        assert!(matches!(
            store.insert(invalid, None),
            Err(HistoryError::InvalidRequest)
        ));
        let saved = store
            .insert(entry("text", HistoryStatus::Success), None)
            .unwrap();
        store
            .lock()
            .unwrap()
            .connection
            .execute(
                "UPDATE entries SET duration=-1 WHERE id=?1",
                [sql_integer(saved.id).unwrap()],
            )
            .unwrap();
        assert!(matches!(store.get(saved.id), Err(HistoryError::Missing)));
    }

    #[test]
    fn entry_total_text_and_age_limits_preserve_newer_content() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let first = store
            .insert(entry("keep until full", HistoryStatus::Success), None)
            .unwrap();
        assert!(matches!(
            store.insert(
                entry(&"x".repeat(MAX_ENTRY_BYTES + 1), HistoryStatus::Success),
                None
            ),
            Err(HistoryError::TooLarge)
        ));
        assert!(store.get(first.id).is_ok());
        for _ in 0..(MAX_TEXT_BYTES / MAX_ENTRY_BYTES) {
            store
                .insert(
                    entry(&"x".repeat(MAX_ENTRY_BYTES), HistoryStatus::Success),
                    None,
                )
                .unwrap();
        }
        assert!(matches!(store.get(first.id), Err(HistoryError::Missing)));
        let state = store.lock().unwrap();
        assert_eq!(
            all(&state)
                .unwrap()
                .0
                .iter()
                .map(|entry| entry.text.len())
                .sum::<usize>(),
            MAX_TEXT_BYTES
        );
        state
            .connection
            .execute("UPDATE entries SET created=1", [])
            .unwrap();
        drop(state);
        assert!(
            store
                .list(&query("", None, 100))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn entry_count_includes_empty_failed_transcripts() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let first = store
            .insert(entry("", HistoryStatus::TranscriptionFailed), None)
            .unwrap();
        // Seed terminal records directly to avoid a thousand fsyncs in this unit test.
        let state = store.lock().unwrap();
        let transaction = state.connection.unchecked_transaction().unwrap();
        for _ in 1..MAX_ENTRIES {
            transaction.execute("INSERT INTO entries(created,source,text,complete,status,language,own_voice,engine) VALUES(?1,0,'',0,1,'en',1,'test')", [sql_integer(now_ms()).unwrap()]).unwrap();
        }
        transaction.commit().unwrap();
        drop(state);
        store
            .insert(entry("", HistoryStatus::TranscriptionFailed), None)
            .unwrap();
        assert!(matches!(store.get(first.id), Err(HistoryError::Missing)));
        assert_eq!(all(&store.lock().unwrap()).unwrap().0.len(), MAX_ENTRIES);
    }

    #[test]
    fn audio_budget_evicts_oldest_recording_without_losing_its_text() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(
                store
                    .insert(entry("partial", HistoryStatus::Success), Some(&source))
                    .unwrap()
                    .id,
            );
        }
        let lease = store.begin_retry(ids[0]).unwrap();
        for id in &ids {
            let file = open_private(&store.audio_directory, &audio_name(*id), false).unwrap();
            file.set_len(40 * 1024 * 1024).unwrap();
        }
        store.maintenance().unwrap();
        assert!(!store.audio_path(ids[1]).exists());
        assert_eq!(store.get(ids[1]).unwrap().text, "partial");
        assert_eq!(
            store.get(ids[1]).unwrap().audio,
            AudioAvailability::Unavailable
        );
        assert!(store.audio_path(ids[0]).exists());
        assert!(ids[2..].iter().all(|id| store.audio_path(*id).exists()));
        drop(lease);
    }

    #[test]
    fn private_file_checks_reject_symlinks_sidecars_and_insecure_files() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        fs::write(&target, "untouched").unwrap();
        let history = directory.path().join("history");
        private_fs::prepare_directory(&history).unwrap();
        symlink(&target, history.join("history.db")).unwrap();
        assert!(matches!(Store::open(&history), Err(HistoryError::Storage)));
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
        let other = directory.path().join("other");
        private_fs::prepare_directory(&other).unwrap();
        symlink(&target, other.join("history.db-journal")).unwrap();
        assert!(matches!(Store::open(&other), Err(HistoryError::Storage)));
        let third = directory.path().join("third");
        let store = Store::open(&third).unwrap();
        drop(store);
        fs::set_permissions(third.join("history.db"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(Store::open(&third), Err(HistoryError::Storage)));
    }

    #[test]
    fn startup_removes_owned_orphans_without_touching_other_files() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let orphan = store.directory.join("audio/900.wav");
        let other = store.directory.join("audio/user-file");
        fs::write(&orphan, "orphan").unwrap();
        fs::set_permissions(&orphan, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&other, "untouched").unwrap();
        drop(store);
        let _store = Store::open(directory.path()).unwrap();
        assert!(!orphan.exists());
        assert!(other.exists());
    }

    #[test]
    fn rollback_journal_is_private_and_database_growth_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let state = store.lock().unwrap();
        let page_size: i64 = state
            .connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        let max_pages: i64 = state
            .connection
            .pragma_query_value(None, "max_page_count", |row| row.get(0))
            .unwrap();
        assert!(page_size * max_pages <= sql_integer(MAX_DATABASE_BYTES).unwrap());
        let transaction = state.connection.unchecked_transaction().unwrap();
        transaction.execute("INSERT INTO entries(created,source,text,complete,status,language,own_voice,engine) VALUES(?1,0,'private',1,0,'en',0,'test')", [sql_integer(now_ms()).unwrap()]).unwrap();
        let journal = directory.path().join("history.db-journal");
        assert_eq!(fs::metadata(&journal).unwrap().mode() & 0o777, 0o600);
        assert!(fs::metadata(&journal).unwrap().len() <= MAX_DATABASE_BYTES + 1024 * 1024);
        transaction.commit().unwrap();
        assert!(!journal.exists());
    }

    #[test]
    fn locked_and_full_databases_return_safe_typed_errors() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let contender = Connection::open(directory.path().join("history.db")).unwrap();
        contender.execute_batch("BEGIN EXCLUSIVE").unwrap();
        assert!(matches!(
            store.insert(entry("private", HistoryStatus::Success), None),
            Err(HistoryError::Busy)
        ));
        contender.execute_batch("ROLLBACK").unwrap();
        let state = store.lock().unwrap();
        let pages: i64 = state
            .connection
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap();
        state
            .connection
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        drop(state);
        assert!(matches!(
            store.insert(
                entry(&"x".repeat(MAX_ENTRY_BYTES), HistoryStatus::Success),
                None
            ),
            Err(HistoryError::TooLarge)
        ));
        assert!(
            store
                .list(&query("", None, 100))
                .unwrap()
                .entries
                .is_empty()
        );
    }
}
