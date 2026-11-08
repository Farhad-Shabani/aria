use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::private_fs;
use aria_platform::sqlite::{self, Database, DriftedMode, Ownership, Policy};
use rusqlite::{OptionalExtension, params};
use rustix::fs::{Mode, OFlags};

use super::{
    AudioAvailability, ClearHistoryResult, DictationContent, HistoryContent, HistoryEntry,
    HistoryError, HistoryPage, HistoryQuery, HistorySection, HistorySource, HistoryStatus,
    PromptResult, Title, TranscriptRead,
};
use aria_kit::audio::RecordingReader;
use aria_platform::delivery::Application;
use aria_primitives::error_chain;
use aria_settings::MAX_PROMPT_NAME_BYTES;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};

const MAX_ERROR_MESSAGE_BYTES: usize = 256;
const MAX_STORED_ERROR_BYTES: usize = 2 * 1024;
pub const MAX_ENTRY_BYTES: usize = 128 * 1024;
const MAX_AUDIO_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DATABASE_BYTES: u64 = 32 * 1024 * 1024;
const DATABASE_POLICY: Policy = Policy {
    max_bytes: MAX_DATABASE_BYTES,
    busy_timeout: Duration::from_millis(250),
    ownership: Ownership::Exclusive {
        lock: "history.lock",
    },
    drifted_mode: DriftedMode::Refuse,
};
const TEXT_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const AUDIO_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const CLEANUP_WARNING: &str = "Some history files could not be removed. Cleanup will be retried.";
const AUDIO_WARNING: &str = "The recording could not be saved. Audio recovery is unavailable.";
const UNREADABLE_WARNING: &str = "Some unreadable history entries were removed.";
const COLUMNS: &str = "id,created,source,app_name,bundle_id,duration,text,complete,status,error,expires,audio_present,deleting,language,own_voice,project,engine,section,title,transcript,reply";
/// Columns added after the table first shipped, in the order `COLUMNS` reads
/// them. The version stays 1: an older daemon names its own columns, so it
/// still reads the table, and its inserts take these defaults.
const ADDED_COLUMNS: &[(&str, &str)] = &[
    (
        "section",
        "section INTEGER NOT NULL DEFAULT 0 CHECK(section IN (0,1,2))",
    ),
    (
        "title",
        "title TEXT CHECK(title IS NULL OR length(CAST(title AS BLOB)) BETWEEN 1 AND 256)",
    ),
    (
        "transcript",
        "transcript BLOB CHECK(transcript IS NULL OR length(transcript)<=4096)
         CHECK((section=1)=(transcript IS NOT NULL))",
    ),
    (
        "reply",
        "reply TEXT CHECK(reply IS NULL OR length(CAST(reply AS BLOB))<=131072)
         CHECK((section=2)=(reply IS NOT NULL))",
    ),
];
/// Prompt results live beside the entries, so an older daemon, which never
/// names this table, reads and writes `entries` as before. An older daemon
/// leaves foreign keys off, so its deletes skip the cascade and the orphan
/// sweep takes what they leave.
const RESULTS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS results (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        entry INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
        created INTEGER NOT NULL CHECK(created>=0),
        prompt_name TEXT NOT NULL CHECK(length(CAST(prompt_name AS BLOB)) BETWEEN 1 AND 128),
        text TEXT NOT NULL CHECK(length(CAST(text AS BLOB)) BETWEEN 1 AND 131072));
     CREATE INDEX IF NOT EXISTS results_by_entry ON results(entry);";
/// How many results one entry keeps. A newer one drops the oldest.
const RESULTS_PER_ENTRY: usize = 20;
/// How much text all results keep together, across entries.
const RESULTS_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// The start of a transcript that its entry keeps and search covers.
const EXCERPT_BYTES: usize = 1024;
/// How much of a transcript Get reads into the entry.
const MAX_TRANSCRIPT_READ: u64 = 1024 * 1024;

/// How much one section keeps before it evicts its own oldest entries.
struct Budget {
    entries: usize,
    text_bytes: usize,
}

fn budget(section: HistorySection) -> Budget {
    match section {
        HistorySection::Dictation => Budget {
            entries: 1_000,
            text_bytes: 8 * 1024 * 1024,
        },
        // A session's text stays in its transcript file.
        HistorySection::SystemTranscription => Budget {
            entries: 500,
            text_bytes: 512 * 1024,
        },
        HistorySection::Chat => Budget {
            entries: 500,
            text_bytes: 8 * 1024 * 1024,
        },
    }
}

pub struct Store {
    directory: PathBuf,
    audio_directory: File,
    // The one lock also bounds audio copies. Copies need their own admission
    // if reads stall.
    state: Mutex<State>,
}

struct State {
    connection: Database,
    pinned: HashSet<u64>,
    warning: Option<String>,
}

pub struct RetryLease {
    pub entry: HistoryEntry,
    /// Read from the descriptor whose size was checked.
    pub recording: RecordingReader,
    store: Arc<Store>,
}

impl Drop for RetryLease {
    fn drop(&mut self) {
        self.store.lock().pinned.remove(&self.entry.id);
        if let Err(error) = self.store.maintenance() {
            tracing::warn!(error = %error_chain(&error), "dictation history cleanup failed after retry");
        }
    }
}

impl Store {
    pub fn open(directory: &Path) -> Result<Arc<Self>, HistoryError> {
        let connection = Database::open(
            &directory.join("history.db"),
            &DATABASE_POLICY,
            &CancelToken::never(),
        )?;
        let directory = directory.canonicalize().map_err(HistoryError::storage)?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 1 {
            return Err(HistoryError::Unavailable);
        }
        connection.execute_batch(
            "PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             PRAGMA journal_size_limit=0;
             PRAGMA secure_delete=ON;",
        )?;
        connection.pragma_update(None, "foreign_keys", true)?;
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
        add_missing_columns(&connection)?;
        connection.execute_batch(RESULTS_SCHEMA)?;
        private_fs::prepare_directory(&directory.join("audio")).map_err(HistoryError::storage)?;
        let audio_directory =
            sqlite::open_directory(&directory.join("audio")).map_err(HistoryError::storage)?;
        let store = Arc::new(Self {
            directory,
            audio_directory,
            state: Mutex::new(State {
                connection,
                pinned: HashSet::new(),
                warning: None,
            }),
        });
        store.maintenance()?;
        Ok(store)
    }

    pub fn insert(
        &self,
        mut entry: HistoryEntry,
        audio: Option<&Path>,
    ) -> Result<HistoryEntry, HistoryError> {
        validate_entry(&entry)?;
        let mut state = self.lock();
        self.maintain(&mut state)?;
        let mut copied = match audio {
            None => None,
            Some(path) => match state.warning.is_none().then(|| self.copy_audio(path)) {
                Some(Ok(copy)) => Some(copy),
                Some(Err(error)) => {
                    tracing::warn!(error = %error_chain(&error), "dictation history audio could not be saved");
                    state.warning = Some(AUDIO_WARNING.to_owned());
                    entry.warning = Some(AUDIO_WARNING.to_owned());
                    None
                }
                None => {
                    state.warning = Some(AUDIO_WARNING.to_owned());
                    entry.warning = Some(AUDIO_WARNING.to_owned());
                    None
                }
            },
        };
        // Free the space before committing: a full database cannot evict anything.
        if let Err(error) = self.make_room(
            &mut state,
            entry.content.section(),
            stored_bytes(&entry),
            copied.as_ref().map_or(0, |(_, size)| *size),
            true,
        ) {
            if copied.is_none() {
                return Err(error);
            }
            copied = None;
            entry.warning = Some(AUDIO_WARNING.to_owned());
            self.make_room(
                &mut state,
                entry.content.section(),
                stored_bytes(&entry),
                0,
                true,
            )?;
        }
        let transaction = state.connection.unchecked_transaction()?;
        let app_name = entry.application.as_ref().map(|app| app.name.as_str());
        let bundle_id = entry
            .application
            .as_ref()
            .and_then(|app| app.bundle_id.as_deref());
        let dictation = entry.dictation();
        transaction.execute(
            "INSERT INTO entries(created,source,app_name,bundle_id,duration,text,complete,status,error,language,own_voice,project,engine,section,title,transcript,reply)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![sql_integer(entry.created_at_ms)?, source_number(entry.source), app_name, bundle_id,
                entry.duration_ms.map(sql_integer).transpose()?, entry.text, entry.complete, status_number(entry.status),
                entry.error.as_ref().map(encode_error).transpose()?, dictation.map_or("", |d| d.language.as_str()),
                dictation.is_some_and(|d| d.own_voice_only),
                entry.project.as_ref().map(|path| path.as_os_str().as_bytes()),
                dictation.map_or("", |d| d.engine.as_str()),
                section_number(entry.content.section()), entry.title.as_ref().map(Title::as_str),
                transcript_path(&entry.content).map(|path| path.as_os_str().as_bytes()),
                reply(&entry.content)],
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

    pub fn list(&self, query: &HistoryQuery) -> Result<HistoryPage, HistoryError> {
        if query.query.len() > HistoryQuery::MAX_QUERY_BYTES
            || query.limit == 0
            || query.limit > 100
        {
            return Err(HistoryError::InvalidRequest);
        }
        if let Some(id) = query.before_id {
            validate_id(id)?;
        }
        let mut state = self.lock();
        self.maintain(&mut state)?;
        let needle = query.query.to_lowercase();
        let (entries, _) = all(&state)?;
        let mut entries = entries
            .into_iter()
            .filter(|entry| {
                query
                    .section
                    .is_none_or(|section| entry.content.section() == section)
            })
            .filter(|entry| query.before_id.is_none_or(|id| entry.id < id))
            .filter(|entry| {
                entry.text.to_lowercase().contains(&needle)
                    || reply(&entry.content)
                        .is_some_and(|reply| reply.to_lowercase().contains(&needle))
                    || entry
                        .title
                        .as_ref()
                        .is_some_and(|title| title.as_str().to_lowercase().contains(&needle))
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

    /// Saves a finished system transcription that wrote `transcript`. The file stays the session's
    /// text, and the entry keeps its excerpt. An empty transcript saves nothing.
    pub fn save_transcript(
        &self,
        transcript: &Path,
        created_at_ms: u64,
        duration_ms: u64,
        failure: Option<RpcFailure>,
    ) -> Result<Option<HistoryEntry>, HistoryError> {
        let mut excerpt = Vec::with_capacity(EXCERPT_BYTES);
        open_transcript(transcript)?
            .take(EXCERPT_BYTES as u64)
            .read_to_end(&mut excerpt)
            .map_err(HistoryError::storage)?;
        let excerpt = leading_text(excerpt).ok_or(HistoryError::InvalidRequest)?;
        if excerpt.trim().is_empty() {
            return Ok(None);
        }
        let entry = HistoryEntry {
            id: 0,
            title: None,
            content: HistoryContent::SystemTranscription {
                transcript: transcript.to_path_buf(),
                read: None,
            },
            created_at_ms,
            duration_ms: Some(duration_ms),
            source: HistorySource::Local,
            application: None,
            project: None,
            text: excerpt,
            complete: failure.is_none(),
            result_count: 0,
            status: match failure {
                None => HistoryStatus::Success,
                Some(_) => HistoryStatus::TranscriptionFailed,
            },
            error: failure,
            warning: None,
            deletion_pending: false,
        };
        self.insert(entry, None).map(Some)
    }

    /// Saves a chat that replied: the question as asked, the folder it worked
    /// in, and the reply. `failure` is why the reply couldn't be typed.
    pub fn save_chat(
        &self,
        question: &str,
        reply: &str,
        workdir: &Path,
        created_at_ms: u64,
        duration_ms: u64,
        failure: Option<RpcFailure>,
    ) -> Result<HistoryEntry, HistoryError> {
        let entry = HistoryEntry {
            id: 0,
            title: None,
            content: HistoryContent::Chat {
                reply: reply.to_owned(),
            },
            created_at_ms,
            duration_ms: Some(duration_ms),
            source: HistorySource::Local,
            application: None,
            project: Some(workdir.to_path_buf()),
            text: question.to_owned(),
            complete: true,
            result_count: 0,
            status: match failure {
                None => HistoryStatus::Success,
                Some(_) => HistoryStatus::DeliveryFailed,
            },
            error: failure,
            warning: None,
            deletion_pending: false,
        };
        self.insert(entry, None)
    }

    /// Saves a prompt's reply under `entry`. An entry keeps its newest
    /// results, and all results share one text budget, so the oldest go first.
    pub fn save_result(
        &self,
        entry: u64,
        prompt_name: &str,
        text: &str,
        created_at_ms: u64,
    ) -> Result<(), HistoryError> {
        validate_id(entry)?;
        if prompt_name.is_empty() || prompt_name.len() > MAX_PROMPT_NAME_BYTES || text.is_empty() {
            return Err(HistoryError::InvalidRequest);
        }
        if text.len() > MAX_ENTRY_BYTES {
            return Err(HistoryError::TooLarge);
        }
        let mut state = self.lock();
        self.maintain(&mut state)?;
        if load(&state, entry)?.deletion_pending {
            return Err(HistoryError::Missing);
        }
        let transaction = state.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO results(entry,created,prompt_name,text) VALUES(?1,?2,?3,?4)",
            params![
                sql_integer(entry)?,
                sql_integer(created_at_ms)?,
                prompt_name,
                text
            ],
        )?;
        transaction.execute(
            "DELETE FROM results WHERE entry=?1 AND id NOT IN
             (SELECT id FROM results WHERE entry=?1 ORDER BY id DESC LIMIT ?2)",
            params![sql_integer(entry)?, RESULTS_PER_ENTRY as i64],
        )?;
        transaction.execute(
            "DELETE FROM results WHERE id IN
             (SELECT id FROM
              (SELECT id, SUM(length(CAST(text AS BLOB))) OVER (ORDER BY id DESC) AS kept
               FROM results)
              WHERE kept>?1)",
            [RESULTS_TEXT_BYTES as i64],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// An entry's prompt results, newest first.
    pub fn results(&self, entry: u64) -> Result<Vec<PromptResult>, HistoryError> {
        validate_id(entry)?;
        let state = self.lock();
        load(&state, entry)?;
        let mut statement = state.connection.prepare(
            "SELECT id,created,prompt_name,text FROM results WHERE entry=?1 ORDER BY id DESC",
        )?;
        let results = statement
            .query_map([sql_integer(entry)?], |row| {
                Ok(PromptResult {
                    id: unsigned(row.get(0)?, 0)?,
                    created_at_ms: unsigned(row.get(1)?, 1)?,
                    prompt_name: row.get(2)?,
                    text: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(results)
    }

    /// A transcript entry comes back with its transcript read, up to `MAX_TRANSCRIPT_READ`, outside
    /// the lock.
    pub fn get(&self, id: u64) -> Result<HistoryEntry, HistoryError> {
        validate_id(id)?;
        let mut entry = {
            let mut state = self.lock();
            self.maintain(&mut state)?;
            load(&state, id)?
        };
        if let HistoryContent::SystemTranscription { transcript, read } = &mut entry.content {
            match read_transcript(transcript) {
                Some((text, whole)) => {
                    entry.text = text;
                    *read = Some(if whole {
                        TranscriptRead::Whole
                    } else {
                        TranscriptRead::Truncated
                    });
                }
                None => *read = Some(TranscriptRead::Unavailable),
            }
        }
        Ok(entry)
    }

    /// `None` removes the title. A regeneration never writes the title, so a
    /// rename may land while one runs.
    pub fn rename(&self, id: u64, title: Option<&Title>) -> Result<HistoryEntry, HistoryError> {
        validate_id(id)?;
        let mut state = self.lock();
        self.maintain(&mut state)?;
        if load(&state, id)?.deletion_pending {
            return Err(HistoryError::Missing);
        }
        state.connection.execute(
            "UPDATE entries SET title=?1 WHERE id=?2",
            params![title.map(Title::as_str), sql_integer(id)?],
        )?;
        load(&state, id)
    }

    pub fn delete(&self, id: u64) -> Result<bool, HistoryError> {
        validate_id(id)?;
        let mut state = self.lock();
        if state.pinned.contains(&id) {
            return Err(HistoryError::Busy);
        }
        let entry = match load(&state, id) {
            Ok(entry) => entry,
            Err(HistoryError::Missing) => return Ok(false),
            Err(error) => return Err(error),
        };
        // The file goes first: nothing sweeps the transcripts folder, so a
        // file whose row went first would never be reclaimed.
        if let Some(transcript) = transcript_path(&entry.content) {
            self.remove_transcript(&mut state, transcript)?;
        }
        self.mark_delete(&mut state, id)?;
        self.remove_entry(&mut state, id)?;
        Ok(true)
    }

    /// `None` clears every section.
    pub fn clear(
        &self,
        section: Option<HistorySection>,
    ) -> Result<ClearHistoryResult, HistoryError> {
        let mut state = self.lock();
        if !state.pinned.is_empty() {
            return Err(HistoryError::Busy);
        }
        state.warning = None;
        let (entries, unreadable) = all(&state)?;
        let mut doomed = Vec::new();
        for entry in entries {
            if section.is_some_and(|section| entry.content.section() != section) {
                continue;
            }
            // A transcript that can't be removed keeps its row, as Delete does.
            if let Some(transcript) = transcript_path(&entry.content)
                && self.remove_transcript(&mut state, transcript).is_err()
            {
                state.warning = Some(CLEANUP_WARNING.to_owned());
                continue;
            }
            doomed.push(entry.id);
        }
        if section.is_none() {
            doomed.extend(unreadable);
        }
        let transaction = state.connection.unchecked_transaction()?;
        let mut deleted_entries = 0;
        for id in doomed {
            deleted_entries +=
                transaction.execute("DELETE FROM entries WHERE id=?1", [sql_integer(id)?])? as u64;
        }
        transaction.commit()?;
        if self.remove_orphans(&mut state).is_err() {
            state.warning = Some(CLEANUP_WARNING.to_owned());
        }
        Ok(ClearHistoryResult {
            deleted_entries,
            cleanup_pending: state.warning.is_some(),
        })
    }

    pub fn begin_retry(self: &Arc<Self>, id: u64) -> Result<RetryLease, HistoryError> {
        validate_id(id)?;
        let mut state = self.lock();
        self.maintain(&mut state)?;
        let mut entry = load(&state, id)?;
        if state.pinned.contains(&id) {
            return Err(HistoryError::Busy);
        }
        if entry.deletion_pending || !matches!(entry.audio(), AudioAvailability::Available { .. }) {
            return Err(HistoryError::AudioUnavailable);
        }
        let audio_path = self.audio_path(id);
        let validated = (|| -> Result<RecordingReader, HistoryError> {
            let file = open_private(&self.audio_directory, &audio_name(id))?;
            check_size(&file, MAX_AUDIO_BYTES)?;
            RecordingReader::from_open_wav(file, &audio_path)
                .map_err(|_| HistoryError::AudioUnavailable)
        })();
        let Ok(recording) = validated else {
            self.invalidate_audio(&mut state, id)?;
            return Err(HistoryError::AudioUnavailable);
        };
        state.pinned.insert(id);
        if let Some(dictation) = entry.dictation_mut() {
            dictation.retrying = true;
        }
        Ok(RetryLease {
            entry,
            recording,
            store: Arc::clone(self),
        })
    }

    pub fn recover(
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
        let mut state = self.lock();
        if !state.pinned.contains(&id) {
            return Err(HistoryError::InvalidRequest);
        }
        let entry = load(&state, id)?;
        if entry.deletion_pending {
            return Err(HistoryError::InvalidRequest);
        }
        self.make_room_for_text(
            &mut state,
            entry.content.section(),
            text.len().saturating_sub(entry.text.len()),
        )?;
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

    pub fn attempt_failed(&self, id: u64, error: &HistoryError) -> Result<(), HistoryError> {
        validate_id(id)?;
        // The top line only: a cause chain can outgrow the stored cap, and it
        // reaches the log through the handler that returns this error.
        let error = encode_error(&RpcFailure::new(error.into(), error.to_string()))?;
        let state = self.lock();
        if !state.pinned.contains(&id) {
            return Err(HistoryError::InvalidRequest);
        }
        state.connection.execute(
            "UPDATE entries SET error=?1 WHERE id=?2",
            params![error, sql_integer(id)?],
        )?;
        Ok(())
    }

    pub fn maintenance(&self) -> Result<(), HistoryError> {
        let mut state = self.lock();
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
            // Expiry takes the row only: a transcript stays a file
            // the person keeps. A row whose file someone removed goes too.
            if entry.deletion_pending
                || now.saturating_sub(entry.created_at_ms) >= TEXT_TTL_MS
                || transcript_path(&entry.content).is_some_and(transcript_gone)
            {
                self.mark_delete(state, entry.id)?;
                if self.remove_entry(state, entry.id).is_err() {
                    state.warning = Some(CLEANUP_WARNING.to_owned());
                }
                continue;
            }
            let expired = match entry.audio() {
                AudioAvailability::Available { expires_at_ms } => expires_at_ms <= now,
                AudioAvailability::Expired => true,
                AudioAvailability::Unavailable => false,
            };
            if expired || entry.audio() == AudioAvailability::Unavailable {
                if self.remove_audio(state, entry.id).is_err() {
                    state.warning = Some(CLEANUP_WARNING.to_owned());
                }
            } else if matches!(entry.audio(), AudioAvailability::Available { .. })
                && open_private(&self.audio_directory, &audio_name(entry.id)).is_err()
            {
                self.invalidate_audio(state, entry.id)?;
            }
        }
        self.remove_orphans(state)?;
        for section in HistorySection::ALL {
            if self.make_room(state, section, 0, 0, false).is_err() {
                state.warning = Some(CLEANUP_WARNING.to_owned());
            }
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

    /// Leaves in place, with a warning, anything at the path that isn't a
    /// regular file of ours: the path is the only check the row gives.
    fn remove_transcript(&self, state: &mut State, transcript: &Path) -> Result<(), HistoryError> {
        let (Some(parent), Some(name)) = (transcript.parent(), transcript.file_name()) else {
            return Err(HistoryError::InvalidRequest);
        };
        let directory = match rustix::fs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => File::from(directory),
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => return Err(HistoryError::storage(error)),
        };
        let stat = match rustix::fs::statat(&directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => return Err(HistoryError::storage(error)),
        };
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
            || stat.st_uid != rustix::process::getuid().as_raw()
        {
            state.warning = Some(CLEANUP_WARNING.to_owned());
            return Ok(());
        }
        match rustix::fs::unlinkat(&directory, name, rustix::fs::AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(HistoryError::storage(error)),
        }
        directory.sync_all().map_err(HistoryError::storage)
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
            Err(error) => return Err(HistoryError::storage(error)),
        }
        self.audio_directory
            .sync_all()
            .map_err(HistoryError::storage)?;
        Ok(())
    }

    /// Removes orphaned audio, and the results an older daemon left behind when it deleted their
    /// entry.
    fn remove_orphans(&self, state: &mut State) -> Result<(), HistoryError> {
        state.connection.execute(
            "DELETE FROM results WHERE entry NOT IN (SELECT id FROM entries)",
            [],
        )?;
        let mut statement = state
            .connection
            .prepare("SELECT id FROM entries WHERE audio_present=1")?;
        let live = statement
            .query_map([], |row| unsigned(row.get(0)?, 0))?
            .map(|id| id.map(audio_name))
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        for item in fs::read_dir(self.directory.join("audio")).map_err(HistoryError::storage)? {
            let item = item.map_err(HistoryError::storage)?;
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

    /// Evicts within `section` only. Audio has one budget across sections.
    fn make_room(
        &self,
        state: &mut State,
        section: HistorySection,
        text_bytes: usize,
        audio_bytes: u64,
        reserve_entry: bool,
    ) -> Result<(), HistoryError> {
        self.make_room_for_text(state, section, text_bytes)?;
        let (mut entries, _) = all(state)?;
        entries.retain(|entry| entry.content.section() == section);
        let budget = budget(section).entries;
        // An insert reserves a slot even when its transcript is empty.
        let limit = if reserve_entry { budget - 1 } else { budget };
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
        let (entries, _) = all(state)?;
        let mut retained_audio = 0;
        for entry in &entries {
            match open_private(&self.audio_directory, &audio_name(entry.id)) {
                Ok(file) => {
                    retained_audio += file.metadata().map_err(HistoryError::storage)?.len();
                }
                Err(HistoryError::Missing) => {}
                Err(error) => return Err(error),
            }
        }
        for entry in entries.iter().rev() {
            if retained_audio.saturating_add(audio_bytes) <= MAX_AUDIO_BYTES {
                break;
            }
            if state.pinned.contains(&entry.id) {
                continue;
            }
            if let Ok(file) = open_private(&self.audio_directory, &audio_name(entry.id)) {
                let size = file.metadata().map_err(HistoryError::storage)?.len();
                self.remove_audio(state, entry.id)?;
                retained_audio = retained_audio.saturating_sub(size);
            }
        }
        if retained_audio.saturating_add(audio_bytes) > MAX_AUDIO_BYTES {
            return Err(HistoryError::Busy);
        }
        Ok(())
    }

    fn make_room_for_text(
        &self,
        state: &mut State,
        section: HistorySection,
        added: usize,
    ) -> Result<(), HistoryError> {
        let limit = budget(section).text_bytes;
        let (mut entries, _) = all(state)?;
        entries.retain(|entry| entry.content.section() == section);
        let mut bytes = entries.iter().map(stored_bytes).sum::<usize>();
        for entry in entries.iter().rev() {
            if bytes.saturating_add(added) <= limit {
                break;
            }
            if state.pinned.contains(&entry.id) {
                continue;
            }
            self.mark_delete(state, entry.id)?;
            self.remove_entry(state, entry.id)?;
            bytes -= stored_bytes(entry);
        }
        if bytes.saturating_add(added) > limit {
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
            .map_err(HistoryError::storage)?;
        let bytes = io::copy(&mut source.take(MAX_AUDIO_BYTES + 1), copy.as_file_mut())
            .map_err(HistoryError::storage)?;
        if bytes > MAX_AUDIO_BYTES || bytes != metadata.len() {
            return Err(HistoryError::AudioUnavailable);
        }
        copy.as_file().sync_all().map_err(HistoryError::storage)?;
        RecordingReader::from_wav_file(copy.path()).map_err(|_| HistoryError::AudioUnavailable)?;
        Ok((copy, bytes))
    }

    fn audio_path(&self, id: u64) -> PathBuf {
        self.directory.join("audio").join(audio_name(id))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
    if entry.text.len() > MAX_ENTRY_BYTES
        || reply(&entry.content).is_some_and(|reply| reply.len() > MAX_ENTRY_BYTES)
    {
        return Err(HistoryError::TooLarge);
    }
    let dictation = entry.dictation();
    if transcript_path(&entry.content)
        .is_some_and(|path| !path.is_absolute() || path.as_os_str().len() > 4096)
        || (entry.content.section() == HistorySection::SystemTranscription
            && entry.text.len() > EXCERPT_BYTES)
    {
        return Err(HistoryError::InvalidRequest);
    }
    if entry.created_at_ms > i64::MAX as u64
        || entry
            .duration_ms
            .is_some_and(|duration| duration > i64::MAX as u64)
        || dictation.is_some_and(|dictation| {
            dictation.engine.len() > 128
                || dictation.language.is_empty()
                || dictation.language.len() > 64
        })
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

fn open_private(directory: &File, name: &str) -> Result<File, HistoryError> {
    let descriptor = rustix::fs::openat(
        directory,
        name,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::NOENT {
            HistoryError::Missing
        } else {
            HistoryError::storage(error)
        }
    })?;
    let file = File::from(descriptor);
    let metadata = private_fs::verify_file(&file).map_err(HistoryError::storage)?;
    if metadata.nlink() != 1 {
        return Err(HistoryError::storage(Linked));
    }
    Ok(file)
}

#[derive(Debug, thiserror::Error)]
#[error("a history file has more than one link")]
struct Linked;

fn check_size(file: &File, limit: u64) -> Result<(), HistoryError> {
    if file.metadata().map_err(HistoryError::storage)?.len() > limit {
        return Err(HistoryError::TooLarge);
    }
    Ok(())
}

fn transcript_path(content: &HistoryContent) -> Option<&Path> {
    match content {
        HistoryContent::SystemTranscription { transcript, .. } => Some(transcript),
        HistoryContent::Dictation(_) | HistoryContent::Chat { .. } => None,
    }
}

fn reply(content: &HistoryContent) -> Option<&str> {
    match content {
        HistoryContent::Chat { reply } => Some(reply),
        HistoryContent::Dictation(_) | HistoryContent::SystemTranscription { .. } => None,
    }
}

/// What an entry counts against its section's text budget.
fn stored_bytes(entry: &HistoryEntry) -> usize {
    entry.text.len() + reply(&entry.content).map_or(0, str::len)
}

/// Gone only while its folder is there, so an unmounted volume doesn't
/// empty the section.
fn transcript_gone(transcript: &Path) -> bool {
    transcript
        .symlink_metadata()
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        && transcript.parent().is_some_and(Path::is_dir)
}

/// Read-only, never through a link, and only a regular file of ours. Its mode
/// is the person's to change.
fn open_transcript(transcript: &Path) -> Result<File, HistoryError> {
    let descriptor = rustix::fs::open(
        transcript,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| match error {
        rustix::io::Errno::NOENT => HistoryError::Missing,
        error => HistoryError::storage(error),
    })?;
    let file = File::from(descriptor);
    let metadata = file.metadata().map_err(HistoryError::storage)?;
    if !metadata.is_file() || metadata.uid() != rustix::process::getuid().as_raw() {
        return Err(HistoryError::InvalidRequest);
    }
    Ok(file)
}

/// The text up to `MAX_TRANSCRIPT_READ`, and whether that was all of it. `None` when the file can't
/// be read as text.
fn read_transcript(transcript: &Path) -> Option<(String, bool)> {
    let mut bytes = Vec::new();
    open_transcript(transcript)
        .ok()?
        .take(MAX_TRANSCRIPT_READ + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let whole = bytes.len() as u64 <= MAX_TRANSCRIPT_READ;
    bytes.truncate(MAX_TRANSCRIPT_READ as usize);
    if whole {
        return String::from_utf8(bytes).ok().map(|text| (text, true));
    }
    leading_text(bytes).map(|text| (text, false))
}

/// The bytes as text, dropping a character the cut split at the end.
fn leading_text(mut bytes: Vec<u8>) -> Option<String> {
    if let Err(error) = std::str::from_utf8(&bytes) {
        if error.error_len().is_some() {
            return None;
        }
        bytes.truncate(error.valid_up_to());
    }
    String::from_utf8(bytes).ok()
}

fn add_missing_columns(connection: &Database) -> Result<(), HistoryError> {
    let transaction = connection.unchecked_transaction()?;
    let present = transaction
        .prepare("SELECT name FROM pragma_table_info('entries')")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    for (name, definition) in ADDED_COLUMNS {
        if !present.contains(*name) {
            transaction.execute_batch(&format!("ALTER TABLE entries ADD COLUMN {definition}"))?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn section_number(section: HistorySection) -> u8 {
    match section {
        HistorySection::Dictation => 0,
        HistorySection::SystemTranscription => 1,
        HistorySection::Chat => 2,
    }
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

/// Keeps a frozen number, which outlives renames of `RpcErrorCode` variants.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredError {
    code: i32,
    message: String,
}

/// The numbers in existing rows. They never change. A new code takes the number
/// `errors.proto` gives it.
fn stored_code(code: RpcErrorCode) -> i32 {
    match code {
        RpcErrorCode::InvalidRequest => 1,
        RpcErrorCode::Busy => 2,
        RpcErrorCode::Internal => 3,
        RpcErrorCode::Cancelled => 4,
        RpcErrorCode::Unauthenticated => 5,
        RpcErrorCode::Unsupported => 6,
        RpcErrorCode::ShuttingDown => 7,
        RpcErrorCode::PayloadTooLarge => 8,
        RpcErrorCode::PermissionDenied => 9,
        RpcErrorCode::DurabilityUnconfirmed => 10,
        RpcErrorCode::SettingsInvalid => 11,
        RpcErrorCode::OverallTimeout => 100,
        RpcErrorCode::RecordingTimeout => 101,
        RpcErrorCode::DeliveryTimeout => 102,
        RpcErrorCode::CaptureFailed => 103,
        RpcErrorCode::NoSpeech => 104,
        RpcErrorCode::RecognitionFailed => 105,
        RpcErrorCode::DeliveryFailed => 106,
        RpcErrorCode::DeliveryTargetChanged => 108,
        RpcErrorCode::DeliveryInterrupted => 109,
        RpcErrorCode::ApplicationExcluded => 110,
        RpcErrorCode::SynthesisFailed => 200,
        RpcErrorCode::PlaybackFailed => 201,
        RpcErrorCode::SpeechTimeout => 202,
        RpcErrorCode::VoiceUnavailable => 203,
        RpcErrorCode::AssistantUnavailable => 300,
        RpcErrorCode::AssistantFailed => 301,
        RpcErrorCode::AssistantTimeout => 302,
        RpcErrorCode::AssistantSignedOut => 303,
        RpcErrorCode::PromptUnreadable => 304,
        RpcErrorCode::PromptMissing => 305,
        RpcErrorCode::PromptsFull => 306,
        RpcErrorCode::PromptWriteFailed => 307,
        RpcErrorCode::SpeakerModelMissing => 500,
        RpcErrorCode::SpeakerAlreadyEnrolled => 501,
        RpcErrorCode::SpeakerNotEnrolled => 502,
        RpcErrorCode::SpeakerInconsistentSamples => 503,
        RpcErrorCode::SpeakerProfileWriteFailed => 504,
        RpcErrorCode::SpeakerNoSpeech => 505,
        RpcErrorCode::SystemTranscriptionUnsupportedOs => 800,
        RpcErrorCode::SystemTranscriptionPermissionDenied => 801,
        RpcErrorCode::SystemTranscriptionCaptureFailed => 802,
        RpcErrorCode::HistoryMissing => 900,
        RpcErrorCode::HistoryAudioUnavailable => 901,
        RpcErrorCode::HistoryUnavailable => 902,
        RpcErrorCode::HistoryProjectUnavailable => 903,
        RpcErrorCode::HistoryEntryEmpty => 904,
        RpcErrorCode::HistoryTranscriptUnreadable => 905,
        RpcErrorCode::Unknown(value) => value,
    }
}

fn code_from_stored(number: i32) -> Option<RpcErrorCode> {
    Some(match number {
        0 => return None,
        1 => RpcErrorCode::InvalidRequest,
        2 => RpcErrorCode::Busy,
        3 => RpcErrorCode::Internal,
        4 => RpcErrorCode::Cancelled,
        5 => RpcErrorCode::Unauthenticated,
        6 => RpcErrorCode::Unsupported,
        7 => RpcErrorCode::ShuttingDown,
        8 => RpcErrorCode::PayloadTooLarge,
        9 => RpcErrorCode::PermissionDenied,
        10 => RpcErrorCode::DurabilityUnconfirmed,
        11 => RpcErrorCode::SettingsInvalid,
        100 => RpcErrorCode::OverallTimeout,
        101 => RpcErrorCode::RecordingTimeout,
        102 => RpcErrorCode::DeliveryTimeout,
        103 => RpcErrorCode::CaptureFailed,
        104 => RpcErrorCode::NoSpeech,
        105 => RpcErrorCode::RecognitionFailed,
        106 => RpcErrorCode::DeliveryFailed,
        108 => RpcErrorCode::DeliveryTargetChanged,
        109 => RpcErrorCode::DeliveryInterrupted,
        110 => RpcErrorCode::ApplicationExcluded,
        200 => RpcErrorCode::SynthesisFailed,
        201 => RpcErrorCode::PlaybackFailed,
        202 => RpcErrorCode::SpeechTimeout,
        203 => RpcErrorCode::VoiceUnavailable,
        300 => RpcErrorCode::AssistantUnavailable,
        301 => RpcErrorCode::AssistantFailed,
        302 => RpcErrorCode::AssistantTimeout,
        303 => RpcErrorCode::AssistantSignedOut,
        304 => RpcErrorCode::PromptUnreadable,
        305 => RpcErrorCode::PromptMissing,
        306 => RpcErrorCode::PromptsFull,
        307 => RpcErrorCode::PromptWriteFailed,
        500 => RpcErrorCode::SpeakerModelMissing,
        501 => RpcErrorCode::SpeakerAlreadyEnrolled,
        502 => RpcErrorCode::SpeakerNotEnrolled,
        503 => RpcErrorCode::SpeakerInconsistentSamples,
        504 => RpcErrorCode::SpeakerProfileWriteFailed,
        505 => RpcErrorCode::SpeakerNoSpeech,
        800 => RpcErrorCode::SystemTranscriptionUnsupportedOs,
        801 => RpcErrorCode::SystemTranscriptionPermissionDenied,
        802 => RpcErrorCode::SystemTranscriptionCaptureFailed,
        900 => RpcErrorCode::HistoryMissing,
        901 => RpcErrorCode::HistoryAudioUnavailable,
        902 => RpcErrorCode::HistoryUnavailable,
        903 => RpcErrorCode::HistoryProjectUnavailable,
        904 => RpcErrorCode::HistoryEntryEmpty,
        905 => RpcErrorCode::HistoryTranscriptUnreadable,
        other => RpcErrorCode::Unknown(other),
    })
}

fn encode_error(error: &RpcFailure) -> Result<String, HistoryError> {
    if error.message.len() > MAX_ERROR_MESSAGE_BYTES || error.code == RpcErrorCode::Unknown(0) {
        return Err(HistoryError::InvalidRequest);
    }
    let stored = StoredError {
        code: stored_code(error.code),
        message: error.message.clone(),
    };
    let stored = serde_json::to_string(&stored).map_err(HistoryError::storage)?;
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
        let code = code_from_stored(stored.code).ok_or(rusqlite::Error::InvalidQuery)?;
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
    let transcript = row
        .get::<_, Option<Vec<u8>>>(19)?
        .map(|bytes| PathBuf::from(OsString::from_vec(bytes)));
    let content = match (
        row.get::<_, u8>(17)?,
        transcript,
        row.get::<_, Option<String>>(20)?,
    ) {
        (0, None, None) => HistoryContent::Dictation(DictationContent {
            language: row.get(13)?,
            engine: row.get(16)?,
            own_voice_only: row.get(14)?,
            audio,
            retrying: false,
        }),
        (1, Some(transcript), None) => HistoryContent::SystemTranscription {
            transcript,
            read: None,
        },
        (2, None, Some(reply)) => HistoryContent::Chat { reply },
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(HistoryEntry {
        id: unsigned(row.get(0)?, 0)?,
        title: row
            .get::<_, Option<String>>(18)?
            .map(|title| Title::new(&title).map_err(|_| rusqlite::Error::InvalidQuery))
            .transpose()?,
        content,
        created_at_ms: unsigned(row.get(1)?, 1)?,
        duration_ms: row
            .get::<_, Option<i64>>(5)?
            .map(|value| unsigned(value, 5))
            .transpose()?,
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
        project: row
            .get::<_, Option<Vec<u8>>>(15)?
            .map(|bytes| PathBuf::from(OsString::from_vec(bytes))),
        text: row.get(6)?,
        complete: row.get(7)?,
        result_count: 0,
        status,
        error: row
            .get::<_, Option<String>>(9)?
            .map(decode_error)
            .transpose()?,
        warning: None,
        deletion_pending: row.get(12)?,
    })
}

fn result_counts(state: &State) -> Result<HashMap<u64, u32>, HistoryError> {
    let mut statement = state
        .connection
        .prepare("SELECT entry, COUNT(*) FROM results GROUP BY entry")?;
    let counts = statement
        .query_map([], |row| {
            Ok((unsigned(row.get(0)?, 0)?, row.get::<_, u32>(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(counts)
}

/// A pinned dictation is one a retry holds.
fn mark_retrying(state: &State, entry: &mut HistoryEntry) {
    let id = entry.id;
    if let Some(dictation) = entry.dictation_mut() {
        dictation.retrying = state.pinned.contains(&id);
    }
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
    mark_retrying(state, &mut entry);
    entry.result_count = state.connection.query_row(
        "SELECT COUNT(*) FROM results WHERE entry=?1",
        [sql_integer(id)?],
        |row| row.get(0),
    )?;
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
    let counts = result_counts(state)?;
    let mut entries = Vec::new();
    let mut unreadable = Vec::new();
    for row in rows {
        let (id, decoded) = row?;
        match decoded {
            Ok(mut entry) => {
                mark_retrying(state, &mut entry);
                entry.result_count = counts.get(&entry.id).copied().unwrap_or(0);
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

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn entry(text: &str, status: HistoryStatus) -> HistoryEntry {
        HistoryEntry {
            id: 0,
            title: None,
            content: HistoryContent::Dictation(DictationContent {
                language: "en".into(),
                engine: "test".into(),
                own_voice_only: true,
                audio: AudioAvailability::Unavailable,
                retrying: false,
            }),
            created_at_ms: now_ms(),
            duration_ms: Some(1_000),
            source: HistorySource::Local,
            application: Some(Application {
                name: "Editor".into(),
                bundle_id: Some("test.editor".into()),
            }),
            project: Some(PathBuf::from("/project")),
            text: text.into(),
            complete: status != HistoryStatus::TranscriptionFailed,
            result_count: 0,
            status,
            error: None,
            warning: None,
            deletion_pending: false,
        }
    }

    fn audio(directory: &Path) -> PathBuf {
        let path = aria_kit::audio::canonical_fixture(directory);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn query(text: &str, before_id: Option<u64>, limit: usize) -> HistoryQuery {
        HistoryQuery {
            section: None,
            query: text.into(),
            before_id,
            limit,
        }
    }

    fn save(
        store: &Store,
        text: &str,
        status: HistoryStatus,
        audio: Option<&Path>,
    ) -> HistoryEntry {
        store.insert(entry(text, status), audio).unwrap()
    }

    /// Edits the database behind the store's back, as damage or an older
    /// daemon would.
    fn tamper(store: &Store, sql: &str, params: impl rusqlite::Params) {
        store.lock().connection.execute(sql, params).unwrap();
    }

    fn listed(store: &Store) -> Vec<HistoryEntry> {
        store.list(&query("", None, 100)).unwrap().entries
    }

    /// The table and insert as the daemon wrote them before sections.
    const SCHEMA_BEFORE_SECTIONS: &str = "CREATE TABLE entries (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        created INTEGER NOT NULL CHECK(created>=0),
        source INTEGER NOT NULL CHECK(source IN (0,1)),
        app_name TEXT, bundle_id TEXT, duration INTEGER,
        text TEXT NOT NULL CHECK(length(CAST(text AS BLOB))<=131072),
        complete INTEGER NOT NULL CHECK(complete IN (0,1)),
        status INTEGER NOT NULL CHECK(status IN (0,1,2,3)), error TEXT,
        expires INTEGER, audio_present INTEGER NOT NULL DEFAULT 0,
        deleting INTEGER NOT NULL DEFAULT 0,
        language TEXT NOT NULL, own_voice INTEGER NOT NULL,
        project BLOB, engine TEXT NOT NULL)";
    const INSERT_BEFORE_SECTIONS: &str = "INSERT INTO entries(created,source,app_name,bundle_id,duration,text,complete,status,error,language,own_voice,project,engine)
        VALUES(?1,0,NULL,NULL,NULL,?2,1,0,NULL,'en',0,NULL,'test')";
    const SELECT_BEFORE_SECTIONS: &str = "SELECT id,created,source,app_name,bundle_id,duration,text,complete,status,error,expires,audio_present,deleting,language,own_voice,project,engine FROM entries";

    /// A transcript entry as the store keeps one, without reading the file.
    fn system_transcription(text: &str, transcript: &Path) -> HistoryEntry {
        HistoryEntry {
            content: HistoryContent::SystemTranscription {
                transcript: transcript.to_path_buf(),
                read: None,
            },
            ..entry(text, HistoryStatus::Success)
        }
    }
    /// A store in a fresh folder, which lives as long as the guard.
    fn opened() -> (tempfile::TempDir, Arc<Store>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        (directory, store)
    }

    /// Saves `file` as a transcript entry that just ended.
    fn linked(store: &Store, file: &Path) -> HistoryEntry {
        store
            .save_transcript(file, now_ms(), 0, None)
            .unwrap()
            .unwrap()
    }

    /// A transcript file as a session leaves it, in `directory/transcripts`.
    fn transcript(directory: &Path, text: &str) -> PathBuf {
        let folder = directory.join("transcripts");
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("2026-11-04T10-00-00Z.txt");
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[test]
    fn a_database_from_before_sections_keeps_its_rows_as_dictations_at_version_one() {
        let directory = tempfile::tempdir().unwrap();
        drop(Store::open(directory.path()).unwrap());
        let database = Connection::open(directory.path().join("history.db")).unwrap();
        database
            .execute_batch(&format!("DROP TABLE entries; {SCHEMA_BEFORE_SECTIONS};"))
            .unwrap();
        database
            .execute(INSERT_BEFORE_SECTIONS, params![now_ms() as i64, "old row"])
            .unwrap();
        drop(database);

        let store = Store::open(directory.path()).unwrap();

        let entries = listed(&store);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "old row");
        assert!(matches!(entries[0].content, HistoryContent::Dictation(_)));
        let version: u32 = store
            .lock()
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        drop(store);
        Store::open(directory.path()).unwrap();
    }

    #[test]
    fn an_older_daemon_still_inserts_and_reads_after_the_migration() {
        let (_directory, store) = opened();
        store
            .insert(
                system_transcription("session", Path::new("/no-folder/session.txt")),
                None,
            )
            .unwrap();

        tamper(
            &store,
            INSERT_BEFORE_SECTIONS,
            params![now_ms() as i64, "from an older daemon"],
        );
        let older_reads = store
            .lock()
            .connection
            .prepare(SELECT_BEFORE_SECTIONS)
            .unwrap()
            .query_map([], |row| row.get::<_, String>(6))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(older_reads, ["session", "from an older daemon"]);
        let sections = listed(&store)
            .iter()
            .map(|entry| (entry.text.clone(), entry.content.section()))
            .collect::<Vec<_>>();
        assert_eq!(
            sections,
            [
                ("from an older daemon".to_owned(), HistorySection::Dictation),
                ("session".to_owned(), HistorySection::SystemTranscription),
            ]
        );
    }

    #[test]
    fn list_filters_by_section_and_its_cursor_skips_other_sections() {
        let (_directory, store) = opened();
        let first = save(&store, "one", HistoryStatus::Success, None);
        store
            .insert(
                system_transcription("session", Path::new("/no-folder/session.txt")),
                None,
            )
            .unwrap();
        let second = save(&store, "two", HistoryStatus::Success, None);
        let dictations = |before_id| HistoryQuery {
            section: Some(HistorySection::Dictation),
            ..query("", before_id, 1)
        };

        let page = store.list(&dictations(None)).unwrap();
        assert_eq!(page.entries[0].id, second.id);
        assert_eq!(page.next_before_id, Some(second.id));
        let page = store.list(&dictations(page.next_before_id)).unwrap();
        assert_eq!(page.entries[0].id, first.id);
        assert_eq!(page.next_before_id, None);
        assert_eq!(listed(&store).len(), 3);
    }

    #[test]
    fn clearing_mac_audio_removes_its_files_and_keeps_dictations() {
        let (directory, store) = opened();
        save(&store, "dictated", HistoryStatus::Success, None);
        let file = transcript(directory.path(), "[00:00:01] heard");
        store.save_transcript(&file, now_ms(), 1_000, None).unwrap();

        let cleared = store
            .clear(Some(HistorySection::SystemTranscription))
            .unwrap();

        assert_eq!(cleared.deleted_entries, 1);
        assert!(!cleared.cleanup_pending);
        assert!(!file.exists());
        let remaining = listed(&store);
        assert_eq!(remaining.len(), 1);
        assert!(matches!(remaining[0].content, HistoryContent::Dictation(_)));
    }

    #[test]
    fn a_rename_sets_a_trimmed_title_that_search_finds_and_none_removes() {
        let (directory, store) = opened();
        let saved = save(&store, "body text", HistoryStatus::Success, None);

        let title = Title::new("  Weekly plan ").unwrap();
        let renamed = store.rename(saved.id, Some(&title)).unwrap();

        assert_eq!(
            renamed.title.as_ref().map(Title::as_str),
            Some("Weekly plan")
        );
        assert_eq!(
            store
                .list(&query("weekly", None, 10))
                .unwrap()
                .entries
                .len(),
            1
        );
        drop(store);
        let store = Store::open(directory.path()).unwrap();
        assert_eq!(store.get(saved.id).unwrap().title, Some(title));
        assert_eq!(store.rename(saved.id, None).unwrap().title, None);
    }

    #[test]
    fn a_blank_overlong_or_multiline_title_is_invalid() {
        for title in ["", "   ", "two\nlines", "tab\tinside", &"é".repeat(129)] {
            assert!(
                matches!(Title::new(title), Err(HistoryError::InvalidTitle)),
                "{title:?}"
            );
        }
        assert!(Title::new(&"a".repeat(Title::MAX_BYTES)).is_ok());
        let error = Title::new("").expect_err("blank");
        assert_eq!(
            error.to_string(),
            "a title must be one line of 1 to 256 bytes"
        );
        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::InvalidRequest);
    }

    #[test]
    fn a_prompt_reads_a_chats_reply_and_any_other_entrys_text() {
        let chat = HistoryEntry {
            content: HistoryContent::Chat {
                reply: "Friday.".into(),
            },
            ..entry("When?", HistoryStatus::Success)
        };
        let whole = HistoryEntry {
            content: HistoryContent::SystemTranscription {
                transcript: PathBuf::from("/transcripts/session.txt"),
                read: Some(TranscriptRead::Whole),
            },
            ..entry("[00:00:01] Ship it.", HistoryStatus::Success)
        };

        assert_eq!(chat.prompt_text().unwrap(), "Friday.");
        assert_eq!(whole.prompt_text().unwrap(), "[00:00:01] Ship it.");
        assert_eq!(
            entry("Hello", HistoryStatus::Success)
                .prompt_text()
                .unwrap(),
            "Hello"
        );
    }

    #[test]
    fn a_prompt_cannot_read_a_blank_entry() {
        let blank_chat = HistoryEntry {
            content: HistoryContent::Chat {
                reply: " \n".into(),
            },
            ..entry("When?", HistoryStatus::Success)
        };

        for blank in [entry(" \n", HistoryStatus::Success), blank_chat] {
            let error = blank.prompt_text().expect_err("no text");
            assert!(matches!(error, HistoryError::EntryEmpty));
            assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::HistoryEntryEmpty);
        }
    }

    #[test]
    fn a_prompt_reads_a_mac_audio_transcript_only_whole() {
        for read in [
            None,
            Some(TranscriptRead::Truncated),
            Some(TranscriptRead::Unavailable),
        ] {
            let partial = HistoryEntry {
                content: HistoryContent::SystemTranscription {
                    transcript: PathBuf::from("/transcripts/session.txt"),
                    read,
                },
                ..entry("[00:00:01] Ship it.", HistoryStatus::Success)
            };

            let error = partial.prompt_text().expect_err("only part is read");
            assert!(
                matches!(error, HistoryError::TranscriptUnreadable),
                "{read:?}"
            );
            assert_eq!(
                RpcErrorCode::from(&error),
                RpcErrorCode::HistoryTranscriptUnreadable
            );
        }
    }

    #[test]
    fn renaming_a_missing_or_deleting_entry_reports_it_missing() {
        let (_directory, store) = opened();
        let title = Title::new("Name").unwrap();
        assert!(matches!(
            store.rename(99, Some(&title)),
            Err(HistoryError::Missing)
        ));
        let saved = save(&store, "text", HistoryStatus::Success, None);
        // Pinned, as by a regeneration, so maintenance keeps the deleting row.
        store.lock().pinned.insert(saved.id);
        tamper(
            &store,
            "UPDATE entries SET deleting=1 WHERE id=?1",
            [saved.id as i64],
        );

        assert!(matches!(
            store.rename(saved.id, Some(&title)),
            Err(HistoryError::Missing)
        ));
    }

    #[test]
    fn a_rename_during_a_regeneration_survives_its_recovery() {
        let (directory, store) = opened();
        let saved = store
            .insert(
                entry("old", HistoryStatus::TranscriptionFailed),
                Some(&audio(directory.path())),
            )
            .unwrap();
        let lease = store.begin_retry(saved.id).unwrap();
        let title = Title::new("Kept").unwrap();

        store.rename(saved.id, Some(&title)).unwrap();
        let recovered = store.recover(saved.id, "new", "test", "en", false).unwrap();
        drop(lease);

        assert_eq!(recovered.text, "new");
        assert_eq!(recovered.title, Some(title));
    }

    #[test]
    fn a_result_is_kept_under_its_entry_newest_first_and_counted() {
        let (_directory, store) = opened();
        let entry = save(&store, "standup notes", HistoryStatus::Success, None);

        store
            .save_result(entry.id, "Summarize", "Short.", 1)
            .unwrap();
        store
            .save_result(entry.id, "Translate", "Kurz.", 2)
            .unwrap();

        let results = store.results(entry.id).unwrap();
        assert_eq!(
            results
                .iter()
                .map(|result| result.prompt_name.as_str())
                .collect::<Vec<_>>(),
            ["Translate", "Summarize"]
        );
        assert_eq!(listed(&store)[0].result_count, 2);
        assert_eq!(store.get(entry.id).unwrap().result_count, 2);
        assert!(
            store
                .list(&query("Kurz", None, 10))
                .unwrap()
                .entries
                .is_empty(),
            "search skips results"
        );
    }

    #[test]
    fn an_entry_keeps_only_its_newest_results() {
        let (_directory, store) = opened();
        let entry = save(&store, "notes", HistoryStatus::Success, None);

        for run in 0..=RESULTS_PER_ENTRY {
            store
                .save_result(entry.id, "Summarize", &format!("run {run}"), 1)
                .unwrap();
        }

        let results = store.results(entry.id).unwrap();
        assert_eq!(results.len(), RESULTS_PER_ENTRY);
        assert_eq!(results.last().unwrap().text, "run 1");
    }

    #[test]
    fn results_share_one_text_budget_and_the_oldest_go_first() {
        let (_directory, store) = opened();
        let first = save(&store, "first", HistoryStatus::Success, None);
        let text = "x".repeat(MAX_ENTRY_BYTES);
        store.save_result(first.id, "Summarize", &text, 1).unwrap();

        let fill = RESULTS_TEXT_BYTES / MAX_ENTRY_BYTES;
        for run in 0..fill {
            if run % RESULTS_PER_ENTRY == 0 {
                save(&store, "more", HistoryStatus::Success, None);
            }
            let newest = listed(&store)[0].id;
            store.save_result(newest, "Summarize", &text, 2).unwrap();
        }

        assert!(store.results(first.id).unwrap().is_empty());
    }

    #[test]
    fn a_result_needs_its_entry() {
        let (_directory, store) = opened();

        assert!(matches!(
            store.save_result(99, "Summarize", "Short.", 1),
            Err(HistoryError::Missing)
        ));
    }

    #[test]
    fn deleting_or_clearing_an_entry_takes_its_results() {
        let (_directory, store) = opened();
        let deleted = save(&store, "deleted", HistoryStatus::Success, None);
        let cleared = save(&store, "cleared", HistoryStatus::Success, None);
        store
            .save_result(deleted.id, "Summarize", "one", 1)
            .unwrap();
        store
            .save_result(cleared.id, "Summarize", "two", 1)
            .unwrap();

        let left = |store: &Store| -> i64 {
            store
                .lock()
                .connection
                .query_row("SELECT COUNT(*) FROM results", [], |row| row.get(0))
                .unwrap()
        };

        store.delete(deleted.id).unwrap();
        assert_eq!(
            left(&store),
            1,
            "a delete takes only its own entry's results"
        );
        store.clear(Some(HistorySection::Dictation)).unwrap();
        assert_eq!(left(&store), 0);
    }

    #[test]
    fn results_whose_entry_an_older_daemon_deleted_are_swept() {
        let (directory, store) = opened();
        let entry = save(&store, "notes", HistoryStatus::Success, None);
        store
            .save_result(entry.id, "Summarize", "Short.", 1)
            .unwrap();

        drop(store);
        // An older daemon's connection, with foreign keys off as SQLite starts.
        let database = Connection::open(directory.path().join("history.db")).unwrap();
        database
            .execute("DELETE FROM entries WHERE id=?1", [entry.id as i64])
            .unwrap();
        drop(database);
        let store = Store::open(directory.path()).unwrap();
        store.maintenance().unwrap();

        let left: i64 = store
            .lock()
            .connection
            .query_row("SELECT COUNT(*) FROM results", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn one_sections_entries_never_evict_another_sections() {
        let (_directory, store) = opened();
        let session = store
            .insert(
                system_transcription("session", Path::new("/no-folder/session.txt")),
                None,
            )
            .unwrap();
        let text = "x".repeat(MAX_ENTRY_BYTES);
        let fill = budget(HistorySection::Dictation).text_bytes / MAX_ENTRY_BYTES + 1;
        for _ in 0..fill {
            save(&store, &text, HistoryStatus::Success, None);
        }

        let entries = listed(&store);
        assert!(entries.iter().any(|entry| entry.id == session.id));
        assert!(entries.len() < fill + 1);
    }

    #[test]
    fn a_chat_keeps_its_reply_which_search_finds_and_its_budget_counts() {
        let (_directory, store) = opened();
        let asked = HistoryEntry {
            content: HistoryContent::Chat {
                reply: "Tuesday at noon".into(),
            },
            ..entry("When is lunch?", HistoryStatus::Success)
        };

        let saved = store.insert(asked.clone(), None).unwrap();

        assert_eq!(store.get(saved.id).unwrap().content, asked.content);
        assert_eq!(
            store.list(&query("noon", None, 10)).unwrap().entries.len(),
            1
        );
        assert_eq!(
            stored_bytes(&saved),
            "When is lunch?".len() + "Tuesday at noon".len()
        );
        let oversized = HistoryEntry {
            content: HistoryContent::Chat {
                reply: "x".repeat(MAX_ENTRY_BYTES + 1),
            },
            ..asked
        };
        assert!(matches!(
            store.insert(oversized, None),
            Err(HistoryError::TooLarge)
        ));
    }

    #[test]
    fn a_linked_transcript_keeps_its_first_kibibyte_and_get_reads_it_whole() {
        let (directory, store) = opened();
        let text = format!("{}é tail", "a".repeat(EXCERPT_BYTES - 1));
        let file = transcript(directory.path(), &text);

        let linked = store
            .save_transcript(&file, now_ms(), 5_000, None)
            .unwrap()
            .unwrap();

        assert_eq!(linked.text, "a".repeat(EXCERPT_BYTES - 1));
        assert_eq!(linked.status, HistoryStatus::Success);
        let got = store.get(linked.id).unwrap();
        assert_eq!(got.text, text);
        assert_eq!(
            got.content,
            HistoryContent::SystemTranscription {
                transcript: file,
                read: Some(TranscriptRead::Whole)
            }
        );
    }

    #[test]
    fn an_empty_transcript_is_not_linked() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "\n\n");

        assert!(
            store
                .save_transcript(&file, now_ms(), 0, None)
                .unwrap()
                .is_none()
        );
        assert!(listed(&store).is_empty());
    }

    #[test]
    fn a_session_that_failed_is_linked_as_a_failed_transcription() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] partial");
        let failure = RpcFailure::new(
            RpcErrorCode::SystemTranscriptionCaptureFailed,
            "capture stopped",
        );

        let linked = store
            .save_transcript(&file, now_ms(), 1_000, Some(failure.clone()))
            .unwrap()
            .unwrap();

        assert_eq!(linked.status, HistoryStatus::TranscriptionFailed);
        assert_eq!(linked.error, Some(failure));
    }

    #[test]
    fn get_reads_a_long_transcript_up_to_one_mebibyte_and_says_so() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), &"é".repeat(MAX_TRANSCRIPT_READ as usize));
        let linked = linked(&store, &file);

        let got = store.get(linked.id).unwrap();

        assert_eq!(got.text.len(), MAX_TRANSCRIPT_READ as usize);
        assert!(matches!(
            got.content,
            HistoryContent::SystemTranscription {
                read: Some(TranscriptRead::Truncated),
                ..
            }
        ));
    }

    #[test]
    fn get_of_a_transcript_replaced_by_a_link_returns_the_excerpt_as_unavailable() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "kept excerpt");
        let linked = linked(&store, &file);
        let elsewhere = directory.path().join("elsewhere.txt");
        fs::write(&elsewhere, "someone else's").unwrap();
        fs::remove_file(&file).unwrap();
        symlink(&elsewhere, &file).unwrap();

        let got = store.get(linked.id).unwrap();

        assert_eq!(got.text, "kept excerpt");
        assert!(matches!(
            got.content,
            HistoryContent::SystemTranscription {
                read: Some(TranscriptRead::Unavailable),
                ..
            }
        ));
    }

    #[test]
    fn deleting_a_mac_audio_entry_removes_its_file_and_its_row() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        let linked = linked(&store, &file);

        assert!(store.delete(linked.id).unwrap());

        assert!(!file.exists());
        assert!(listed(&store).is_empty());
    }

    #[test]
    fn a_transcript_path_that_became_a_link_is_left_and_its_target_kept() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        let linked = linked(&store, &file);
        let elsewhere = directory.path().join("elsewhere.txt");
        fs::write(&elsewhere, "keep me").unwrap();
        fs::remove_file(&file).unwrap();
        symlink(&elsewhere, &file).unwrap();

        assert!(store.delete(linked.id).unwrap());

        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "keep me");
        assert!(file.symlink_metadata().is_ok());
        assert!(listed(&store).is_empty());
    }

    #[test]
    fn a_transcript_that_cant_be_removed_keeps_its_row() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        let linked = linked(&store, &file);
        let folder = file.parent().unwrap();
        fs::set_permissions(folder, fs::Permissions::from_mode(0o500)).unwrap();

        let deleted = store.delete(linked.id);
        fs::set_permissions(folder, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(deleted, Err(HistoryError::Storage(_))));
        assert!(file.exists());
        assert_eq!(listed(&store).len(), 1);
    }

    #[test]
    fn maintenance_drops_a_row_whose_file_is_gone_only_while_its_folder_is_there() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        linked(&store, &file);
        store
            .insert(
                system_transcription("unmounted", Path::new("/no-folder/session.txt")),
                None,
            )
            .unwrap();

        fs::remove_file(&file).unwrap();
        store.maintenance().unwrap();

        let remaining = listed(&store);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "unmounted");
    }

    #[test]
    fn expiry_removes_a_mac_audio_row_but_keeps_its_file() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        let linked = linked(&store, &file);
        tamper(
            &store,
            "UPDATE entries SET created=0 WHERE id=?1",
            [linked.id as i64],
        );

        store.maintenance().unwrap();

        assert!(listed(&store).is_empty());
        assert!(file.exists());
    }
    #[test]
    fn typed_errors_and_existing_plain_errors_survive_reopening() {
        let (directory, store) = opened();
        let failures = [
            RpcFailure::new(
                RpcErrorCode::DeliveryTargetChanged,
                "Delivery target changed.",
            ),
            (&HistoryError::ProjectUnavailable).into(),
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
            let id = save(&store, "keep transcript", HistoryStatus::Success, None).id;
            tamper(
                &store,
                "UPDATE entries SET error=?1 WHERE id=?2",
                params![text, sql_integer(id).unwrap()],
            );
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
        let (_directory, store) = opened();
        let kept = save(&store, "kept", HistoryStatus::Success, None);
        let damaged = save(&store, "damaged", HistoryStatus::Success, None);
        tamper(
            &store,
            "UPDATE entries SET error='{broken' WHERE id=?1",
            params![sql_integer(damaged.id).unwrap()],
        );

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
    fn stored_codes_keep_their_numbers_and_read_back() {
        for (code, number) in [
            (RpcErrorCode::InvalidRequest, 1),
            (RpcErrorCode::Internal, 3),
            (RpcErrorCode::SettingsInvalid, 11),
            (RpcErrorCode::DeliveryTargetChanged, 108),
            (RpcErrorCode::PromptUnreadable, 304),
            (RpcErrorCode::PromptMissing, 305),
            (RpcErrorCode::PromptsFull, 306),
            (RpcErrorCode::PromptWriteFailed, 307),
            (RpcErrorCode::HistoryProjectUnavailable, 903),
            (RpcErrorCode::HistoryEntryEmpty, 904),
            (RpcErrorCode::HistoryTranscriptUnreadable, 905),
            (RpcErrorCode::Unknown(9_999), 9_999),
        ] {
            assert_eq!(stored_code(code), number);
            assert_eq!(code_from_stored(number), Some(code));
        }
        assert_eq!(code_from_stored(0), None);
    }

    #[test]
    fn malformed_or_oversized_stored_errors_are_rejected() {
        for error in [
            "{broken".to_owned(),
            "  {broken".to_owned(),
            r#"{"code":0,"message":"missing category"}"#.to_owned(),
            r#"{"code":"not_a_code","message":"broken"}"#.to_owned(),
            serde_json::to_string(&StoredError {
                code: stored_code(RpcErrorCode::Internal),
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
        let (directory, store) = opened();
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
            found.application.as_ref().unwrap().bundle_id.as_deref(),
            Some("test.editor")
        );
        assert_eq!(found.project.as_deref(), Some(Path::new("/project")));
        assert!(found.dictation().unwrap().own_voice_only);
        assert_eq!(found.dictation().unwrap().engine, "test");
        assert!(store.delete(first.id).unwrap());
        assert!(!store.delete(first.id).unwrap());
        let second = save(&store, "next", HistoryStatus::Success, None);
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
        store.mark_delete(&mut store.lock(), last_id).unwrap();
        fs::write(history.join("audio/9999.wav"), "orphan").unwrap();
        fs::write(history.join("audio/.copy-interrupted"), "partial").unwrap();
        assert_eq!(
            store.clear(None).unwrap(),
            ClearHistoryResult {
                deleted_entries: 8,
                cleanup_pending: false,
            }
        );
        assert!(listed(&store).is_empty());
        assert_eq!(fs::read_dir(history.join("audio")).unwrap().count(), 0);
        assert!(source.exists());
        assert!(matches!(Store::open(&history), Err(HistoryError::Busy)));
        assert_eq!(store.clear(None).unwrap().deleted_entries, 0);
        drop(store);
        let store = Store::open(&history).unwrap();
        let later = save(&store, "later", HistoryStatus::Success, None);
        assert!(later.id > last_id);
        assert_eq!(store.get(later.id).unwrap().text, "later");
    }

    #[test]
    fn history_clear_busy_and_database_failure_leave_entries_intact() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let saved = save(&store, "retry", HistoryStatus::Success, Some(&source));
        let other = save(&store, "other", HistoryStatus::Success, None);
        let lease = store.begin_retry(saved.id).unwrap();
        assert!(matches!(store.clear(None), Err(HistoryError::Busy)));
        assert_eq!(store.get(other.id).unwrap().text, "other");
        assert!(store.audio_path(lease.entry.id).exists());
        drop(lease);
        store
            .lock()
            .connection
            .pragma_update(None, "query_only", true)
            .unwrap();
        let failure = store.clear(None).err().unwrap();
        assert!(matches!(failure, HistoryError::Storage(_)));
        assert!(
            std::error::Error::source(&failure)
                .is_some_and(|cause| cause.downcast_ref::<rusqlite::Error>().is_some())
        );
        store
            .lock()
            .connection
            .pragma_update(None, "query_only", false)
            .unwrap();
        assert_eq!(listed(&store).len(), 2);
        assert!(store.audio_path(saved.id).exists());
        assert_eq!(store.clear(None).unwrap().deleted_entries, 2);
    }

    #[test]
    fn history_clear_reports_audio_failure_and_reopen_finishes_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let history = directory.path().join("history");
        let store = Store::open(&history).unwrap();
        let saved = save(&store, "private", HistoryStatus::Success, Some(&source));
        let audio_directory = history.join("audio");
        fs::set_permissions(&audio_directory, fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(
            store.clear(None).unwrap(),
            ClearHistoryResult {
                deleted_entries: 1,
                cleanup_pending: true,
            }
        );
        assert!(matches!(store.get(saved.id), Err(HistoryError::Missing)));
        assert!(store.audio_path(saved.id).exists());
        assert!(store.clear(None).unwrap().cleanup_pending);
        fs::set_permissions(&audio_directory, fs::Permissions::from_mode(0o700)).unwrap();
        drop(store);
        let store = Store::open(&history).unwrap();
        assert!(!store.audio_path(saved.id).exists());
        assert!(!store.clear(None).unwrap().cleanup_pending);
    }

    #[test]
    fn history_clear_reports_sweep_failure_after_deleting_text() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("history")).unwrap();
        save(&store, "private", HistoryStatus::Success, None);
        let audio_directory = store.directory.join("audio");
        let moved = store.directory.join("moved-audio");
        fs::rename(&audio_directory, &moved).unwrap();
        assert_eq!(
            store.clear(None).unwrap(),
            ClearHistoryResult {
                deleted_entries: 1,
                cleanup_pending: true,
            }
        );
        fs::rename(&moved, &audio_directory).unwrap();
        store.maintenance().unwrap();
        assert!(listed(&store).is_empty());
    }

    #[test]
    fn history_ownership_outlives_the_store_handle_until_retry_releases() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let history = directory.path().join("history");
        let store = Store::open(&history).unwrap();
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(&source),
        );
        let lease = store.begin_retry(failed.id).unwrap();
        let audio_path = store.audio_path(lease.entry.id);
        assert!(matches!(Store::open(&history), Err(HistoryError::Busy)));
        assert!(audio_path.exists());
        drop(store);
        assert!(matches!(Store::open(&history), Err(HistoryError::Busy)));
        drop(lease);
        let reopened = Store::open(&history).unwrap();
        assert_eq!(reopened.get(failed.id).unwrap().text, "partial");
        assert!(audio_path.exists());
        assert!(
            !reopened
                .get(failed.id)
                .unwrap()
                .dictation()
                .unwrap()
                .retrying
        );
    }

    #[test]
    fn failed_attempt_stores_the_top_line_of_a_long_cause_chain() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(&source),
        );
        let _lease = store.begin_retry(failed.id).unwrap();
        let error = HistoryError::transcription(
            RpcErrorCode::RecognitionFailed,
            io::Error::other("x".repeat(MAX_ERROR_MESSAGE_BYTES)),
        );
        assert!(error_chain(&error).len() > MAX_ERROR_MESSAGE_BYTES);
        store.attempt_failed(failed.id, &error).unwrap();
        assert_eq!(
            store.get(failed.id).unwrap().error,
            Some(RpcFailure::new(
                RpcErrorCode::RecognitionFailed,
                "history transcription failed"
            ))
        );
    }

    #[test]
    fn retry_pins_original_audio_and_recovers_the_same_entry() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(&source),
        );
        assert!(source.exists());
        assert!(matches!(
            failed.audio(),
            AudioAvailability::Available { .. }
        ));
        let lease = store.begin_retry(failed.id).unwrap();
        assert!(matches!(
            store.begin_retry(failed.id),
            Err(HistoryError::Busy)
        ));
        assert!(matches!(store.delete(failed.id), Err(HistoryError::Busy)));
        store
            .attempt_failed(failed.id, &HistoryError::Cancelled)
            .unwrap();
        assert_eq!(
            store.get(failed.id).unwrap().error,
            Some((&HistoryError::Cancelled).into())
        );
        assert_eq!(store.get(failed.id).unwrap().text, "partial");
        let recovered = store
            .recover(failed.id, "whole transcript", "whisper", "en", false)
            .unwrap();
        assert_eq!(recovered.id, failed.id);
        assert_eq!(recovered.created_at_ms, failed.created_at_ms);
        assert_eq!(recovered.status, HistoryStatus::Recovered);
        assert!(recovered.error.is_none());
        assert_eq!(recovered.audio(), AudioAvailability::Unavailable);
        assert!(!store.audio_path(lease.entry.id).exists());
        drop(lease);
        assert!(!store.get(failed.id).unwrap().dictation().unwrap().retrying);
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
        let original = save(&store, "first", HistoryStatus::Success, Some(&source));
        for (text, engine, language) in [("second", "whisper", "fr"), ("third", "moonshine", "en")]
        {
            let lease = store.begin_retry(original.id).unwrap();
            let recovered = store
                .recover(original.id, text, engine, language, true)
                .unwrap();
            assert_eq!(recovered.audio(), original.audio());
            assert_eq!(recovered.text, text);
            assert_eq!(recovered.dictation().unwrap().engine, engine);
            assert_eq!(recovered.dictation().unwrap().language, language);
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
        assert_eq!(recovered.audio(), AudioAvailability::Unavailable);
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
        let failed = save(&store, "partial", HistoryStatus::Success, Some(&source));
        let lease = store.begin_retry(failed.id).unwrap();
        tamper(
            &store,
            "UPDATE entries SET expires=1 WHERE id=?1",
            [sql_integer(failed.id).unwrap()],
        );
        store.maintenance().unwrap();
        assert!(store.audio_path(lease.entry.id).exists());
        assert_eq!(
            store.get(failed.id).unwrap().audio(),
            AudioAvailability::Expired
        );
        let path = store.audio_path(lease.entry.id);
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
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(&source),
        );
        let later = save(&store, "later text", HistoryStatus::Success, None);
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
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(&source),
        );
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
        let pending = save(&store, "pending", HistoryStatus::Success, None);
        {
            let mut state = store.lock();
            store.mark_delete(&mut state, pending.id).unwrap();
        }
        assert!(matches!(store.get(pending.id), Err(HistoryError::Missing)));
    }

    #[test]
    fn recovery_commit_survives_audio_cleanup_failure() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(&source),
        );
        let lease = store.begin_retry(failed.id).unwrap();
        let path = store.audio_path(lease.entry.id);
        let audio_directory = path.parent().unwrap().to_path_buf();
        fs::set_permissions(&audio_directory, fs::Permissions::from_mode(0o500)).unwrap();
        let recovered = store
            .recover(failed.id, "recovered text", "whisper", "en", false)
            .unwrap();
        assert_eq!(recovered.status, HistoryStatus::Recovered);
        assert!(recovered.error.is_none());
        assert!(recovered.warning.is_some());
        assert_eq!(recovered.audio(), AudioAvailability::Unavailable);
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
            let saved = save(
                &store,
                "partial",
                HistoryStatus::TranscriptionFailed,
                Some(&path),
            );
            assert_eq!(saved.audio(), AudioAvailability::Unavailable);
            assert!(saved.warning.is_some());
            assert!(matches!(
                store.begin_retry(saved.id),
                Err(HistoryError::AudioUnavailable)
            ));
        }
        let source = audio(directory.path());
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(&source),
        );
        fs::rename(
            store.audio_path(failed.id),
            directory.path().join("removed.wav"),
        )
        .unwrap();
        assert_eq!(
            store.get(failed.id).unwrap().audio(),
            AudioAvailability::Unavailable
        );
    }

    #[test]
    fn invalid_retained_audio_stays_unavailable_until_cleanup_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let corrupt = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(&source),
        );
        fs::write(store.audio_path(corrupt.id), "corrupted after retention").unwrap();
        assert!(matches!(
            store.begin_retry(corrupt.id),
            Err(HistoryError::AudioUnavailable)
        ));
        assert_eq!(
            store.get(corrupt.id).unwrap().audio(),
            AudioAvailability::Unavailable
        );
        assert!(!store.audio_path(corrupt.id).exists());

        let insecure = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(&source),
        );
        let path = store.audio_path(insecure.id);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            store.get(insecure.id).unwrap().audio(),
            AudioAvailability::Unavailable
        );
        assert!(!path.exists());
        assert!(matches!(
            store.begin_retry(insecure.id),
            Err(HistoryError::AudioUnavailable)
        ));
        assert_eq!(
            store.get(insecure.id).unwrap().audio(),
            AudioAvailability::Unavailable
        );
    }

    #[test]
    fn search_is_unicode_literal_and_cursor_order_survives_deletion() {
        let (_directory, store) = opened();
        let first = save(&store, "CAFÉ [x]", HistoryStatus::Success, None);
        let middle = save(&store, "unrelated", HistoryStatus::Success, None);
        let last = save(&store, "café [x]", HistoryStatus::Success, None);
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
        let (_directory, store) = opened();
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
        let saved = save(&store, "text", HistoryStatus::Success, None);
        tamper(
            &store,
            "UPDATE entries SET duration=-1 WHERE id=?1",
            [sql_integer(saved.id).unwrap()],
        );
        assert!(matches!(store.get(saved.id), Err(HistoryError::Missing)));
    }

    #[test]
    fn entry_total_text_and_age_limits_preserve_newer_content() {
        let (_directory, store) = opened();
        let first = save(&store, "keep until full", HistoryStatus::Success, None);
        assert!(matches!(
            store.insert(
                entry(&"x".repeat(MAX_ENTRY_BYTES + 1), HistoryStatus::Success),
                None
            ),
            Err(HistoryError::TooLarge)
        ));
        assert!(store.get(first.id).is_ok());
        for _ in 0..(budget(HistorySection::Dictation).text_bytes / MAX_ENTRY_BYTES) {
            save(
                &store,
                &"x".repeat(MAX_ENTRY_BYTES),
                HistoryStatus::Success,
                None,
            );
        }
        assert!(matches!(store.get(first.id), Err(HistoryError::Missing)));
        let state = store.lock();
        assert_eq!(
            all(&state)
                .unwrap()
                .0
                .iter()
                .map(|entry| entry.text.len())
                .sum::<usize>(),
            budget(HistorySection::Dictation).text_bytes
        );
        state
            .connection
            .execute("UPDATE entries SET created=1", [])
            .unwrap();
        drop(state);
        assert!(listed(&store).is_empty());
    }

    #[test]
    fn entry_count_includes_empty_failed_transcripts() {
        let (_directory, store) = opened();
        let first = save(&store, "", HistoryStatus::TranscriptionFailed, None);
        // Seed terminal records directly to avoid a thousand fsyncs in this unit test.
        let state = store.lock();
        let transaction = state.connection.unchecked_transaction().unwrap();
        for _ in 1..budget(HistorySection::Dictation).entries {
            transaction.execute("INSERT INTO entries(created,source,text,complete,status,language,own_voice,engine) VALUES(?1,0,'',0,1,'en',1,'test')", [sql_integer(now_ms()).unwrap()]).unwrap();
        }
        transaction.commit().unwrap();
        drop(state);
        save(&store, "", HistoryStatus::TranscriptionFailed, None);
        assert!(matches!(store.get(first.id), Err(HistoryError::Missing)));
        assert_eq!(
            all(&store.lock()).unwrap().0.len(),
            budget(HistorySection::Dictation).entries
        );
    }

    #[test]
    fn audio_budget_evicts_oldest_recording_without_losing_its_text() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio(directory.path());
        let store = Store::open(&directory.path().join("history")).unwrap();
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(save(&store, "partial", HistoryStatus::Success, Some(&source)).id);
        }
        let lease = store.begin_retry(ids[0]).unwrap();
        for id in &ids {
            let file = open_private(&store.audio_directory, &audio_name(*id)).unwrap();
            file.set_len(40 * 1024 * 1024).unwrap();
        }
        store.maintenance().unwrap();
        assert!(!store.audio_path(ids[1]).exists());
        assert_eq!(store.get(ids[1]).unwrap().text, "partial");
        assert_eq!(
            store.get(ids[1]).unwrap().audio(),
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
        assert!(matches!(
            Store::open(&history),
            Err(HistoryError::Storage(_))
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
        let other = directory.path().join("other");
        private_fs::prepare_directory(&other).unwrap();
        symlink(&target, other.join("history.db-journal")).unwrap();
        assert!(matches!(Store::open(&other), Err(HistoryError::Storage(_))));
        let third = directory.path().join("third");
        let store = Store::open(&third).unwrap();
        drop(store);
        fs::set_permissions(third.join("history.db"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(Store::open(&third), Err(HistoryError::Storage(_))));
    }

    #[test]
    fn startup_removes_owned_orphans_without_touching_other_files() {
        let (directory, store) = opened();
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
        let (directory, store) = opened();
        let state = store.lock();
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
        let (directory, store) = opened();
        let contender = Connection::open(directory.path().join("history.db")).unwrap();
        contender.execute_batch("BEGIN EXCLUSIVE").unwrap();
        assert!(matches!(
            store.insert(entry("private", HistoryStatus::Success), None),
            Err(HistoryError::Busy)
        ));
        contender.execute_batch("ROLLBACK").unwrap();
        let state = store.lock();
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
        assert!(listed(&store).is_empty());
    }

    #[test]
    fn the_results_table_holds_a_name_as_long_as_a_prompt_may_have() {
        assert!(RESULTS_SCHEMA.contains(&format!(
            "length(CAST(prompt_name AS BLOB)) BETWEEN 1 AND {MAX_PROMPT_NAME_BYTES})"
        )));
    }
}
