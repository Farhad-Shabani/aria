use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aria_kit::cancel::CancelToken;
use aria_platform::sqlite::{AtRest, Database, DriftedMode, Ownership, Policy};
use rusqlite::functions::FunctionFlags;
use rusqlite::{OptionalExtension, params};

use super::{
    AudioAvailability, ChatPage, ChatQuery, ClearHistoryResult, DictationContent, HistoryContent,
    HistoryEntry, HistoryError, HistoryPage, HistoryQuery, HistorySection, HistorySource,
    HistoryStatus, PromptResult, Title, TranscriptRead,
};
use aria_kit::audio::{CaptureError, RecordingReader, encode_wav};
use aria_platform::delivery::Application;
use aria_primitives::error_chain;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_settings::{ChatKey, ChatName, MAX_PROMPT_NAME_BYTES, Quote};

use crate::transcript_file::{self, EXCERPT_BYTES};

pub(super) mod transcripts;

use transcripts::{EXCERPTS_SCHEMA, REMOVALS_SCHEMA, is_removal_waiting};

pub const DIRECTORY: &str = ".aria/history";
pub const DB_FILE: &str = "history.db";
const MAX_ERROR_MESSAGE_BYTES: usize = 256;
const MAX_STORED_ERROR_BYTES: usize = 2 * 1024;
pub const MAX_ENTRY_BYTES: usize = 128 * 1024;
const MAX_AUDIO_BYTES: u64 = 128 * 1024 * 1024;
/// A 16-bit PCM WAV's RIFF, format and data headers.
const WAV_HEADER_BYTES: u64 = 44;
/// Text and chats, plus every recording audio keeps.
const MAX_DATABASE_BYTES: u64 = 48 * 1024 * 1024 + MAX_AUDIO_BYTES;
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
/// The version this daemon writes. It refuses a newer database.
const SCHEMA_VERSION: u32 = 2;
const COLUMNS: &str = "id,created,source,app_name,bundle_id,duration,text,complete,status,error,expires,audio_present,deleting,language,own_voice,project,engine,section,title,transcript,reply,assistant,chat_name";
/// The ids of the entries a list returns, as `listed`. `?1` is the section,
/// or NULL for all; `?2` the lowercased search text; `?3` whether each chat
/// shows once; `?4` the exclusive cursor, or NULL.
///
/// A chat shows at its newest matching ask, picked before the cursor applies,
/// so a chat shown on one page shows no older ask on the next. An ask whose
/// chat this daemon can't read shows on its own, as the decoder reads it.
const LISTED: &str = "WITH matching AS (
        SELECT e.id, e.assistant, e.chat_name FROM entries e
        LEFT JOIN chats c ON c.assistant=e.assistant AND c.name=e.chat_name
        WHERE (?1 IS NULL OR e.section=?1)
        AND (?2='' OR instr(lowercase(e.text),?2)>0 OR instr(lowercase(e.reply),?2)>0
            OR instr(lowercase(c.quote),?2)>0 OR instr(lowercase(e.title),?2)>0
            OR instr(lowercase(e.chat_name),?2)>0 OR instr(lowercase(e.app_name),?2)>0)),
    newest AS (SELECT MAX(id) AS id FROM matching
        WHERE is_chat(assistant, chat_name) GROUP BY assistant, chat_name),
    listed AS (SELECT m.id FROM matching m
        WHERE (NOT ?3 OR NOT is_chat(m.assistant, m.chat_name)
            OR m.id IN (SELECT id FROM newest))
        AND (?4 IS NULL OR m.id<?4))";
/// What a chat's asks share, such as the text the chat is about. Each ask
/// references its chat, so deleting a chat deletes its asks and their results,
/// and renaming it renames theirs.
fn chats_schema() -> String {
    format!(
        "CREATE TABLE chats (
            assistant TEXT NOT NULL CHECK(length(CAST(assistant AS BLOB)) BETWEEN 1 AND 64),
            name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 128),
            quote TEXT CHECK(quote IS NULL OR length(CAST(quote AS BLOB)) BETWEEN 1 AND {}),
            PRIMARY KEY(assistant, name)) WITHOUT ROWID",
        Quote::MAX_BYTES
    )
}
/// Lets a load of one section skip the others' rows: Mac Audio rows can
/// number in the thousands, and every call loads the budgeted sections.
/// Created at each open, so a database an older Aria made gains it. An older
/// Aria keeps it up to date without reading it.
const SECTION_INDEX: &str = "CREATE INDEX IF NOT EXISTS entries_by_section ON entries(section)";
/// Finds a chat's asks, for reads and for its delete's cascade.
const CHAT_INDEX: &str = "CREATE INDEX entries_by_chat
     ON entries(assistant, chat_name) WHERE chat_name IS NOT NULL";
/// Columns a version 1 table may lack. Each holds the names the column had
/// there, newest first, then its name now, then the value a table without it
/// gets so its rows keep their meaning. A row from before sections is a
/// dictation.
const VERSION_ONE_LATER_COLUMNS: &[(&[&str], &str, &str)] = &[
    (&["section"], "section", "0"),
    (&["title"], "title", "NULL"),
    (&["transcript"], "transcript", "NULL"),
    (&["reply"], "reply", "NULL"),
    (&["resume", "conversation"], "resume", "NULL"),
    (&["assistant"], "assistant", "NULL"),
    (&["chat_name", "session_name"], "chat_name", "NULL"),
];
/// The columns every version 1 table has.
const VERSION_ONE_COLUMNS: &str = "id,created,source,app_name,bundle_id,duration,text,complete,status,error,expires,audio_present,deleting,language,own_voice,project,engine";
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

/// How much one section keeps before it evicts its own oldest entries.
struct Budget {
    entries: usize,
    text_bytes: usize,
}

enum Retention {
    /// Its entries expire, and past the budget it evicts its own oldest.
    Budget(Budget),
    /// Its entries stay until the person deletes them, and past `most` it
    /// refuses a new one. Each keeps at most an excerpt, so `most` also
    /// bounds the section's text.
    Kept { most: usize },
}

/// A Mac Audio transcript is the person's record, so only the person deletes
/// it.
fn retention(section: HistorySection) -> Retention {
    match section {
        HistorySection::Dictation => Retention::Budget(Budget {
            entries: 1_000,
            text_bytes: 8 * 1024 * 1024,
        }),
        HistorySection::SystemTranscription => Retention::Kept {
            most: MAX_TRANSCRIPTS,
        },
        HistorySection::Chat => Retention::Budget(Budget {
            entries: 500,
            text_bytes: 8 * 1024 * 1024,
        }),
    }
}

fn is_budgeted(section: HistorySection) -> bool {
    matches!(retention(section), Retention::Budget(_))
}

const MAX_TRANSCRIPTS: usize = 10_000;

/// Which rows a load reads.
#[derive(Clone, Copy)]
enum Rows {
    All,
    Section(HistorySection),
    /// The sections with a budget. It leaves out Mac Audio, whose rows can
    /// number in the thousands.
    Budgeted,
}

impl Rows {
    fn filter(self) -> String {
        match self {
            Self::All => String::new(),
            Self::Section(section) => format!("WHERE section={}", section_number(section)),
            Self::Budgeted => {
                let budgeted: Vec<String> = HistorySection::ALL
                    .into_iter()
                    .filter(|&section| is_budgeted(section))
                    .map(|section| section_number(section).to_string())
                    .collect();
                format!("WHERE section IN ({})", budgeted.join(","))
            }
        }
    }
}

pub struct Store {
    /// The file, so an error reading kept audio names where it lives.
    database: PathBuf,
    // The one lock also bounds audio copies. Copies need their own admission
    // if reads stall.
    state: Mutex<State>,
    /// Held by each operation that makes, ends or removes a transcript file,
    /// so no two act on one file together. Taken before `state`, never after.
    files: Mutex<()>,
}

struct State {
    connection: Database,
    pinned: HashSet<u64>,
    /// The transcript entries this store began whose session still records.
    /// Any other entry without a duration lost its session.
    recording: HashSet<u64>,
    warning: Option<String>,
}

pub struct RetryLease {
    pub entry: HistoryEntry,
    /// Decrypted into memory only, so no plaintext copy reaches the disk.
    pub recording: RecordingReader,
    store: Arc<Store>,
}

impl Drop for RetryLease {
    fn drop(&mut self) {
        let mut state = self.store.lock();
        state.pinned.remove(&self.entry.id);
        // Only the budgeted sections: the full sweep checks every Mac Audio
        // file, and the daemon may still hold the microphone here.
        if let Err(error) = self.store.maintain(&mut state) {
            tracing::warn!(error = %error_chain(&error), "dictation history cleanup failed after retry");
        }
    }
}

/// An ask that replied, as history saves it.
pub struct NewAsk<'a> {
    /// The request as asked, before any memory was added to it.
    pub request: &'a str,
    /// The text the ask's chat is about, saved with the chat unless it has one.
    pub quote: Option<&'a Quote>,
    pub reply: &'a str,
    pub workdir: &'a Path,
    /// The assistant session the ask continued, so a later ask in it can recall it.
    pub resume: Option<&'a str>,
    pub chat: Option<ChatKey>,
    pub created_at_ms: u64,
    pub duration_ms: u64,
    /// Why the reply couldn't be typed.
    pub failure: Option<RpcFailure>,
}

impl Store {
    pub fn open(directory: &Path, at_rest: &AtRest) -> Result<Arc<Self>, HistoryError> {
        let database = directory.join(DB_FILE);
        let connection =
            Database::open(&database, &DATABASE_POLICY, at_rest, &CancelToken::never())?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(HistoryError::Unavailable);
        }
        connection.execute_batch(
            "PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             PRAGMA journal_size_limit=0;
             PRAGMA secure_delete=ON;",
        )?;
        match version {
            0 => create_schema(&connection)?,
            1 => {
                if !is_current(&connection)? {
                    migrate_from_version_one(&connection)?;
                }
                migrate_to_version_two(&connection)?;
            }
            _ => {}
        }
        connection.execute_batch(SECTION_INDEX)?;
        connection.execute_batch(REMOVALS_SCHEMA)?;
        connection.execute_batch(EXCERPTS_SCHEMA)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        // Search folds case as Rust does, so non-ASCII text matches too.
        connection.create_scalar_function(
            "lowercase",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |context| {
                Ok(context
                    .get::<Option<String>>(0)?
                    .map(|text| text.to_lowercase()))
            },
        )?;
        // Lists group asks into a chat only where the decoder reads one.
        connection.create_scalar_function(
            "is_chat",
            2,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |context| {
                let (assistant, name) = (
                    context.get::<Option<String>>(0)?,
                    context.get::<Option<String>>(1)?,
                );
                Ok(assistant
                    .zip(name)
                    .is_some_and(|(assistant, name)| chat_key(&assistant, &name).is_some()))
            },
        )?;
        remove_plaintext_audio(directory)?;
        let store = Arc::new(Self {
            database,
            files: Mutex::new(()),
            state: Mutex::new(State {
                connection,
                pinned: HashSet::new(),
                recording: HashSet::new(),
                warning: None,
            }),
        });
        store.maintenance()?;
        Ok(store)
    }

    pub fn insert(
        &self,
        entry: HistoryEntry,
        audio: Option<&[f32]>,
    ) -> Result<HistoryEntry, HistoryError> {
        let mut state = self.lock();
        self.insert_into(&mut state, entry, audio, None, None)
    }

    /// `quote` goes on the entry's chat, unless the chat has one already.
    fn insert_into(
        &self,
        state: &mut State,
        mut entry: HistoryEntry,
        audio: Option<&[f32]>,
        resume: Option<&str>,
        quote: Option<&Quote>,
    ) -> Result<HistoryEntry, HistoryError> {
        validate_entry(&entry)?;
        self.maintain(state)?;
        let mut copied = match audio {
            None => None,
            Some(samples) => match state.warning.is_none().then(|| encoded_audio(samples)) {
                Some(Ok(wav)) => Some(wav),
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
        let joining = entry.content.chat().cloned();
        // Only a chat with no quote yet stores this one.
        let quote = match (&joining, quote) {
            (Some(chat), Some(quote)) if quote_of(state, chat)?.is_none() => Some(quote),
            _ => None,
        };
        let added = stored_bytes(&entry) + quote.map_or(0, |quote| quote.as_str().len());
        // Free the space before committing: a full database cannot evict anything.
        if let Err(error) = self.make_room(
            state,
            entry.content.section(),
            added,
            copied.as_ref().map_or(0, |wav| wav.len() as u64),
            true,
            joining.as_ref(),
        ) {
            if copied.is_none() {
                return Err(error);
            }
            copied = None;
            entry.warning = Some(AUDIO_WARNING.to_owned());
            self.make_room(
                state,
                entry.content.section(),
                added,
                0,
                true,
                joining.as_ref(),
            )?;
        }
        let transaction = state.connection.unchecked_transaction()?;
        let app_name = entry.application.as_ref().map(|app| app.name.as_str());
        let bundle_id = entry
            .application
            .as_ref()
            .and_then(|app| app.bundle_id.as_deref());
        let dictation = entry.dictation();
        let chat = entry.content.chat();
        if let Some(chat) = chat {
            transaction.execute(
                "INSERT INTO chats(assistant,name,quote) VALUES(?1,?2,?3)
                 ON CONFLICT(assistant,name) DO UPDATE SET quote=COALESCE(quote,excluded.quote)",
                params![
                    chat.assistant_id.name(),
                    chat.name.as_str(),
                    quote.map(Quote::as_str)
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO entries(created,source,app_name,bundle_id,duration,text,complete,status,error,language,own_voice,project,engine,section,title,transcript,reply,resume,assistant,chat_name)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            params![sql_integer(entry.created_at_ms)?, source_number(entry.source), app_name, bundle_id,
                entry.duration_ms.map(sql_integer).transpose()?, entry.text, entry.complete, status_number(entry.status),
                entry.error.as_ref().map(encode_error).transpose()?, dictation.map_or("", |d| d.language.as_str()),
                dictation.is_some_and(|d| d.own_voice_only),
                entry.project.as_ref().map(|path| path.as_os_str().as_bytes()),
                dictation.map_or("", |d| d.engine.as_str()),
                section_number(entry.content.section()), entry.title.as_ref().map(Title::as_str),
                transcript_path(&entry.content).map(|path| path.as_os_str().as_bytes()),
                reply(&entry.content), resume,
                chat.map(|chat| chat.assistant_id.name()),
                chat.map(|chat| chat.name.as_str())],
        )?;
        let id = unsigned(transaction.last_insert_rowid(), 0)?;
        transaction.commit()?;
        // Its own transaction: an entry whose audio fails is still saved.
        if let Some(wav) = copied
            && let Err(error) = save_audio(state, id, &wav)
        {
            {
                tracing::warn!(error = %error_chain(&error), "dictation history audio could not be saved");
                entry.warning = Some(AUDIO_WARNING.to_owned());
            }
        }
        let mut saved = load(state, id)?;
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
        let mut statement = state.connection.prepare(&format!(
            "{LISTED} SELECT {COLUMNS} FROM entries
             WHERE id IN (SELECT id FROM listed ORDER BY id DESC LIMIT ?5)
             ORDER BY id DESC"
        ))?;
        let rows = statement.query_map(
            params![
                query.section.map(section_number),
                needle,
                query.collapse_chats,
                query.before_id.map(sql_integer).transpose()?,
                sql_integer(query.limit as u64 + 1)?,
            ],
            |row| Ok((unsigned(row.get(0)?, 0)?, decode(row))),
        )?;
        let mut ids = Vec::new();
        let mut entries = Vec::new();
        for row in rows {
            let (id, decoded) = row?;
            ids.push(id);
            if ids.len() > query.limit {
                break;
            }
            // Maintenance removes it. The page goes on without it.
            match decoded {
                Ok(mut entry) => {
                    mark_retrying(&state, &mut entry);
                    entry.result_count = result_count(&state, id)?;
                    entries.push(entry);
                }
                Err(error) => tracing::warn!(id, %error, "history entry is unreadable"),
            }
        }
        let next_before_id = (ids.len() > query.limit).then(|| ids[query.limit - 1]);
        Ok(HistoryPage {
            entries,
            next_before_id,
            warning: state.warning.clone(),
        })
    }

    /// Whether Mac Audio holds `MAX_TRANSCRIPTS`, so a new session runs
    /// unsaved.
    pub(crate) fn is_history_full(&self) -> Result<bool, HistoryError> {
        let state = self.lock();
        let section = HistorySection::SystemTranscription;
        Ok(match retention(section) {
            Retention::Kept { most } => section_size(&state, section)? >= most,
            Retention::Budget(_) => false,
        })
    }

    pub fn save_ask(&self, ask: NewAsk<'_>) -> Result<HistoryEntry, HistoryError> {
        let NewAsk {
            request,
            quote,
            reply,
            workdir,
            resume,
            chat,
            created_at_ms,
            duration_ms,
            failure,
        } = ask;
        let entry = HistoryEntry {
            id: 0,
            title: None,
            content: HistoryContent::Ask {
                reply: reply.to_owned(),
                chat,
            },
            created_at_ms,
            duration_ms: Some(duration_ms),
            source: HistorySource::Local,
            application: None,
            project: Some(workdir.to_path_buf()),
            text: request.to_owned(),
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
        let mut state = self.lock();
        self.insert_into(&mut state, entry, None, resume, quote)
    }

    /// The newest asks saved under the assistant session `resume` whose
    /// requests and replies fit in `max_bytes` together, oldest first. An ask
    /// too big to fit on its own is skipped, so it doesn't cost the ones
    /// before it. The chat's quote doesn't count: `session_quote` reads it.
    pub fn recall(
        &self,
        resume: &str,
        max_bytes: usize,
    ) -> Result<Vec<HistoryEntry>, HistoryError> {
        let mut state = self.lock();
        self.maintain(&mut state)?;
        let mut statement = state.connection.prepare(&format!(
            "SELECT {COLUMNS} FROM entries WHERE resume=?1 AND deleting=0 ORDER BY id DESC"
        ))?;
        let mut rows = statement.query([resume])?;
        let mut asks = Vec::new();
        let mut used = 0;
        while let Some(row) = rows.next()? {
            let ask = decode(row)?;
            let bytes = ask.text.len() + reply(&ask.content).map_or(0, str::len);
            if bytes > max_bytes {
                continue;
            }
            used += bytes;
            if used > max_bytes {
                break;
            }
            asks.push(ask);
        }
        asks.reverse();
        Ok(asks)
    }

    /// The quote of the chat the assistant session `resume` ran in.
    pub fn session_quote(&self, resume: &str) -> Result<Option<Quote>, HistoryError> {
        self.first_quote(
            "SELECT chats.quote FROM entries JOIN chats
             ON chats.assistant=entries.assistant AND chats.name=entries.chat_name
             WHERE entries.resume=?1 AND entries.deleting=0 AND chats.quote IS NOT NULL
             LIMIT 1",
            params![resume],
        )
    }

    pub fn chat_quote(&self, chat: &ChatKey) -> Result<Option<Quote>, HistoryError> {
        let mut state = self.lock();
        self.maintain(&mut state)?;
        quote_of(&state, chat)
    }

    fn first_quote(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Option<Quote>, HistoryError> {
        let mut state = self.lock();
        self.maintain(&mut state)?;
        let quote: Option<String> = state
            .connection
            .query_row(sql, params, |row| row.get(0))
            .optional()?;
        Ok(quote.and_then(stored_quote))
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

    /// A transcript entry comes back with its transcript read, up to `MAX_READ_BYTES`, outside
    /// the lock.
    pub fn get(&self, id: u64) -> Result<HistoryEntry, HistoryError> {
        validate_id(id)?;
        let mut entry = {
            let mut state = self.lock();
            self.maintain(&mut state)?;
            load(&state, id)?
        };
        if let HistoryContent::SystemTranscription { transcript, read } = &mut entry.content {
            // A link or another user's file at the path reads as unavailable,
            // showing the excerpt.
            let read_ours = transcript_file::open(transcript)
                .ok()
                .and_then(|mut file| transcript_file::read_file(&mut file).ok());
            match read_ours.map(TranscriptRead::of) {
                Some((text, how_much)) => {
                    entry.text = text;
                    *read = Some(how_much);
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
        let _files = self.lease();
        {
            let mut state = self.lock();
            if state.pinned.contains(&id) || state.recording.contains(&id) {
                return Err(HistoryError::Busy);
            }
            let entry = match load(&state, id) {
                Ok(entry) => entry,
                Err(HistoryError::Missing) => return Ok(false),
                Err(error) => return Err(error),
            };
            // The file goes first, or is recorded to go: nothing sweeps the
            // transcripts folder, so a file whose row went first would never
            // be reclaimed.
            if let Some(transcript) = transcript_path(&entry.content) {
                self.remove_transcript(&mut state, transcript)?;
            }
            self.mark_delete(&mut state, id)?;
            self.remove_entry(&mut state, id)?;
        }
        Ok(true)
    }

    /// A chat's newest asks before the cursor, returned oldest first.
    /// A chat with no saved asks fails with `HistoryError::Missing`.
    pub fn chat(&self, query: &ChatQuery) -> Result<ChatPage, HistoryError> {
        if !(1..=ChatQuery::MAX_LIMIT).contains(&query.limit) {
            return Err(HistoryError::InvalidRequest);
        }
        if let Some(id) = query.before_id {
            validate_id(id)?;
        }
        let mut state = self.lock();
        self.maintain(&mut state)?;
        let asks = chat_ids(&state, &query.chat)?;
        if asks.is_empty() {
            return Err(HistoryError::Missing);
        }
        let mut page = asks
            .into_iter()
            .filter(|id| query.before_id.is_none_or(|before| *id < before))
            .take(query.limit + 1)
            .collect::<Vec<_>>();
        let next_before_id = (page.len() > query.limit).then(|| page[query.limit - 1]);
        page.truncate(query.limit);
        let entries = page
            .into_iter()
            .rev()
            .map(|id| load(&state, id))
            .collect::<Result<_, _>>()?;
        Ok(ChatPage {
            entries,
            next_before_id,
            quote: quote_of(&state, &query.chat)?,
        })
    }

    /// Gives a chat a new name, which its asks follow. Renaming a chat with
    /// no saved asks succeeds and changes nothing.
    pub fn rename_chat(&self, chat: &ChatKey, name: &ChatName) -> Result<(), HistoryError> {
        let mut state = self.lock();
        self.maintain(&mut state)?;
        let renamed = state.connection.execute(
            "UPDATE chats SET name=?3 WHERE assistant=?1 AND name=?2",
            params![chat.assistant_id.name(), chat.name.as_str(), name.as_str()],
        );
        match renamed {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY =>
            {
                Err(HistoryError::ChatNameTaken)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Deletes a chat, its asks, and their results, and answers whether
    /// history had the chat.
    pub fn delete_chat(&self, chat: &ChatKey) -> Result<bool, HistoryError> {
        let mut state = self.lock();
        let asks = chat_ids(&state, chat)?;
        if asks.iter().any(|id| state.pinned.contains(id)) {
            return Err(HistoryError::Busy);
        }
        // One statement cascades to the asks and their results, so a failure
        // leaves the chat whole. Audio goes afterward, as in `clear`.
        let deleted = state.connection.execute(
            "DELETE FROM chats WHERE assistant=?1 AND name=?2",
            params![chat.assistant_id.name(), chat.name.as_str()],
        )?;
        if self.remove_orphans(&mut state).is_err() {
            state.warning = Some(CLEANUP_WARNING.to_owned());
        }
        Ok(deleted > 0)
    }

    /// `None` clears every section.
    pub fn clear(
        &self,
        section: Option<HistorySection>,
    ) -> Result<ClearHistoryResult, HistoryError> {
        let _files = self.lease();
        let mut state = self.lock();
        let clears_transcripts =
            section.is_none_or(|section| section == HistorySection::SystemTranscription);
        if !state.pinned.is_empty() || (clears_transcripts && !state.recording.is_empty()) {
            return Err(HistoryError::Busy);
        }
        state.warning = None;
        let (entries, unreadable) = load_rows(&state, section.map_or(Rows::All, Rows::Section))?;
        let mut doomed = Vec::new();
        for entry in entries {
            // A transcript whose removal can't be recorded keeps its row, as in
            // Delete.
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
        let waiting = is_removal_waiting(&state)?;
        Ok(ClearHistoryResult {
            deleted_entries,
            cleanup_pending: state.warning.is_some() || waiting,
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
        let recording = match self.kept_audio(&state, id) {
            Ok(recording) => recording,
            Err(error @ HistoryError::Storage(_)) => return Err(error),
            Err(_) => {
                self.invalidate_audio(&mut state, id)?;
                return Err(HistoryError::AudioUnavailable);
            }
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
        if let Retention::Budget(budget) = retention(entry.content.section()) {
            self.make_room_for_text(
                &mut state,
                entry.content.section(),
                budget.text_bytes,
                text.len().saturating_sub(entry.text.len()),
                None,
            )?;
        }
        state.connection.execute(
            "UPDATE entries SET text=?1,complete=1,status=3,error=NULL,engine=?3,language=?4,
             expires=CASE WHEN ?5 THEN expires ELSE NULL END WHERE id=?2",
            params![text, sql_integer(id)?, engine, language, retain_audio],
        )?;
        // A cleared expiry keeps a failed removal pending for maintenance.
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

    /// The full sweep, Mac Audio rows included. Runs when the store opens and
    /// when the daemon is idle.
    pub fn maintenance(&self) -> Result<(), HistoryError> {
        let _files = self.lease();
        // Never stops history opening: what it can't finish waits for the next sweep.
        let unfinished = self.finish_pending().err();
        let gone = self.gone_transcripts()?;
        self.finish_interrupted()?;
        let mut state = self.lock();
        self.sweep(&mut state, Rows::All, &gone)?;
        if let Some(error) = unfinished {
            tracing::warn!(error = %error_chain(&error), "transcript removals could not be finished");
            state.warning = Some(CLEANUP_WARNING.to_owned());
        }
        Ok(())
    }

    /// The sweep every call runs first. It skips Mac Audio rows, which never
    /// expire or evict, and leaves their file checks to the full sweep.
    fn maintain(&self, state: &mut State) -> Result<(), HistoryError> {
        self.sweep(state, Rows::Budgeted, &HashMap::new())
    }

    /// `gone` holds the transcripts found missing before the lock was taken.
    /// An entry goes only if it still names that file and the file is still gone.
    fn sweep(
        &self,
        state: &mut State,
        rows: Rows,
        gone: &HashMap<u64, PathBuf>,
    ) -> Result<(), HistoryError> {
        state.warning = None;
        let now = now_ms();
        let (entries, unreadable) = load_rows(state, rows)?;
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
            let expires = is_budgeted(entry.content.section());
            // A row goes when its file is gone from a folder that is there. A
            // folder that is away, as on an unmounted volume, keeps its rows.
            if entry.deletion_pending
                || (expires && now.saturating_sub(entry.created_at_ms) >= TEXT_TTL_MS)
                || (!state.recording.contains(&entry.id)
                    && transcript_path(&entry.content).is_some_and(|transcript| {
                        gone.get(&entry.id).is_some_and(|seen| seen == transcript)
                            && transcript_file::is_gone(transcript)
                    }))
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
                && audio_size(state, entry.id)?.is_none()
            {
                self.invalidate_audio(state, entry.id)?;
            }
        }
        for section in HistorySection::ALL {
            if !is_budgeted(section) {
                continue;
            }
            if self.make_room(state, section, 0, 0, false, None).is_err() {
                state.warning = Some(CLEANUP_WARNING.to_owned());
            }
        }
        // After every eviction, so no call starts on a chat without asks.
        self.remove_orphans(state)
    }

    fn mark_delete(&self, state: &mut State, id: u64) -> Result<(), HistoryError> {
        state.connection.execute(
            "UPDATE entries SET deleting=1 WHERE id=?1",
            [sql_integer(id)?],
        )?;
        Ok(())
    }

    /// Its audio goes with it, through the foreign key.
    fn remove_entry(&self, state: &mut State, id: u64) -> Result<(), HistoryError> {
        state
            .connection
            .execute("DELETE FROM entries WHERE id=?1", [sql_integer(id)?])?;
        Ok(())
    }

    fn invalidate_audio(&self, state: &mut State, id: u64) -> Result<(), HistoryError> {
        // Commit the loss of availability first. The removal can fail on its own.
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
        let transaction = state.connection.unchecked_transaction()?;
        transaction.execute("DELETE FROM audio WHERE entry=?1", [sql_integer(id)?])?;
        transaction.execute(
            "UPDATE entries SET audio_present=0 WHERE id=?1",
            [sql_integer(id)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Removes every chat with no asks left.
    fn remove_orphans(&self, state: &mut State) -> Result<(), HistoryError> {
        state.connection.execute(
            "DELETE FROM chats WHERE NOT EXISTS (SELECT 1 FROM entries
             WHERE entries.assistant=chats.assistant
             AND entries.chat_name=chats.name)",
            [],
        )?;
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
        joining: Option<&ChatKey>,
    ) -> Result<(), HistoryError> {
        match retention(section) {
            Retention::Budget(budget) => {
                self.make_room_for_text(state, section, budget.text_bytes, text_bytes, joining)?;
                let (mut entries, _) = load_rows(state, Rows::Section(section))?;
                // An insert reserves a slot even when its text is empty.
                let limit = if reserve_entry {
                    budget.entries - 1
                } else {
                    budget.entries
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
            }
            Retention::Kept { most } if reserve_entry && section_size(state, section)? >= most => {
                return Err(HistoryError::Full);
            }
            Retention::Kept { .. } => {}
        }
        if reserve_entry && audio_bytes == 0 {
            return Ok(());
        }
        // Mac Audio entries keep no audio.
        let (entries, _) = load_rows(state, Rows::Budgeted)?;
        let mut retained_audio = 0;
        for entry in &entries {
            retained_audio += audio_size(state, entry.id)?.unwrap_or(0);
        }
        for entry in entries.iter().rev() {
            if retained_audio.saturating_add(audio_bytes) <= MAX_AUDIO_BYTES {
                break;
            }
            if state.pinned.contains(&entry.id) {
                continue;
            }
            if let Some(size) = audio_size(state, entry.id)? {
                self.remove_audio(state, entry.id)?;
                retained_audio = retained_audio.saturating_sub(size);
            }
        }
        if retained_audio.saturating_add(audio_bytes) > MAX_AUDIO_BYTES {
            return Err(HistoryError::Busy);
        }
        Ok(())
    }

    /// `joining` is the chat an insert adds to, which keeps its quote even
    /// when its last ask goes.
    fn make_room_for_text(
        &self,
        state: &mut State,
        section: HistorySection,
        limit: usize,
        added: usize,
        joining: Option<&ChatKey>,
    ) -> Result<(), HistoryError> {
        let (entries, _) = load_rows(state, Rows::Section(section))?;
        let quotes = chat_quotes(state)?;
        let mut asks_left: HashMap<&ChatKey, usize> = HashMap::new();
        for chat in entries.iter().filter_map(|entry| entry.content.chat()) {
            *asks_left.entry(chat).or_default() += 1;
        }
        let quoted_bytes =
            |chat: &ChatKey| quotes.get(chat).map_or(0, |quote| quote.as_str().len());
        let mut bytes = entries.iter().map(stored_bytes).sum::<usize>()
            + asks_left
                .keys()
                .map(|chat| quoted_bytes(chat))
                .sum::<usize>();
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
            // A chat's last ask takes the chat, and its quote, with it.
            if let Some(chat) = entry.content.chat()
                && let Some(left) = asks_left.get_mut(chat)
            {
                *left -= 1;
                if *left == 0 && joining != Some(chat) {
                    bytes -= quoted_bytes(chat);
                }
            }
        }
        if bytes.saturating_add(added) > limit {
            return Err(HistoryError::TooLarge);
        }
        Ok(())
    }

    fn kept_audio(&self, state: &State, id: u64) -> Result<RecordingReader, HistoryError> {
        let blob = state.connection.blob_open(
            rusqlite::MAIN_DB,
            c"audio",
            c"wav",
            sql_integer(id)?,
            true,
        )?;
        RecordingReader::from_wav(blob, &self.database).map_err(|error| {
            match error {
                CaptureError::Wav(error) => {
                    tracing::warn!(error = %error_chain(&error), "kept audio is unreadable and is dropped");
                    HistoryError::AudioUnavailable
                }
                error => HistoryError::storage(error),
            }
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lease(&self) -> MutexGuard<'_, ()> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The WAV an insert saves, refused past the audio budget. Encoded in
/// memory, so the recording never reaches a file.
fn encoded_audio(samples: &[f32]) -> Result<Vec<u8>, HistoryError> {
    if encoded_bytes(samples.len()) > MAX_AUDIO_BYTES {
        return Err(HistoryError::AudioUnavailable);
    }
    encode_wav(samples).map_err(|_| HistoryError::AudioUnavailable)
}

/// The size of the WAV `encoded_audio` makes from `samples` samples.
fn encoded_bytes(samples: usize) -> u64 {
    WAV_HEADER_BYTES + 2 * samples as u64
}

fn save_audio(state: &State, id: u64, wav: &[u8]) -> Result<(), HistoryError> {
    let transaction = state.connection.unchecked_transaction()?;
    transaction.execute(
        "INSERT INTO audio(entry,wav) VALUES(?1,?2)",
        params![sql_integer(id)?, wav],
    )?;
    let expires = now_ms().saturating_add(AUDIO_TTL_MS);
    transaction.execute(
        "UPDATE entries SET expires=?1,audio_present=1 WHERE id=?2",
        params![sql_integer(expires)?, sql_integer(id)?],
    )?;
    transaction.commit()?;
    Ok(())
}

fn audio_size(state: &State, id: u64) -> Result<Option<u64>, HistoryError> {
    let size: Option<i64> = state
        .connection
        .query_row(
            "SELECT length(wav) FROM audio WHERE entry=?1",
            [sql_integer(id)?],
            |row| row.get(0),
        )
        .optional()?;
    size.map(|size| unsigned(size, 0).map_err(HistoryError::from))
        .transpose()
}

/// Recordings an older daemon kept as files, in plaintext. Version 2 keeps
/// none, so whatever is left is removed.
fn remove_plaintext_audio(directory: &Path) -> Result<(), HistoryError> {
    match fs::remove_dir_all(directory.join("audio")) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HistoryError::storage(error)),
    }
}

fn transcript_path(content: &HistoryContent) -> Option<&Path> {
    match content {
        HistoryContent::SystemTranscription { transcript, .. } => Some(transcript),
        HistoryContent::Dictation(_) | HistoryContent::Ask { .. } => None,
    }
}

fn reply(content: &HistoryContent) -> Option<&str> {
    match content {
        HistoryContent::Ask { reply, .. } => Some(reply),
        HistoryContent::Dictation(_) | HistoryContent::SystemTranscription { .. } => None,
    }
}

/// What an entry counts against its section's text budget. Its chat's quote
/// counts once per chat, apart.
fn stored_bytes(entry: &HistoryEntry) -> usize {
    entry.text.len() + reply(&entry.content).map_or(0, str::len)
}

/// `table` is `entries`, or the table a migration copies into before
/// renaming it to `entries`.
fn entries_schema(table: &str) -> String {
    format!(
        "CREATE TABLE {table} (
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
            project BLOB, engine TEXT NOT NULL,
            section INTEGER NOT NULL CHECK(section IN (0,1,2)),
            title TEXT CHECK(title IS NULL OR length(CAST(title AS BLOB)) BETWEEN 1 AND 256),
            transcript BLOB CHECK(transcript IS NULL OR length(transcript)<=4096)
                CHECK((section=1)=(transcript IS NOT NULL)),
            reply TEXT CHECK(reply IS NULL OR length(CAST(reply AS BLOB))<={MAX_ENTRY_BYTES})
                CHECK((section=2)=(reply IS NOT NULL)),
            -- Which assistant session an ask continued, so a later ask in it
            -- can recall it. Clients never see it.
            resume TEXT CHECK(resume IS NULL
                OR (section=2 AND length(CAST(resume AS BLOB)) BETWEEN 1 AND 128)),
            assistant TEXT,
            chat_name TEXT,
            CHECK((chat_name IS NULL)=(assistant IS NULL)),
            CHECK(chat_name IS NULL OR section=2),
            FOREIGN KEY(assistant, chat_name) REFERENCES chats(assistant, name)
                ON DELETE CASCADE ON UPDATE CASCADE)"
    )
}

/// A saved recording, whole, as a 16 kHz mono 16-bit WAV.
fn audio_schema() -> String {
    format!(
        "CREATE TABLE audio (
            entry INTEGER PRIMARY KEY REFERENCES entries(id) ON DELETE CASCADE,
            wav BLOB NOT NULL CHECK(length(wav)<={MAX_AUDIO_BYTES}))"
    )
}

fn create_schema(connection: &Database) -> Result<(), HistoryError> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(&chats_schema())?;
    transaction.execute_batch(&entries_schema("entries"))?;
    transaction.execute_batch(CHAT_INDEX)?;
    transaction.execute_batch(RESULTS_SCHEMA)?;
    transaction.execute_batch(&audio_schema())?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

/// Whether a version 1 database has every table and column this daemon
/// reads. Version 1 tables gained columns over time without a version bump.
fn is_current(connection: &Database) -> Result<bool, HistoryError> {
    let columns = |table: &str| -> Result<HashSet<String>, HistoryError> {
        Ok(connection
            .prepare("SELECT name FROM pragma_table_info(?1)")?
            .query_map([table], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let entries = columns("entries")?;
    Ok(COLUMNS.split(',').all(|column| entries.contains(column))
        && columns("chats")?.contains("quote"))
}

/// Rebuilds an older version 1 database with SQLite's steps for changing a
/// table's schema. Its entries may lack later columns, and its chats table,
/// if any, its quote.
fn migrate_from_version_one(connection: &Database) -> Result<(), HistoryError> {
    // Set outside the transaction, because inside one the pragma has no
    // effect. Left on, dropping the old table would delete every result
    // through its cascade.
    connection.pragma_update(None, "foreign_keys", false)?;
    let transaction = connection.unchecked_transaction()?;
    let present = transaction
        .prepare("SELECT name FROM pragma_table_info('entries')")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    let (names, values): (Vec<&str>, Vec<String>) = VERSION_ONE_LATER_COLUMNS
        .iter()
        .map(|&(before, now, missing)| {
            let kept: Vec<&str> = before
                .iter()
                .copied()
                .filter(|name| present.contains(*name))
                .collect();
            let value = match kept.as_slice() {
                [] => missing.to_owned(),
                [name] => (*name).to_owned(),
                names => format!("COALESCE({})", names.join(",")),
            };
            (now, value)
        })
        .unzip();
    let sequence: Option<i64> = transaction
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='entries'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    transaction.execute_batch("DROP TABLE IF EXISTS chats")?;
    transaction.execute_batch(&chats_schema())?;
    transaction.execute_batch(&entries_schema("new_entries"))?;
    transaction.execute_batch(&format!(
        "INSERT INTO new_entries({VERSION_ONE_COLUMNS},{})
         SELECT {VERSION_ONE_COLUMNS},{} FROM entries;
         INSERT INTO chats(assistant,name)
         SELECT DISTINCT assistant,chat_name FROM new_entries WHERE chat_name IS NOT NULL;
         DROP TABLE entries;
         ALTER TABLE new_entries RENAME TO entries;",
        names.join(","),
        values.join(","),
    ))?;
    // The rebuilt table counts only the IDs it copied. The old count keeps a
    // deleted entry's ID from coming back.
    if let Some(sequence) = sequence {
        transaction.execute("DELETE FROM sqlite_sequence WHERE name='entries'", [])?;
        transaction.execute(
            "INSERT INTO sqlite_sequence(name,seq) VALUES('entries',?1)",
            [sequence],
        )?;
    }
    transaction.execute_batch(CHAT_INDEX)?;
    transaction.execute_batch(RESULTS_SCHEMA)?;
    // Results whose entry a daemon deleted without them, with foreign keys
    // off. The check below would refuse them.
    transaction.execute(
        "DELETE FROM results WHERE entry NOT IN (SELECT id FROM entries)",
        [],
    )?;
    if transaction
        .prepare("PRAGMA foreign_key_check")?
        .query([])?
        .next()?
        .is_some()
    {
        return Err(HistoryError::storage(Dangling));
    }
    transaction.commit()?;
    Ok(())
}

/// Version 2 keeps audio in the database. The files version 1 kept are
/// not carried over, so no entry has audio any more.
fn migrate_to_version_two(connection: &Database) -> Result<(), HistoryError> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(&audio_schema())?;
    transaction.execute("UPDATE entries SET audio_present=0,expires=NULL", [])?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("a history row references one that is missing")]
struct Dangling;

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
        RpcErrorCode::TranscriptionFailed => 105,
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
        RpcErrorCode::ChatNameTaken => 308,
        RpcErrorCode::SpeakerModelMissing => 500,
        RpcErrorCode::SpeakerAlreadyEnrolled => 501,
        RpcErrorCode::SpeakerNotEnrolled => 502,
        RpcErrorCode::SpeakerInconsistentSamples => 503,
        RpcErrorCode::SpeakerProfileWriteFailed => 504,
        RpcErrorCode::SpeakerNoSpeech => 505,
        RpcErrorCode::SystemTranscriptionUnsupportedOs => 800,
        RpcErrorCode::SystemTranscriptionPermissionDenied => 801,
        RpcErrorCode::SystemTranscriptionCaptureFailed => 802,
        RpcErrorCode::UnsavedTranscriptMissing => 803,
        RpcErrorCode::HistoryMissing => 900,
        RpcErrorCode::HistoryAudioUnavailable => 901,
        RpcErrorCode::HistoryUnavailable => 902,
        RpcErrorCode::HistoryProjectUnavailable => 903,
        RpcErrorCode::HistoryEntryEmpty => 904,
        RpcErrorCode::HistoryTranscriptUnreadable => 905,
        RpcErrorCode::HistoryFull => 906,
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
        105 => RpcErrorCode::TranscriptionFailed,
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
        308 => RpcErrorCode::ChatNameTaken,
        500 => RpcErrorCode::SpeakerModelMissing,
        501 => RpcErrorCode::SpeakerAlreadyEnrolled,
        502 => RpcErrorCode::SpeakerNotEnrolled,
        503 => RpcErrorCode::SpeakerInconsistentSamples,
        504 => RpcErrorCode::SpeakerProfileWriteFailed,
        505 => RpcErrorCode::SpeakerNoSpeech,
        800 => RpcErrorCode::SystemTranscriptionUnsupportedOs,
        801 => RpcErrorCode::SystemTranscriptionPermissionDenied,
        802 => RpcErrorCode::SystemTranscriptionCaptureFailed,
        803 => RpcErrorCode::UnsavedTranscriptMissing,
        900 => RpcErrorCode::HistoryMissing,
        901 => RpcErrorCode::HistoryAudioUnavailable,
        902 => RpcErrorCode::HistoryUnavailable,
        903 => RpcErrorCode::HistoryProjectUnavailable,
        904 => RpcErrorCode::HistoryEntryEmpty,
        905 => RpcErrorCode::HistoryTranscriptUnreadable,
        906 => RpcErrorCode::HistoryFull,
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

/// The chat an ask's columns name, when this daemon knows its assistant and
/// accepts its name.
fn chat_key(assistant: &str, name: &str) -> Option<ChatKey> {
    Some(ChatKey {
        assistant_id: assistant.parse().ok()?,
        name: ChatName::new(name).ok()?,
    })
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
    // A newer daemon can name an assistant or a chat name this one rejects.
    // Its ask stays readable here without the chat, and the row
    // keeps both columns for the daemon that wrote them.
    let chat = match (
        row.get::<_, Option<String>>(21)?,
        row.get::<_, Option<String>>(22)?,
    ) {
        (None, None) => None,
        (Some(assistant), Some(name)) => Some(chat_key(&assistant, &name)),
        (Some(_), None) | (None, Some(_)) => return Err(rusqlite::Error::InvalidQuery),
    };
    let content = match (
        row.get::<_, u8>(17)?,
        transcript,
        row.get::<_, Option<String>>(20)?,
        chat,
    ) {
        (0, None, None, None) => HistoryContent::Dictation(DictationContent {
            language: row.get(13)?,
            engine: row.get(16)?,
            own_voice_only: row.get(14)?,
            audio,
            retrying: false,
        }),
        (1, Some(transcript), None, None) => HistoryContent::SystemTranscription {
            transcript,
            read: None,
        },
        (2, None, Some(reply), chat) => {
            if chat.as_ref().is_some_and(Option::is_none) {
                tracing::warn!(
                    id = row.get::<_, i64>(0)?,
                    "a history ask's chat is unreadable; reading it without one"
                );
            }
            HistoryContent::Ask {
                reply,
                chat: chat.flatten(),
            }
        }
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

fn chat_ids(state: &State, chat: &ChatKey) -> Result<Vec<u64>, HistoryError> {
    let mut statement = state
        .connection
        .prepare("SELECT id FROM entries WHERE assistant=?1 AND chat_name=?2 ORDER BY id DESC")?;
    let ids = statement
        .query_map(
            params![chat.assistant_id.name(), chat.name.as_str()],
            |row| unsigned(row.get(0)?, 0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
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

fn result_count(state: &State, id: u64) -> Result<u32, HistoryError> {
    Ok(state.connection.query_row(
        "SELECT COUNT(*) FROM results WHERE entry=?1",
        [sql_integer(id)?],
        |row| row.get(0),
    )?)
}

fn section_size(state: &State, section: HistorySection) -> Result<usize, HistoryError> {
    let size: i64 = state.connection.query_row(
        "SELECT COUNT(*) FROM entries WHERE section=?1",
        [section_number(section)],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(size).unwrap_or(usize::MAX))
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
    entry.result_count = result_count(state, id)?;
    if entry.status == HistoryStatus::Recovered {
        entry.warning = state.warning.clone();
    }
    Ok(entry)
}

/// Each chat's quote, for chats that have a readable one.
fn chat_quotes(state: &State) -> Result<HashMap<ChatKey, Quote>, HistoryError> {
    let mut statement = state
        .connection
        .prepare("SELECT assistant,name,quote FROM chats WHERE quote IS NOT NULL")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut quotes = HashMap::new();
    for row in rows {
        let (assistant, name, quote) = row?;
        // A chat a newer daemon named, as `decode` reads its asks.
        let (Ok(assistant_id), Ok(name)) = (assistant.parse(), ChatName::new(&name)) else {
            continue;
        };
        if let Some(quote) = stored_quote(quote) {
            quotes.insert(ChatKey { assistant_id, name }, quote);
        }
    }
    Ok(quotes)
}

fn quote_of(state: &State, chat: &ChatKey) -> Result<Option<Quote>, HistoryError> {
    let quote: Option<String> = state
        .connection
        .query_row(
            "SELECT quote FROM chats WHERE assistant=?1 AND name=?2 AND quote IS NOT NULL",
            params![chat.assistant_id.name(), chat.name.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(quote.and_then(stored_quote))
}

/// A quote this daemon refuses, such as a blank one a newer daemon wrote,
/// reads as none, so its chat stays readable.
fn stored_quote(quote: String) -> Option<Quote> {
    Quote::new(quote)
        .inspect_err(|_| {
            tracing::warn!("a history chat's quote is unreadable; reading it without one");
        })
        .ok()
}

// Rows that fail to decode come back as ids, so one damaged row cannot hide
// the rest of the history.
fn load_rows(state: &State, rows: Rows) -> Result<(Vec<HistoryEntry>, Vec<u64>), HistoryError> {
    let mut statement = state.connection.prepare(&format!(
        "SELECT {COLUMNS} FROM entries {} ORDER BY id DESC",
        rows.filter()
    ))?;
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
    use aria_kit::primitives::private_fs;
    use rusqlite::Connection;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use super::*;

    /// A connection past the store, keyed as the store keys its own.
    fn keyed(path: PathBuf) -> Connection {
        let connection = Connection::open(path).unwrap();
        AtRest::for_tests().unlock_for_tests(&connection);
        connection
    }

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

    /// One second of a recording.
    fn audio() -> &'static [f32] {
        &[0.25; 16_000]
    }

    fn query(text: &str, before_id: Option<u64>, limit: usize) -> HistoryQuery {
        HistoryQuery {
            section: None,
            query: text.into(),
            before_id,
            limit,
            collapse_chats: false,
        }
    }

    fn collapsed(text: &str, before_id: Option<u64>, limit: usize) -> HistoryQuery {
        HistoryQuery {
            collapse_chats: true,
            ..query(text, before_id, limit)
        }
    }

    fn questions(entries: &[HistoryEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.text.as_str()).collect()
    }

    pub(super) fn save(
        store: &Store,
        text: &str,
        status: HistoryStatus,
        audio: Option<&[f32]>,
    ) -> HistoryEntry {
        store.insert(entry(text, status), audio).unwrap()
    }

    /// Edits the database behind the store's back, as damage or an older
    /// daemon would.
    pub(super) fn tamper(store: &Store, sql: &str, params: impl rusqlite::Params) {
        store.lock().connection.execute(sql, params).unwrap();
    }

    fn has_audio(store: &Store, id: u64) -> bool {
        audio_size(&store.lock(), id).unwrap().is_some()
    }

    pub(super) fn listed(store: &Store) -> Vec<HistoryEntry> {
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
    /// The columns a version 1 table gained after sections, through
    /// `conversation`, which held what `resume` holds.
    const ADDED_BEFORE_RESUME: &str = "
        ALTER TABLE entries ADD COLUMN section INTEGER NOT NULL DEFAULT 0 CHECK(section IN (0,1,2));
        ALTER TABLE entries ADD COLUMN title TEXT
            CHECK(title IS NULL OR length(CAST(title AS BLOB)) BETWEEN 1 AND 256);
        ALTER TABLE entries ADD COLUMN transcript BLOB
            CHECK(transcript IS NULL OR length(transcript)<=4096)
            CHECK((section=1)=(transcript IS NOT NULL));
        ALTER TABLE entries ADD COLUMN reply TEXT
            CHECK(reply IS NULL OR length(CAST(reply AS BLOB))<=131072)
            CHECK((section=2)=(reply IS NOT NULL));
        ALTER TABLE entries ADD COLUMN conversation TEXT CHECK(conversation IS NULL
            OR (section=2 AND length(CAST(conversation AS BLOB)) BETWEEN 1 AND 128));";
    /// The columns it gained after that, in the order a database that saw
    /// every daemon added them.
    const ADDED_AFTER_CONVERSATION: &str = "
        ALTER TABLE entries ADD COLUMN resume TEXT CHECK(resume IS NULL
            OR (section=2 AND length(CAST(resume AS BLOB)) BETWEEN 1 AND 128));
        ALTER TABLE entries ADD COLUMN assistant TEXT CHECK(assistant IS NULL
            OR length(CAST(assistant AS BLOB)) BETWEEN 1 AND 64);
        ALTER TABLE entries ADD COLUMN session_name TEXT CHECK(session_name IS NULL
            OR length(CAST(session_name AS BLOB)) BETWEEN 1 AND 128)
            CHECK((session_name IS NULL)=(assistant IS NULL))
            CHECK(session_name IS NULL OR section=2);
        CREATE INDEX entries_by_conversation
            ON entries(assistant, session_name) WHERE session_name IS NOT NULL;";

    /// Replaces the store's database with a version 1 one built by `build`.
    fn version_one(directory: &Path, build: &str) {
        drop(Store::open(directory, &AtRest::for_tests()).unwrap());
        let database = keyed(directory.join("history.db"));
        database
            .execute_batch(&format!(
                "DROP TABLE audio; DROP TABLE results; DROP TABLE entries; DROP TABLE chats;
                 {build}; PRAGMA user_version=1;"
            ))
            .unwrap();
    }

    fn user_version(store: &Store) -> u32 {
        store
            .lock()
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    /// A transcript entry as the store keeps one, without reading the file.
    pub(super) fn system_transcription(text: &str, transcript: &Path) -> HistoryEntry {
        HistoryEntry {
            content: HistoryContent::SystemTranscription {
                transcript: transcript.to_path_buf(),
                read: None,
            },
            ..entry(text, HistoryStatus::Success)
        }
    }
    fn chat(session: &str) -> ChatKey {
        ChatKey {
            assistant_id: aria_settings::config::AssistantId::ClaudeCode,
            name: ChatName::new(session).unwrap(),
        }
    }

    fn new_ask(request: &str) -> NewAsk<'_> {
        NewAsk {
            request,
            quote: None,
            reply: "reply",
            workdir: Path::new("/project"),
            resume: None,
            chat: None,
            created_at_ms: now_ms(),
            duration_ms: 1,
            failure: None,
        }
    }

    fn ask(store: &Store, question: &str, chat: Option<ChatKey>) -> HistoryEntry {
        store
            .save_ask(NewAsk {
                chat,
                ..new_ask(question)
            })
            .unwrap()
    }

    fn budget(section: HistorySection) -> Budget {
        match retention(section) {
            Retention::Budget(budget) => budget,
            Retention::Kept { .. } => panic!("{section:?} keeps every entry"),
        }
    }

    /// A store in a fresh folder, which lives as long as the guard.
    pub(super) fn opened() -> (tempfile::TempDir, Arc<Store>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();
        (directory, store)
    }

    #[test]
    fn a_database_from_before_sections_keeps_its_rows_as_dictations_in_the_current_shape() {
        let directory = tempfile::tempdir().unwrap();
        version_one(directory.path(), SCHEMA_BEFORE_SECTIONS);
        let database = keyed(directory.path().join("history.db"));
        database
            .execute(INSERT_BEFORE_SECTIONS, params![now_ms() as i64, "old row"])
            .unwrap();
        drop(database);

        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();

        let entries = listed(&store);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "old row");
        assert!(matches!(entries[0].content, HistoryContent::Dictation(_)));
        assert_eq!(user_version(&store), SCHEMA_VERSION);
        drop(store);
        Store::open(directory.path(), &AtRest::for_tests()).unwrap();
    }

    #[test]
    fn a_version_one_database_from_before_resume_keeps_its_assistant_sessions() {
        let directory = tempfile::tempdir().unwrap();
        version_one(
            directory.path(),
            &format!("{SCHEMA_BEFORE_SECTIONS}; {ADDED_BEFORE_RESUME}; {RESULTS_SCHEMA}"),
        );
        let database = keyed(directory.path().join("history.db"));
        database
            .execute(
                "INSERT INTO entries(created,source,text,complete,status,language,own_voice,engine,section,reply,conversation)
                 VALUES(?1,0,'When?',1,0,'',0,'',2,'reply','s1')",
                params![now_ms() as i64],
            )
            .unwrap();
        drop(database);

        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();

        assert_eq!(
            questions(&store.recall("s1", 1024).unwrap()),
            ["When?"],
            "the session its ask continued"
        );
    }

    #[test]
    fn a_version_one_database_migrates_with_its_chats_results_and_ids() {
        let directory = tempfile::tempdir().unwrap();
        version_one(
            directory.path(),
            &format!(
                "{SCHEMA_BEFORE_SECTIONS}; {ADDED_BEFORE_RESUME}; {ADDED_AFTER_CONVERSATION}; \
                 {RESULTS_SCHEMA}"
            ),
        );
        let database = keyed(directory.path().join("history.db"));
        // Foreign keys off, as a daemon from before results ran, so the
        // delete below leaves the result behind.
        database.pragma_update(None, "foreign_keys", false).unwrap();
        let first = chat("first");
        database
            .execute(INSERT_BEFORE_SECTIONS, params![now_ms() as i64, "dictated"])
            .unwrap();
        let insert_ask = "INSERT INTO entries(created,source,text,complete,status,language,own_voice,engine,section,reply,conversation,resume,assistant,session_name)
             VALUES(?1,0,?2,1,0,'',0,'',2,'reply',?3,?3,?4,?5)";
        for (question, resume, key) in [
            ("When?", Some("s1"), Some(&first)),
            ("And then?", None, Some(&first)),
            ("Where?", None, None),
            ("Gone?", None, None),
        ] {
            database
                .execute(
                    insert_ask,
                    params![
                        now_ms() as i64,
                        question,
                        resume,
                        key.map(|key| key.assistant_id.name()),
                        key.map(|key| key.name.as_str())
                    ],
                )
                .unwrap();
        }
        database
            .execute_batch(
                "INSERT INTO results(entry,created,prompt_name,text) VALUES(2,0,'Polish','Kept.');
                 INSERT INTO results(entry,created,prompt_name,text) VALUES(5,0,'Polish','Orphan.');
                 DELETE FROM entries WHERE id=5;",
            )
            .unwrap();
        drop(database);

        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();

        assert_eq!(user_version(&store), SCHEMA_VERSION);
        let entries = listed(&store)
            .into_iter()
            .map(|entry| (entry.text, entry.content.chat().cloned()))
            .collect::<Vec<_>>();
        let continued = Some(first.clone());
        assert_eq!(
            entries,
            [
                ("Where?".to_owned(), None),
                ("And then?".to_owned(), continued.clone()),
                ("When?".to_owned(), continued),
                ("dictated".to_owned(), None),
            ]
        );
        assert_eq!(store.results(2).unwrap().len(), 1);
        assert_eq!(
            store
                .recall("s1", 1024)
                .unwrap()
                .into_iter()
                .map(|ask| ask.text)
                .collect::<Vec<_>>(),
            ["When?"]
        );
        let state = store.lock();
        let columns = state
            .connection
            .prepare("SELECT name FROM pragma_table_info('entries')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<HashSet<_>>>()
            .unwrap();
        assert!(columns.contains("chat_name"));
        assert!(!columns.contains("session_name"));
        let results: i64 = state
            .connection
            .query_row("SELECT COUNT(*) FROM results", [], |row| row.get(0))
            .unwrap();
        assert_eq!(results, 1, "the orphaned result is gone");
        let foreign_keys: bool = state
            .connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert!(foreign_keys);
        drop(state);
        assert_eq!(ask(&store, "Next?", None).id, 6, "a deleted ID stays used");
        store.delete_chat(&first).unwrap();
        assert_eq!(questions(&listed(&store)), ["Next?", "Where?", "dictated"]);
        assert!(matches!(store.results(2), Err(HistoryError::Missing)));
    }

    #[test]
    fn a_database_at_the_current_version_reopens_and_a_newer_one_is_refused() {
        let (directory, store) = opened();
        assert_eq!(user_version(&store), SCHEMA_VERSION);
        drop(store);
        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();
        tamper(&store, "PRAGMA user_version=3", []);
        drop(store);

        assert!(matches!(
            Store::open(directory.path(), &AtRest::for_tests()),
            Err(HistoryError::Unavailable)
        ));
    }

    fn quoted_ask(store: &Store, question: &str, chat: ChatKey, quote: &Quote) -> HistoryEntry {
        store
            .save_ask(NewAsk {
                quote: Some(quote),
                chat: Some(chat),
                ..new_ask(question)
            })
            .unwrap()
    }

    #[test]
    fn a_database_whose_chats_lack_quotes_is_rebuilt_and_keeps_its_rows() {
        let (directory, store) = opened();
        let earlier = ask(&store, "When?", Some(chat("plans")));
        tamper(&store, "ALTER TABLE chats DROP COLUMN quote", []);
        tamper(&store, "ALTER TABLE entries ADD COLUMN quote TEXT", []);
        tamper(&store, "DROP TABLE audio", []);
        tamper(&store, "PRAGMA user_version=1", []);
        drop(store);

        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();
        let quote = Quote::new("the notes".to_owned()).unwrap();
        quoted_ask(&store, "Shorter?", chat("notes"), &quote);

        assert_eq!(user_version(&store), SCHEMA_VERSION);
        assert_eq!(store.get(earlier.id).unwrap().content, earlier.content);
        assert_eq!(store.chat_quote(&chat("notes")).unwrap(), Some(quote));
    }

    #[test]
    fn a_chat_keeps_its_first_quote_and_its_sessions_recall_it_apart_from_the_budget() {
        let (_directory, store) = opened();
        let long = Quote::new("a".repeat(40 * 1024)).unwrap();
        let later = Quote::new("later notes".to_owned()).unwrap();
        for (question, quote, resume) in [("Shorter?", &long, "s1"), ("Again?", &later, "s2")] {
            store
                .save_ask(NewAsk {
                    quote: Some(quote),
                    resume: Some(resume),
                    chat: Some(chat("notes")),
                    ..new_ask(question)
                })
                .unwrap();
        }

        let recalled = store.recall("s1", 32 * 1024).unwrap();

        assert_eq!(
            questions(&recalled),
            ["Shorter?"],
            "the quote costs no recall budget"
        );
        assert_eq!(
            store.chat_quote(&chat("notes")).unwrap(),
            Some(long.clone())
        );
        assert_eq!(store.session_quote("s2").unwrap(), Some(long));
        assert_eq!(store.session_quote("s3").unwrap(), None);
    }

    #[test]
    fn a_quote_this_daemon_refuses_reads_as_none_and_hides_nothing_else() {
        let (_directory, store) = opened();
        let quote = Quote::new("the notes".to_owned()).unwrap();
        quoted_ask(&store, "Shorter?", chat("notes"), &quote);
        tamper(&store, "UPDATE chats SET quote=' '", []);

        let next = ask(&store, "Again?", Some(chat("notes")));

        assert_eq!(store.chat_quote(&chat("notes")).unwrap(), None);
        assert_eq!(
            questions(&store.list(&query("again", None, 100)).unwrap().entries),
            ["Again?"]
        );
        assert!(store.get(next.id).is_ok());
    }

    #[test]
    fn a_quote_its_chat_has_already_makes_no_room() {
        let (_directory, store) = opened();
        let quote = Quote::new("a".repeat(Quote::MAX_BYTES)).unwrap();
        quoted_ask(&store, "First?", chat("notes"), &quote);
        let filler = "a".repeat(MAX_ENTRY_BYTES);
        let fillers = budget(HistorySection::Chat).text_bytes / MAX_ENTRY_BYTES - 2;
        let oldest = ask(&store, &filler, None);
        for _ in 1..fillers {
            ask(&store, &filler, None);
        }

        quoted_ask(&store, "Again?", chat("notes"), &quote);

        assert!(store.get(oldest.id).is_ok(), "nothing was evicted");
    }

    #[test]
    fn each_chats_quote_counts_once_against_the_chat_budget() {
        let (_directory, store) = opened();
        let quote = Quote::new("a".repeat(Quote::MAX_BYTES)).unwrap();
        let chats = budget(HistorySection::Chat).text_bytes / Quote::MAX_BYTES;
        let first = quoted_ask(&store, "First?", chat("chat 0"), &quote);
        for number in 1..=chats {
            quoted_ask(&store, "Next?", chat(&format!("chat {number}")), &quote);
        }

        assert!(
            matches!(store.get(first.id), Err(HistoryError::Missing)),
            "the oldest chat made room"
        );
    }

    #[test]
    fn search_finds_a_chat_by_its_quote() {
        let (_directory, store) = opened();
        ask(&store, "Unrelated?", None);
        let quoted = quoted_ask(
            &store,
            "Shorter?",
            chat("notes"),
            &Quote::new("Launch plan for Friday".to_owned()).unwrap(),
        );

        let found = store
            .list(&query("launch PLAN", None, 100))
            .unwrap()
            .entries;

        let ids: Vec<u64> = found.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, [quoted.id]);
    }

    #[test]
    fn a_session_recalls_its_newest_chats_that_fit_oldest_first() {
        let (_directory, store) = opened();
        let ask = |request: &str, resume: Option<&str>| {
            store
                .save_ask(NewAsk {
                    reply: "ok",
                    workdir: Path::new("/p"),
                    resume,
                    ..new_ask(request)
                })
                .unwrap();
        };
        ask("one", Some("c1"));
        ask("other", Some("c2"));
        ask("two", Some("c1"));
        ask("none", None);
        ask("three", Some("c1"));
        let questions = |resume: &str, max_bytes: usize| {
            store
                .recall(resume, max_bytes)
                .unwrap()
                .into_iter()
                .map(|ask| ask.text)
                .collect::<Vec<_>>()
        };

        assert_eq!(questions("c1", 1024), ["one", "two", "three"]);
        // "three" and "two" with their replies take 12 bytes; "one" doesn't fit.
        assert_eq!(questions("c1", 12), ["two", "three"]);
        assert_eq!(questions("c2", 1024), ["other"]);
        // A chat bigger than the whole budget doesn't hide the ones before it.
        ask("a request far longer than the budget", Some("c1"));
        assert_eq!(questions("c1", 12), ["two", "three"]);
        assert!(questions("c3", 1024).is_empty());
    }

    #[test]
    fn an_ask_keeps_the_chat_it_continued_and_a_folders_ask_keeps_none() {
        let (directory, store) = opened();

        let named = ask(&store, "When?", Some(chat("first")));
        let folders = ask(&store, "Where?", None);

        let key = |entry: &HistoryEntry| entry.content.chat().cloned();
        assert_eq!(key(&named), Some(chat("first")));
        assert_eq!(key(&folders), None);
        drop(store);
        let reopened = Store::open(directory.path(), &AtRest::for_tests()).unwrap();
        assert_eq!(key(&reopened.get(named.id).unwrap()), Some(chat("first")));
    }

    #[test]
    fn an_ask_whose_assistant_this_daemon_doesnt_know_survives_maintenance_without_a_chat() {
        let (_directory, store) = opened();
        let newer = ask(&store, "When?", Some(chat("first")));
        tamper(
            &store,
            "INSERT INTO chats(assistant,name) VALUES('future-assistant','first')",
            [],
        );
        tamper(
            &store,
            "UPDATE entries SET assistant='future-assistant' WHERE id=?1",
            params![sql_integer(newer.id).unwrap()],
        );

        store.maintenance().unwrap();

        let page = store.list(&collapsed("", None, 10)).unwrap();
        assert_eq!(page.warning, None);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].id, newer.id);
        assert_eq!(page.entries[0].content.chat(), None);
        let assistant: String = store
            .lock()
            .connection
            .query_row(
                "SELECT assistant FROM entries WHERE id=?1",
                [sql_integer(newer.id).unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(assistant, "future-assistant");
    }

    #[test]
    fn collapsing_shows_each_chat_once_at_its_newest_ask() {
        let (_directory, store) = opened();
        ask(&store, "first one", Some(chat("first")));
        ask(&store, "folder one", None);
        ask(&store, "second one", Some(chat("second")));
        ask(&store, "first two", Some(chat("first")));
        ask(&store, "folder two", None);

        let page = store.list(&collapsed("", None, 100)).unwrap();

        assert_eq!(
            questions(&page.entries),
            ["folder two", "first two", "second one", "folder one"]
        );
        assert_eq!(store.list(&query("", None, 100)).unwrap().entries.len(), 5);
    }

    #[test]
    fn a_collapsed_search_finds_a_chat_by_any_of_its_asks() {
        let (_directory, store) = opened();
        ask(&store, "lunch on friday", Some(chat("first")));
        ask(&store, "and dinner", Some(chat("first")));
        ask(&store, "breakfast", Some(chat("second")));

        let page = store.list(&collapsed("lunch", None, 100)).unwrap();

        assert_eq!(questions(&page.entries), ["lunch on friday"]);
    }

    #[test]
    fn collapsed_pages_never_repeat_a_chat() {
        let (_directory, store) = opened();
        ask(&store, "first one", Some(chat("first")));
        ask(&store, "second one", Some(chat("second")));
        ask(&store, "first two", Some(chat("first")));
        ask(&store, "third one", Some(chat("third")));

        let page = store.list(&collapsed("", None, 2)).unwrap();
        assert_eq!(questions(&page.entries), ["third one", "first two"]);
        let next = store.list(&collapsed("", page.next_before_id, 2)).unwrap();

        assert_eq!(questions(&next.entries), ["second one"]);
        assert_eq!(next.next_before_id, None);
    }

    #[test]
    fn reading_a_chat_returns_its_newest_asks_oldest_first_in_full() {
        let (_directory, store) = opened();
        let long = "x".repeat(600);
        ask(&store, "one", Some(chat("first")));
        ask(&store, "elsewhere", Some(chat("second")));
        ask(&store, &long, Some(chat("first")));
        ask(&store, "three", Some(chat("first")));
        let read = |before_id, limit| {
            store.chat(&ChatQuery {
                chat: chat("first"),
                before_id,
                limit,
            })
        };

        let page = read(None, 2).unwrap();
        assert_eq!(questions(&page.entries), [long.as_str(), "three"]);
        let older = read(page.next_before_id, 2).unwrap();
        assert_eq!(questions(&older.entries), ["one"]);
        assert_eq!(older.next_before_id, None);
        for limit in [0, ChatQuery::MAX_LIMIT + 1] {
            assert!(matches!(
                read(None, limit),
                Err(HistoryError::InvalidRequest)
            ));
        }
        assert!(matches!(
            store.chat(&ChatQuery {
                chat: chat("never"),
                before_id: None,
                limit: 10,
            }),
            Err(HistoryError::Missing)
        ));
    }

    #[test]
    fn deleting_a_chat_takes_its_asks_and_their_results_and_leaves_the_rest() {
        let (_directory, store) = opened();
        let asked = ask(&store, "one", Some(chat("first")));
        ask(&store, "two", Some(chat("first")));
        let other = ask(&store, "elsewhere", Some(chat("second")));
        let folder = ask(&store, "folder", None);
        store
            .save_result(asked.id, "Polish", "Polished.", now_ms())
            .unwrap();
        store
            .save_result(other.id, "Polish", "Kept.", now_ms())
            .unwrap();

        store.delete_chat(&chat("first")).unwrap();
        store.delete_chat(&chat("never")).unwrap();

        assert_eq!(
            listed(&store)
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [folder.id, other.id]
        );
        let results: i64 = store
            .lock()
            .connection
            .query_row("SELECT COUNT(*) FROM results", [], |row| row.get(0))
            .unwrap();
        assert_eq!(results, 1);
    }

    #[test]
    fn a_chat_whose_delete_fails_keeps_every_ask() {
        let (_directory, store) = opened();
        let first = ask(&store, "one", Some(chat("first")));
        let second = ask(&store, "two", Some(chat("first")));
        tamper(
            &store,
            &format!(
                "CREATE TEMP TRIGGER refuse BEFORE DELETE ON entries WHEN old.id={}
                 BEGIN SELECT RAISE(ABORT, 'injected'); END",
                first.id
            ),
            [],
        );

        assert!(matches!(
            store.delete_chat(&chat("first")),
            Err(HistoryError::Storage(_))
        ));

        assert_eq!(
            listed(&store)
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [second.id, first.id]
        );
    }

    fn chat_names(entries: &[HistoryEntry]) -> Vec<(&str, Option<&str>)> {
        entries
            .iter()
            .map(|entry| {
                let name = entry.content.chat().map(|chat| chat.name.as_str());
                (entry.text.as_str(), name)
            })
            .collect()
    }

    #[test]
    fn a_renamed_chat_takes_its_asks_and_search_finds_it_by_name() {
        let (directory, store) = opened();
        ask(&store, "one", Some(chat("first")));
        ask(&store, "elsewhere", Some(chat("second")));
        ask(&store, "two", Some(chat("first")));
        ask(&store, "folder", None);

        store
            .rename_chat(&chat("first"), &ChatName::new("Trip plans").unwrap())
            .unwrap();

        assert_eq!(
            chat_names(&listed(&store)),
            [
                ("folder", None),
                ("two", Some("Trip plans")),
                ("elsewhere", Some("second")),
                ("one", Some("Trip plans")),
            ]
        );
        let page = |name: &str| {
            store.chat(&ChatQuery {
                chat: chat(name),
                before_id: None,
                limit: 10,
            })
        };
        assert_eq!(
            questions(&page("Trip plans").unwrap().entries),
            ["one", "two"]
        );
        assert!(matches!(page("first"), Err(HistoryError::Missing)));
        assert_eq!(
            questions(&store.list(&collapsed("trip", None, 10)).unwrap().entries),
            ["two"]
        );
        let next = ask(&store, "three", Some(chat("Trip plans")));
        assert_eq!(chat_names(&[next]), [("three", Some("Trip plans"))]);
        drop(store);
        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();
        assert_eq!(
            chat_names(&listed(&store))[0],
            ("three", Some("Trip plans"))
        );
    }

    #[test]
    fn a_chat_renamed_to_a_name_another_chat_has_is_refused() {
        let (_directory, store) = opened();
        ask(&store, "one", Some(chat("first")));
        ask(&store, "two", Some(chat("second")));

        assert!(matches!(
            store.rename_chat(&chat("first"), &ChatName::new("second").unwrap()),
            Err(HistoryError::ChatNameTaken)
        ));
        assert_eq!(
            chat_names(&listed(&store)),
            [("two", Some("second")), ("one", Some("first"))]
        );
    }

    #[test]
    fn expiring_a_chats_last_ask_drops_the_chat() {
        let (_directory, store) = opened();
        let old = ask(&store, "one", Some(chat("first")));
        tamper(
            &store,
            "UPDATE entries SET created=0 WHERE id=?1",
            [old.id as i64],
        );

        store.maintenance().unwrap();

        let chats: i64 = store
            .lock()
            .connection
            .query_row("SELECT COUNT(*) FROM chats", [], |row| row.get(0))
            .unwrap();
        assert_eq!(chats, 0);
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
        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();
        assert_eq!(store.get(saved.id).unwrap().title, Some(title));
        assert_eq!(store.rename(saved.id, None).unwrap().title, None);
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
        let (_directory, store) = opened();
        let saved = store
            .insert(
                entry("old", HistoryStatus::TranscriptionFailed),
                Some(audio()),
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
            content: HistoryContent::Ask {
                reply: "Tuesday at noon".into(),
                chat: None,
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
            content: HistoryContent::Ask {
                reply: "x".repeat(MAX_ENTRY_BYTES + 1),
                chat: None,
            },
            ..asked
        };
        assert!(matches!(
            store.insert(oversized, None),
            Err(HistoryError::TooLarge)
        ));
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
        let reopened = Store::open(directory.path(), &AtRest::for_tests()).unwrap();
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
            (RpcErrorCode::ChatNameTaken, 308),
            (RpcErrorCode::HistoryProjectUnavailable, 903),
            (RpcErrorCode::HistoryEntryEmpty, 904),
            (RpcErrorCode::HistoryTranscriptUnreadable, 905),
            (RpcErrorCode::HistoryFull, 906),
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
    fn an_entry_keeps_its_metadata_across_a_reopen() {
        let (directory, store) = opened();
        let sent = HistoryEntry {
            application: Some(Application {
                name: "Editor".into(),
                bundle_id: Some("test.editor".into()),
            }),
            project: Some(PathBuf::from("/project")),
            content: HistoryContent::Dictation(DictationContent {
                language: "en".into(),
                engine: "test".into(),
                own_voice_only: true,
                audio: AudioAvailability::Unavailable,
                retrying: false,
            }),
            ..entry("private words", HistoryStatus::DeliveryFailed)
        };
        let first = store.insert(sent, None).unwrap();
        let created = first.created_at_ms;
        drop(store);

        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();

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
    }

    #[test]
    fn a_deleted_id_is_never_reused() {
        let (_directory, store) = opened();
        let first = save(&store, "private words", HistoryStatus::DeliveryFailed, None);

        assert!(store.delete(first.id).unwrap());
        assert!(!store.delete(first.id).unwrap());

        let second = save(&store, "next", HistoryStatus::Success, None);
        assert!(second.id > first.id);
    }

    #[test]
    fn the_history_folder_and_database_are_private() {
        let (directory, _store) = opened();

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
    fn history_clear_removes_every_status_and_audio() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio();
        let history = directory.path().join("history");
        let store = Store::open(&history, &AtRest::for_tests()).unwrap();
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
                last_id = store.insert(saved, Some(source)).unwrap().id;
            }
        }
        store.mark_delete(&mut store.lock(), last_id).unwrap();
        assert_eq!(
            store.clear(None).unwrap(),
            ClearHistoryResult {
                deleted_entries: 8,
                cleanup_pending: false,
            }
        );
        assert!(listed(&store).is_empty());
        let recordings: i64 = store
            .lock()
            .connection
            .query_row("SELECT count(*) FROM audio", [], |row| row.get(0))
            .unwrap();
        assert_eq!(recordings, 0);
        assert_eq!(store.clear(None).unwrap().deleted_entries, 0);
    }

    #[test]
    fn ids_are_not_reused_after_a_clear_and_reopen() {
        let (directory, store) = opened();
        let last = save(&store, "private words", HistoryStatus::Success, None);
        store.clear(None).unwrap();
        drop(store);

        let store = Store::open(directory.path(), &AtRest::for_tests()).unwrap();

        let later = save(&store, "later", HistoryStatus::Success, None);
        assert!(later.id > last.id);
        assert_eq!(store.get(later.id).unwrap().text, "later");
    }

    #[test]
    fn history_clear_busy_and_database_failure_leave_entries_intact() {
        let (_directory, store) = opened();
        let source = audio();
        let saved = save(&store, "retry", HistoryStatus::Success, Some(source));
        let other = save(&store, "other", HistoryStatus::Success, None);
        let lease = store.begin_retry(saved.id).unwrap();
        assert!(matches!(store.clear(None), Err(HistoryError::Busy)));
        assert_eq!(store.get(other.id).unwrap().text, "other");
        assert!(has_audio(&store, lease.entry.id));
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
        assert!(has_audio(&store, saved.id));
        assert_eq!(store.clear(None).unwrap().deleted_entries, 2);
    }

    #[test]
    fn history_ownership_outlives_the_store_handle_until_retry_releases() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio();
        let history = directory.path().join("history");
        let store = Store::open(&history, &AtRest::for_tests()).unwrap();
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(source),
        );
        let lease = store.begin_retry(failed.id).unwrap();
        assert!(matches!(
            Store::open(&history, &AtRest::for_tests()),
            Err(HistoryError::Busy)
        ));
        drop(store);
        assert!(matches!(
            Store::open(&history, &AtRest::for_tests()),
            Err(HistoryError::Busy)
        ));
        drop(lease);
        let reopened = Store::open(&history, &AtRest::for_tests()).unwrap();
        assert_eq!(reopened.get(failed.id).unwrap().text, "partial");
        assert!(has_audio(&reopened, failed.id));
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
        let (_directory, store) = opened();
        let source = audio();
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(source),
        );
        let _lease = store.begin_retry(failed.id).unwrap();
        let error = HistoryError::transcription(
            RpcErrorCode::TranscriptionFailed,
            io::Error::other("x".repeat(MAX_ERROR_MESSAGE_BYTES)),
        );
        assert!(error_chain(&error).len() > MAX_ERROR_MESSAGE_BYTES);
        store.attempt_failed(failed.id, &error).unwrap();
        assert_eq!(
            store.get(failed.id).unwrap().error,
            Some(RpcFailure::new(
                RpcErrorCode::TranscriptionFailed,
                "history transcription failed"
            ))
        );
    }

    #[test]
    fn retry_pins_original_audio_and_recovers_the_same_entry() {
        let (_directory, store) = opened();
        let source = audio();
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(source),
        );
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
        assert!(!has_audio(&store, lease.entry.id));
        drop(lease);
        assert!(!store.get(failed.id).unwrap().dictation().unwrap().retrying);
        assert!(matches!(
            store.begin_retry(failed.id),
            Err(HistoryError::AudioUnavailable)
        ));
    }

    #[test]
    fn regeneration_retains_audio_and_original_expiry_until_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio();
        let history = directory.path().join("history");
        let store = Store::open(&history, &AtRest::for_tests()).unwrap();
        let original = save(&store, "first", HistoryStatus::Success, Some(source));
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
        let store = Store::open(&history, &AtRest::for_tests()).unwrap();
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
        let (_directory, store) = opened();
        let source = audio();
        let failed = save(&store, "partial", HistoryStatus::Success, Some(source));
        let lease = store.begin_retry(failed.id).unwrap();
        tamper(
            &store,
            "UPDATE entries SET expires=1 WHERE id=?1",
            [sql_integer(failed.id).unwrap()],
        );
        store.maintenance().unwrap();
        assert!(has_audio(&store, lease.entry.id));
        assert_eq!(
            store.get(failed.id).unwrap().audio(),
            AudioAvailability::Expired
        );
        drop(lease);
        assert!(!has_audio(&store, failed.id));
        assert_eq!(store.get(failed.id).unwrap().text, "partial");
        assert!(matches!(
            store.begin_retry(failed.id),
            Err(HistoryError::AudioUnavailable)
        ));
    }

    #[test]
    fn the_audio_budget_counts_the_bytes_the_wav_takes() {
        let wav = encode_wav(&[0.25; 3]).unwrap();

        assert_eq!(encoded_bytes(3), wav.len() as u64);
    }

    #[test]
    fn audio_an_older_build_saved_still_plays_back() {
        let (directory, store) = opened();
        let saved = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(audio()),
        );
        let fixture = aria_kit::audio::canonical_fixture(directory.path());
        tamper(
            &store,
            "UPDATE audio SET wav=?1 WHERE entry=?2",
            params![fs::read(&fixture).unwrap(), sql_integer(saved.id).unwrap()],
        );
        let mut expected = RecordingReader::from_wav_file(&fixture).unwrap();
        expected.fill().unwrap();

        let mut lease = store.begin_retry(saved.id).unwrap();
        lease.recording.fill().unwrap();

        assert_eq!(lease.recording.samples(), expected.samples());
    }

    #[test]
    fn saved_audio_lives_in_the_database_and_a_retry_reads_it_into_memory() {
        let directory = tempfile::tempdir().unwrap();
        let source = audio();
        let history = directory.path().join("history");
        let store = Store::open(&history, &AtRest::for_tests()).unwrap();
        let saved = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(source),
        );
        let mut kept: Vec<_> = fs::read_dir(&history)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();
        kept.sort();
        assert_eq!(kept, ["history.db", "history.lock"]);

        let mut lease = store.begin_retry(saved.id).unwrap();

        lease.recording.fill().unwrap();
        assert_eq!(lease.recording.samples(), source);
    }

    #[test]
    fn opening_removes_the_audio_files_an_older_daemon_kept() {
        let directory = tempfile::tempdir().unwrap();
        let history = directory.path().join("history");
        let files = history.join("audio");
        private_fs::prepare_directory(&history).unwrap();
        private_fs::prepare_directory(&files).unwrap();
        fs::write(files.join("1.wav"), "plaintext audio").unwrap();

        drop(Store::open(&history, &AtRest::for_tests()).unwrap());

        assert!(!files.exists());
    }

    #[test]
    fn deleting_an_entry_deletes_its_audio() {
        let (_directory, store) = opened();
        let source = audio();
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(source),
        );
        let later = save(&store, "later text", HistoryStatus::Success, None);
        assert!(has_audio(&store, failed.id));
        assert!(store.delete(failed.id).unwrap());
        assert!(matches!(store.get(failed.id), Err(HistoryError::Missing)));
        assert!(!has_audio(&store, failed.id));
        assert_eq!(store.get(later.id).unwrap().text, "later text");
    }
    #[test]
    fn an_interrupted_deletion_finishes_on_the_next_sweep() {
        let (_directory, store) = opened();
        let pending = save(&store, "pending", HistoryStatus::Success, None);
        {
            let mut state = store.lock();
            store.mark_delete(&mut state, pending.id).unwrap();
        }
        assert!(matches!(store.get(pending.id), Err(HistoryError::Missing)));
    }

    #[test]
    fn audio_deleted_behind_the_store_is_never_retryable() {
        let (_directory, store) = opened();
        let source = audio();
        let failed = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(source),
        );
        tamper(
            &store,
            "DELETE FROM audio WHERE entry=?1",
            [sql_integer(failed.id).unwrap()],
        );
        assert_eq!(
            store.get(failed.id).unwrap().audio(),
            AudioAvailability::Unavailable
        );
    }

    #[test]
    fn invalid_retained_audio_stays_unavailable_until_cleanup_finishes() {
        let (_directory, store) = opened();
        let source = audio();
        let corrupt = save(
            &store,
            "partial",
            HistoryStatus::TranscriptionFailed,
            Some(source),
        );
        tamper(
            &store,
            "UPDATE audio SET wav=X'00' WHERE entry=?1",
            [sql_integer(corrupt.id).unwrap()],
        );
        assert!(matches!(
            store.begin_retry(corrupt.id),
            Err(HistoryError::AudioUnavailable)
        ));
        assert_eq!(
            store.get(corrupt.id).unwrap().audio(),
            AudioAvailability::Unavailable
        );
        assert!(!has_audio(&store, corrupt.id));
    }

    #[test]
    fn search_folds_case_and_takes_wildcards_literally() {
        let (_directory, store) = opened();
        let first = save(&store, "CAFÉ [x]", HistoryStatus::Success, None);
        save(&store, "unrelated", HistoryStatus::Success, None);
        let last = save(&store, "café [x]", HistoryStatus::Success, None);

        let found = store.list(&query("café [x]", None, 100)).unwrap();

        assert_eq!(
            found
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
    }

    #[test]
    fn a_cursor_survives_a_deleted_entry() {
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
    }

    #[test]
    fn search_matches_the_application_name() {
        let (_directory, store) = opened();
        let first = save(&store, "CAFÉ [x]", HistoryStatus::Success, None);
        let last = save(&store, "café [x]", HistoryStatus::Success, None);

        let application_matches = store.list(&query("EDITOR", None, 100)).unwrap();

        assert_eq!(
            application_matches
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [last.id, first.id]
        );
    }

    #[test]
    fn out_of_range_requests_are_invalid() {
        let (_directory, store) = opened();
        assert!(matches!(
            store.get(u64::MAX),
            Err(HistoryError::InvalidRequest)
        ));
        assert!(matches!(store.get(0), Err(HistoryError::InvalidRequest)));
        assert!(matches!(
            store.list(&query(&"x".repeat(257), None, 100)),
            Err(HistoryError::InvalidRequest)
        ));
        let mut invalid = entry("text", HistoryStatus::Success);
        invalid.duration_ms = Some(u64::MAX);
        assert!(matches!(
            store.insert(invalid, None),
            Err(HistoryError::InvalidRequest)
        ));
    }

    #[test]
    fn a_row_with_a_negative_duration_reads_as_missing() {
        let (_directory, store) = opened();
        let saved = save(&store, "text", HistoryStatus::Success, None);
        tamper(
            &store,
            "UPDATE entries SET duration=-1 WHERE id=?1",
            [sql_integer(saved.id).unwrap()],
        );

        assert!(matches!(store.get(saved.id), Err(HistoryError::Missing)));
    }

    #[test]
    fn an_oversized_entry_is_refused() {
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
    }

    #[test]
    fn the_dictation_text_budget_evicts_the_oldest_entry() {
        let (_directory, store) = opened();
        let first = save(&store, "keep until full", HistoryStatus::Success, None);
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
            load_rows(&state, Rows::All)
                .unwrap()
                .0
                .iter()
                .map(|entry| entry.text.len())
                .sum::<usize>(),
            budget(HistorySection::Dictation).text_bytes
        );
    }

    #[test]
    fn an_entry_expires_after_its_text_ttl() {
        let (_directory, store) = opened();
        save(&store, "keep until full", HistoryStatus::Success, None);
        store
            .lock()
            .connection
            .execute("UPDATE entries SET created=1", [])
            .unwrap();

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
            transaction.execute("INSERT INTO entries(created,source,text,complete,status,language,own_voice,engine,section) VALUES(?1,0,'',0,1,'en',1,'test',0)", [sql_integer(now_ms()).unwrap()]).unwrap();
        }
        transaction.commit().unwrap();
        drop(state);
        save(&store, "", HistoryStatus::TranscriptionFailed, None);
        assert!(matches!(store.get(first.id), Err(HistoryError::Missing)));
        assert_eq!(
            load_rows(&store.lock(), Rows::All).unwrap().0.len(),
            budget(HistorySection::Dictation).entries
        );
    }

    #[test]
    fn audio_budget_evicts_oldest_recording_without_losing_its_text() {
        let (_directory, store) = opened();
        let source = audio();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(save(&store, "partial", HistoryStatus::Success, Some(source)).id);
        }
        let lease = store.begin_retry(ids[0]).unwrap();
        for id in &ids {
            tamper(
                &store,
                "UPDATE audio SET wav=zeroblob(?1) WHERE entry=?2",
                [50 * 1024 * 1024, sql_integer(*id).unwrap()],
            );
        }
        store.maintenance().unwrap();
        assert!(!has_audio(&store, ids[1]));
        assert_eq!(store.get(ids[1]).unwrap().text, "partial");
        assert_eq!(
            store.get(ids[1]).unwrap().audio(),
            AudioAvailability::Unavailable
        );
        assert!(has_audio(&store, ids[0]));
        assert!(has_audio(&store, ids[2]));
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
            Store::open(&history, &AtRest::for_tests()),
            Err(HistoryError::Storage(_))
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
        let other = directory.path().join("other");
        private_fs::prepare_directory(&other).unwrap();
        symlink(&target, other.join("history.db-journal")).unwrap();
        assert!(matches!(
            Store::open(&other, &AtRest::for_tests()),
            Err(HistoryError::Storage(_))
        ));
        let third = directory.path().join("third");
        let store = Store::open(&third, &AtRest::for_tests()).unwrap();
        drop(store);
        fs::set_permissions(third.join("history.db"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Store::open(&third, &AtRest::for_tests()),
            Err(HistoryError::Storage(_))
        ));
    }

    #[test]
    fn a_commit_leaves_no_journal() {
        let (directory, store) = opened();
        let state = store.lock();
        let transaction = state.connection.unchecked_transaction().unwrap();
        transaction.execute("INSERT INTO entries(created,source,text,complete,status,language,own_voice,engine,section) VALUES(?1,0,'private',1,0,'en',0,'test',0)", [sql_integer(now_ms()).unwrap()]).unwrap();
        let journal = directory.path().join("history.db-journal");
        assert!(journal.exists());
        transaction.commit().unwrap();
        assert!(!journal.exists());
    }

    #[test]
    fn locked_and_full_databases_return_safe_typed_errors() {
        let (directory, store) = opened();
        let contender = keyed(directory.path().join("history.db"));
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

    #[test]
    fn asks_whose_chat_this_daemon_cant_read_each_show_when_chats_collapse() {
        let (_directory, store) = opened();
        {
            let state = store.lock();
            state
                .connection
                .execute(
                    "INSERT INTO chats(assistant,name) VALUES('a-newer-assistant','notes')",
                    [],
                )
                .unwrap();
            for text in ["first", "second"] {
                state
                    .connection
                    .execute(
                        "INSERT INTO entries(created,source,text,complete,status,language,own_voice,engine,section,reply,assistant,chat_name)
                         VALUES(?1,0,?2,1,0,'',0,'',2,'reply','a-newer-assistant','notes')",
                        params![sql_integer(now_ms()).unwrap(), text],
                    )
                    .unwrap();
            }
        }

        let page = store.list(&collapsed("", None, 10)).unwrap();

        let mut texts: Vec<_> = page
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        texts.sort_unstable();
        assert_eq!(texts, ["first", "second"]);
    }
}
