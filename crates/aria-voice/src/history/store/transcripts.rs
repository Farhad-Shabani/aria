//! The files of Mac Audio entries: making a session's file and entry,
//! ending it, and removing it. Aria owns its transcripts folder, so the file
//! at an entry's path is the entry's, as long as it is a regular file the
//! person owns.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use aria_primitives::error_chain;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use rusqlite::params;
use time::OffsetDateTime;

use super::{
    CLEANUP_WARNING, State, Store, encode_error, load, section_number, sql_integer, status_number,
    unsigned, validate_id,
};
use crate::history::{
    HistoryContent, HistoryEntry, HistoryError, HistorySection, HistorySource, HistoryStatus,
};
use crate::transcript_file::{self, Creation, FolderIdentity, OpenError, ReadError, Removal};

/// Each deleted entry's file still to remove, by path. A row goes in before
/// the file goes, and leaves only once the file is gone and its folder
/// synced. So a crash, a folder Aria can't change yet, or a failed sync
/// leaves the removal for the next sweep, and the file never comes back.
pub(super) const REMOVALS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS file_removals (
    path BLOB NOT NULL CHECK(length(path)<=4096))";

/// Each ended Mac Audio entry whose excerpt still waits for its file, as
/// when its folder couldn't be read. The entry's outcome is already saved.
pub(super) const EXCERPTS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS pending_excerpts (
    entry INTEGER PRIMARY KEY REFERENCES entries(id) ON DELETE CASCADE)";

/// Why a transcript entry ended without its session saying how.
pub const TRANSCRIPTION_INTERRUPTED: &str = "Transcription stopped unexpectedly.";

/// Why [`Store::create_transcript`] started no transcript.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CreateError {
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("the transcript file couldn't be created")]
    File(#[source] io::Error),
}

/// The files [`Store::owned_files`] found. A path matches by its exact
/// spelling, or by its name in the same folder however a link or other case
/// spells that folder. A folder that can't be read matches by spelling only.
#[derive(Default)]
pub(crate) struct OwnedFiles {
    paths: HashSet<PathBuf>,
    /// The name keys in each folder, by the folder's device and inode.
    folders: HashMap<FolderIdentity, HashSet<String>>,
}

impl OwnedFiles {
    /// Reads the identity of the folder each of `paths` is in.
    pub(crate) fn locate(paths: HashSet<PathBuf>) -> Self {
        let mut identities: HashMap<&Path, Option<FolderIdentity>> = HashMap::new();
        let mut folders: HashMap<FolderIdentity, HashSet<String>> = HashMap::new();
        for path in &paths {
            let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
                continue;
            };
            let identity = *identities
                .entry(folder)
                .or_insert_with(|| transcript_file::folder_identity(folder));
            if let Some(identity) = identity {
                folders
                    .entry(identity)
                    .or_default()
                    .insert(transcript_file::name_key(name));
            }
        }
        drop(identities);
        Self { paths, folders }
    }

    pub(crate) fn contains(&self, path: &Path) -> bool {
        if self.paths.contains(path) {
            return true;
        }
        let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        transcript_file::folder_identity(folder)
            .and_then(|identity| self.folders.get(&identity))
            .is_some_and(|names| names.contains(&transcript_file::name_key(name)))
    }
}

impl Store {
    /// Starts a session's transcript in `folder`, under a timestamped name
    /// nothing has on disk or in history. The entry commits first, then the
    /// file is made, so a crash between leaves an entry without its file,
    /// which history drops. A failure to make the file takes the entry back.
    /// The entry has no excerpt and no duration until
    /// [`Store::finish_transcript`]: a transcript entry without a duration
    /// is a session still recording. Fails with `HistoryError::Full` at
    /// `MAX_TRANSCRIPTS`. Returns the file, its path and the entry's id.
    pub(crate) fn create_transcript(
        &self,
        folder: &Path,
    ) -> Result<(File, PathBuf, u64), CreateError> {
        let now = OffsetDateTime::now_utc();
        let created_at_ms =
            u64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap_or_default();
        let _files = self.lease();
        for name in transcript_file::names_at(now) {
            let transcript = folder.join(name.map_err(CreateError::File)?);
            let Some(entry) = self.start_entry(&transcript, created_at_ms)? else {
                continue;
            };
            let path = recording(&entry)?;
            match transcript_file::create(&path, Creation::New) {
                Ok(file) => return Ok((file, path, entry.id)),
                // Something outside Aria took the name since history looked.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.cancel_transcript(entry.id)?;
                }
                Err(error) => {
                    self.cancel_transcript(entry.id)?;
                    return Err(CreateError::File(error));
                }
            }
        }
        Err(CreateError::File(transcript_file::taken()))
    }

    /// The entry of a session about to write `transcript`, or `None` while
    /// anything has that name. Called under the file lease.
    fn start_entry(
        &self,
        transcript: &Path,
        created_at_ms: u64,
    ) -> Result<Option<HistoryEntry>, HistoryError> {
        if self.is_taken(transcript)? {
            return Ok(None);
        }
        let mut state = self.lock();
        let entry = self.insert_into(
            &mut state,
            transcript_entry(transcript, created_at_ms),
            None,
            None,
            None,
        )?;
        state.recording.insert(entry.id);
        Ok(Some(entry))
    }

    /// Whether anything is at `transcript`, or an entry or a pending removal
    /// in any folder has its name. Names compare as a volume that ignores case
    /// compares them, so a folder reached through a link, or spelled in other
    /// case, can't hand out a name twice. Called under the file lease, so a
    /// free name stays free until the caller's entry takes it.
    fn is_taken(&self, transcript: &Path) -> Result<bool, HistoryError> {
        let Some(name) = transcript.file_name() else {
            return Err(HistoryError::InvalidRequest);
        };
        let name = transcript_file::name_key(name);
        {
            let state = self.lock();
            let mut statement = state.connection.prepare(
                "SELECT transcript FROM entries WHERE transcript IS NOT NULL
                 UNION ALL SELECT path FROM file_removals",
            )?;
            for path in statement.query_map([], |row| row.get::<_, Vec<u8>>(0))? {
                let held = Path::new(OsStr::from_bytes(&path?))
                    .file_name()
                    .map(transcript_file::name_key);
                if held.as_ref() == Some(&name) {
                    return Ok(true);
                }
            }
        }
        match transcript.symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(HistoryError::storage(error)),
            Ok(_) => Ok(true),
        }
    }

    /// Takes back an entry started for a file its session never made.
    /// Whatever sits at the path stays.
    fn cancel_transcript(&self, id: u64) -> Result<(), HistoryError> {
        validate_id(id)?;
        let mut state = self.lock();
        state.recording.remove(&id);
        self.mark_delete(&mut state, id)?;
        self.remove_entry(&mut state, id)
    }

    /// Ends the entry [`Store::create_transcript`] started with its
    /// session's outcome, then takes the file's excerpt. A blank transcript
    /// takes its file and its entry with it, and returns `None`. A file it
    /// can't read yet keeps the outcome, and the sweep takes the excerpt
    /// once it can.
    pub fn finish_transcript(
        &self,
        id: u64,
        duration_ms: u64,
        failure: Option<RpcFailure>,
    ) -> Result<Option<HistoryEntry>, HistoryError> {
        validate_id(id)?;
        let _files = self.lease();
        let transcript = {
            let mut state = self.lock();
            // Whatever happens next, the session no longer runs.
            state.recording.remove(&id);
            let transcript = load(&state, id).and_then(|entry| recording(&entry))?;
            end_recording(&state, id, duration_ms, failure.as_ref())?;
            transcript
        };
        let ending = read_ending(&transcript);
        let mut state = self.lock();
        match ending {
            Some(ending) => self.settle(&mut state, id, &transcript, ending),
            None => load(&state, id).map(Some),
        }
    }

    /// Takes entry `id`'s excerpt from what `ending` found at `transcript`.
    /// Returns the entry, or `None` once it's gone with its file.
    fn settle(
        &self,
        state: &mut State,
        id: u64,
        transcript: &Path,
        ending: Ending,
    ) -> Result<Option<HistoryEntry>, HistoryError> {
        match ending {
            Ending::Text(text) => {
                state.connection.execute(
                    "UPDATE entries SET text=?1 WHERE id=?2",
                    params![text, sql_integer(id)?],
                )?;
            }
            // Its excerpt stays empty.
            Ending::NotText(error) => {
                forget_excerpt(state, id)?;
                return Err(error);
            }
            Ending::Foreign => {
                tracing::warn!(
                    id,
                    "a transcription's path holds a link or another user's file, so it stays without an entry"
                );
                self.mark_delete(state, id)?;
                self.remove_entry(state, id)?;
                return Ok(None);
            }
            Ending::Blank => {
                self.remove_transcript(state, transcript)?;
                self.mark_delete(state, id)?;
                self.remove_entry(state, id)?;
                return Ok(None);
            }
            Ending::Missing => {
                self.mark_delete(state, id)?;
                self.remove_entry(state, id)?;
                return Ok(None);
            }
        }
        forget_excerpt(state, id)?;
        load(state, id).map(Some)
    }

    /// Removes a deleted entry's file at `transcript`. The removal is
    /// recorded first, and its record goes only once the file is gone and
    /// its folder synced. Until then [`Store::finish_pending`] retries it.
    /// A link or another user's file at the path stays, with a warning.
    pub(super) fn remove_transcript(
        &self,
        state: &mut State,
        transcript: &Path,
    ) -> Result<(), HistoryError> {
        state.connection.execute(
            "INSERT INTO file_removals(path) VALUES(?1)",
            [transcript.as_os_str().as_bytes()],
        )?;
        let record = state.connection.last_insert_rowid();
        if is_removed(state, transcript) {
            drop_removal(state, record)?;
        } else {
            state.warning = Some(CLEANUP_WARNING.to_owned());
        }
        Ok(())
    }

    /// Finishes each removal and each excerpt still waiting. Runs when the
    /// store opens and with every full sweep, under the file lease.
    pub(super) fn finish_pending(&self) -> Result<(), HistoryError> {
        let waiting = {
            let state = self.lock();
            let mut statement = state.connection.prepare(
                "SELECT entries.id,entries.transcript FROM pending_excerpts
                 JOIN entries ON entries.id=pending_excerpts.entry",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        unsigned(row.get(0)?, 0)?,
                        PathBuf::from(OsString::from_vec(row.get(1)?)),
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        // Read without the lock, so a slow volume stalls only this sweep.
        for (id, transcript) in waiting {
            if let Some(ending) = read_ending(&transcript) {
                let mut state = self.lock();
                if let Err(error) = self.settle(&mut state, id, &transcript, ending) {
                    tracing::warn!(id, error = %error_chain(&error), "a transcript's excerpt could not be saved");
                }
            }
        }
        let mut state = self.lock();
        let removals = state
            .connection
            .prepare("SELECT rowid,path FROM file_removals")?
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    PathBuf::from(OsString::from_vec(row.get(1)?)),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (record, path) in removals {
            if is_removed(&mut state, &path) {
                drop_removal(&state, record)?;
            }
        }
        Ok(())
    }

    /// Ends each entry still recording that no session of this store owns.
    /// One daemon owns the store, through `history.lock`, so such an entry
    /// lost its session to a crash or to an end that couldn't be saved. Its
    /// file is read without the lock, so a slow volume stalls only this
    /// sweep. The caller holds the file lease, so nothing else moves the file.
    pub(super) fn finish_interrupted(&self) -> Result<(), HistoryError> {
        let interrupted = {
            let state = self.lock();
            let mut statement = state.connection.prepare(
                "SELECT id,created,transcript FROM entries WHERE section=?1 AND duration IS NULL",
            )?;
            statement
                .query_map(
                    [section_number(HistorySection::SystemTranscription)],
                    |row| {
                        Ok((
                            unsigned(row.get(0)?, 0)?,
                            unsigned(row.get(1)?, 1)?,
                            PathBuf::from(OsString::from_vec(row.get(2)?)),
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(id, _, _)| !state.recording.contains(id))
                .collect::<Vec<_>>()
        };
        for (id, created_at_ms, transcript) in interrupted {
            // The last paragraph it wrote is as close to its end as anything.
            let duration_ms = transcript
                .symlink_metadata()
                .ok()
                .as_ref()
                .and_then(transcript_file::modified_ms)
                .map_or(0, |modified| modified.saturating_sub(created_at_ms));
            let failure = RpcFailure::new(RpcErrorCode::Internal, TRANSCRIPTION_INTERRUPTED);
            // The lock goes before the file is read.
            let ended = end_recording(&self.lock(), id, duration_ms, Some(&failure));
            let ended = ended.and_then(|()| match read_ending(&transcript) {
                Some(ending) => self
                    .settle(&mut self.lock(), id, &transcript, ending)
                    .map(drop),
                None => Ok(()),
            });
            if let Err(error) = ended {
                tracing::warn!(id, error = %error_chain(&error), "an interrupted transcription could not be ended");
                self.lock().warning = Some(CLEANUP_WARNING.to_owned());
            }
        }
        Ok(())
    }

    /// Each Mac Audio entry whose file is gone, with that file. Checked
    /// without the lock, so a slow volume stalls only this sweep.
    pub(super) fn gone_transcripts(&self) -> Result<HashMap<u64, PathBuf>, HistoryError> {
        Ok(self
            .transcripts()?
            .into_iter()
            .filter(|(_, transcript)| transcript_file::is_gone(transcript))
            .collect())
    }

    /// Each Mac Audio entry's id and the file it names.
    fn transcripts(&self) -> Result<Vec<(u64, PathBuf)>, HistoryError> {
        let state = self.lock();
        let mut statement = state
            .connection
            .prepare("SELECT id,transcript FROM entries WHERE section=?1 AND deleting=0")?;
        let transcripts = statement
            .query_map(
                [section_number(HistorySection::SystemTranscription)],
                |row| {
                    Ok((
                        unsigned(row.get(0)?, 0)?,
                        PathBuf::from(OsString::from_vec(row.get(1)?)),
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(transcripts)
    }

    /// The files history owns, which no unsaved list may offer, no cleanup
    /// or Discard remove, and no `--output` session append to: each entry's
    /// file, and a file that waits to be removed.
    pub(crate) fn owned_files(&self) -> Result<OwnedFiles, HistoryError> {
        let paths = {
            let state = self.lock();
            let mut statement = state.connection.prepare(
                "SELECT transcript FROM entries WHERE transcript IS NOT NULL
                 UNION ALL SELECT path FROM file_removals",
            )?;
            statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))?
                .map(|path| path.map(|path| PathBuf::from(OsString::from_vec(path))))
                .collect::<rusqlite::Result<HashSet<_>>>()?
        };
        Ok(OwnedFiles::locate(paths))
    }
}

fn transcript_entry(transcript: &Path, created_at_ms: u64) -> HistoryEntry {
    HistoryEntry {
        id: 0,
        title: None,
        content: HistoryContent::SystemTranscription {
            transcript: transcript.to_path_buf(),
            read: None,
        },
        created_at_ms,
        duration_ms: None,
        source: HistorySource::Local,
        application: None,
        project: None,
        text: String::new(),
        complete: true,
        result_count: 0,
        status: HistoryStatus::Success,
        error: None,
        warning: None,
        deletion_pending: false,
    }
}

/// The file of a transcript entry that is still recording.
fn recording(entry: &HistoryEntry) -> Result<PathBuf, HistoryError> {
    match (&entry.content, entry.duration_ms) {
        (HistoryContent::SystemTranscription { transcript, .. }, None) => Ok(transcript.clone()),
        _ => Err(HistoryError::InvalidRequest),
    }
}

/// Whether a deleted entry's file is gone for good: removed, never there, or
/// a link or another user's file, which stays.
fn is_removed(state: &mut State, transcript: &Path) -> bool {
    match transcript_file::unlink(transcript) {
        Ok(Removal::Removed | Removal::Missing) => true,
        Ok(Removal::Foreign) => {
            tracing::warn!(
                "a deleted entry's path holds a link or another user's file, so it stays"
            );
            state.warning = Some(CLEANUP_WARNING.to_owned());
            true
        }
        Ok(Removal::Unavailable) => false,
        Err(error) => {
            tracing::warn!(%error, "a deleted entry's file waits to be removed");
            false
        }
    }
}

/// Saves how entry `id`'s session ended, and marks its excerpt as waiting
/// for its file, together. The entry no longer records.
fn end_recording(
    state: &State,
    id: u64,
    duration_ms: u64,
    failure: Option<&RpcFailure>,
) -> Result<(), HistoryError> {
    let transaction = state.connection.unchecked_transaction()?;
    transaction.execute(
        "UPDATE entries SET duration=?1,complete=?2,status=?3,error=?4 WHERE id=?5",
        params![
            sql_integer(duration_ms)?,
            failure.is_none(),
            status_number(match failure {
                None => HistoryStatus::Success,
                Some(_) => HistoryStatus::TranscriptionFailed,
            }),
            failure.map(encode_error).transpose()?,
            sql_integer(id)?,
        ],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO pending_excerpts(entry) VALUES(?1)",
        [sql_integer(id)?],
    )?;
    transaction.commit()?;
    Ok(())
}

fn forget_excerpt(state: &State, id: u64) -> Result<(), HistoryError> {
    state.connection.execute(
        "DELETE FROM pending_excerpts WHERE entry=?1",
        [sql_integer(id)?],
    )?;
    Ok(())
}

fn drop_removal(state: &State, record: i64) -> Result<(), HistoryError> {
    state
        .connection
        .execute("DELETE FROM file_removals WHERE rowid=?1", [record])?;
    Ok(())
}

pub(super) fn is_removal_waiting(state: &State) -> Result<bool, HistoryError> {
    let waiting: bool =
        state
            .connection
            .query_row("SELECT EXISTS(SELECT 1 FROM file_removals)", [], |row| {
                row.get(0)
            })?;
    Ok(waiting)
}

/// What a recording's end found at its file, all through one handle.
enum Ending {
    /// Its excerpt.
    Text(String),
    /// Blank all the way through.
    Blank,
    NotText(HistoryError),
    /// Nothing, in a folder that is there.
    Missing,
    /// A link, or a file of another user.
    Foreign,
}

/// The file at `transcript`, read through one open handle. `None` while it
/// can't be checked, as when its folder is away or can't be read.
fn read_ending(transcript: &Path) -> Option<Ending> {
    let mut file = match transcript_file::open(transcript) {
        Ok(file) => file,
        Err(OpenError::Missing) => {
            return transcript_file::is_gone(transcript).then_some(Ending::Missing);
        }
        Err(OpenError::Foreign) => return Some(Ending::Foreign),
        Err(OpenError::Io(error)) => {
            tracing::warn!(%error, "a transcript could not be checked");
            return None;
        }
    };
    match transcript_file::excerpt_of(&mut file) {
        Ok(Some(text)) => Some(Ending::Text(text)),
        Ok(None) => Some(Ending::Blank),
        Err(error @ (ReadError::NotText | ReadError::Open(_))) => {
            Some(Ending::NotText(HistoryError::from(error)))
        }
        Err(ReadError::Io(error)) => {
            tracing::warn!(%error, "a transcript could not be read");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::Arc;

    use aria_platform::sqlite::AtRest;

    use super::super::tests::{listed, opened, save, system_transcription, tamper};
    use super::super::{MAX_TRANSCRIPTS, now_ms, section_size, transcript_path};
    use super::*;
    use crate::history::TranscriptRead;
    use crate::transcript_file::{EXCERPT_BYTES, MAX_READ_BYTES};

    /// Records a session that wrote `file`, from its start to its end.
    fn recorded(
        store: &Store,
        file: &Path,
        created_at_ms: u64,
        duration_ms: u64,
        failure: Option<RpcFailure>,
    ) -> Result<Option<HistoryEntry>, HistoryError> {
        let begun = begin(store, file, created_at_ms)?;
        store.finish_transcript(begun.id, duration_ms, failure)
    }

    /// Begins an entry for `file`, already written, as a session begins it
    /// before making the file.
    fn begin(store: &Store, file: &Path, created_at_ms: u64) -> Result<HistoryEntry, HistoryError> {
        let aside = file.with_extension("aside");
        let present = fs::rename(file, &aside).is_ok();
        let begun = store.start_entry(file, created_at_ms);
        if present {
            fs::rename(&aside, file).unwrap();
        }
        let begun = begun?.expect("the name is free");
        assert_eq!(transcript_path(&begun.content), Some(file));
        Ok(begun)
    }

    /// Saves `file` as a transcript entry that just ended.
    fn linked(store: &Store, file: &Path) -> HistoryEntry {
        recorded(store, file, now_ms(), 0, None).unwrap().unwrap()
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

    fn reopened(directory: &tempfile::TempDir, store: Arc<Store>) -> Arc<Store> {
        drop(store);
        Store::open(directory.path(), &AtRest::for_tests()).unwrap()
    }

    /// A session started through history, with one paragraph written to its
    /// file.
    fn session(directory: &tempfile::TempDir, store: &Store) -> (PathBuf, u64) {
        let (mut file, path, id) = store
            .create_transcript(&directory.path().join("transcripts"))
            .unwrap();
        io::Write::write_all(&mut file, b"[00:00:00] Speaker 1: heard").unwrap();
        (path, id)
    }

    fn files_in(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn clearing_mac_audio_removes_its_files_and_keeps_dictations() {
        let (directory, store) = opened();
        save(&store, "dictated", HistoryStatus::Success, None);
        let file = transcript(directory.path(), "[00:00:01] heard");
        recorded(&store, &file, now_ms(), 1_000, None).unwrap();

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
    fn a_linked_transcript_keeps_its_first_kibibyte_and_get_reads_it_whole() {
        let (directory, store) = opened();
        let text = format!("{}é tail", "a".repeat(EXCERPT_BYTES - 1));
        let file = transcript(directory.path(), &text);

        let linked = recorded(&store, &file, now_ms(), 5_000, None)
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
    fn a_blank_transcript_leaves_no_entry_and_no_file() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "\n\n");

        assert!(
            recorded(&store, &file, now_ms(), 0, None)
                .unwrap()
                .is_none()
        );
        assert!(listed(&store).is_empty());
        assert!(!file.exists());
    }

    #[test]
    fn a_session_still_recording_when_history_reopens_ends_as_interrupted() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] cut short");
        let blank = directory
            .path()
            .join("transcripts/2026-11-04T11-00-00Z.txt");
        fs::write(&blank, "").unwrap();
        let begun = begin(&store, &file, now_ms()).unwrap();
        begin(&store, &blank, now_ms()).unwrap();
        assert_eq!(listed(&store)[1].duration_ms, None);

        let store = reopened(&directory, store);

        let entries = listed(&store);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, begun.id);
        assert_eq!(entries[0].text, "[00:00:01] cut short");
        assert!(entries[0].duration_ms.is_some());
        assert_eq!(entries[0].status, HistoryStatus::TranscriptionFailed);
        assert!(!blank.exists());
    }

    #[test]
    fn a_session_that_failed_is_linked_as_a_failed_transcription() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] partial");
        let failure = RpcFailure::new(
            RpcErrorCode::SystemTranscriptionCaptureFailed,
            "capture stopped",
        );

        let linked = recorded(&store, &file, now_ms(), 1_000, Some(failure.clone()))
            .unwrap()
            .unwrap();

        assert_eq!(linked.status, HistoryStatus::TranscriptionFailed);
        assert_eq!(linked.error, Some(failure));
    }

    #[test]
    fn get_reads_a_long_transcript_up_to_one_mebibyte_and_says_so() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), &"é".repeat(MAX_READ_BYTES as usize));
        let linked = linked(&store, &file);

        let got = store.get(linked.id).unwrap();

        assert_eq!(got.text.len(), MAX_READ_BYTES as usize);
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
    fn a_transcript_that_cant_be_removed_yet_goes_once_it_can() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        let linked = linked(&store, &file);
        let folder = file.parent().unwrap();
        fs::set_permissions(folder, fs::Permissions::from_mode(0o500)).unwrap();

        let deleted = store.delete(linked.id);
        let waited = file.exists();
        fs::set_permissions(folder, fs::Permissions::from_mode(0o700)).unwrap();
        store.maintenance().unwrap();

        assert!(deleted.unwrap());
        assert!(waited);
        assert!(listed(&store).is_empty());
        assert!(!file.exists());
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
    fn a_mac_audio_entry_never_expires_and_keeps_its_file() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        let linked = linked(&store, &file);
        tamper(
            &store,
            "UPDATE entries SET created=0 WHERE id=?1",
            [linked.id as i64],
        );

        store.maintenance().unwrap();

        assert_eq!(listed(&store).len(), 1);
        assert!(file.exists());
    }

    #[test]
    fn a_call_leaves_a_mac_audio_row_whose_file_is_gone_to_the_full_sweep() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        linked(&store, &file);
        fs::remove_file(&file).unwrap();

        assert_eq!(listed(&store).len(), 1);
        store.maintenance().unwrap();
        assert!(listed(&store).is_empty());
    }

    #[test]
    fn mac_audio_evicts_nothing_and_refuses_an_entry_past_its_most() {
        let (directory, store) = opened();
        let state = store.lock();
        let transaction = state.connection.unchecked_transaction().unwrap();
        for _ in 1..MAX_TRANSCRIPTS {
            transaction
                .execute(
                    "INSERT INTO entries(created,source,text,complete,status,language,own_voice,engine,section,transcript)
                     VALUES(?1,0,'kept',1,0,'',0,'',1,CAST('/no-folder/kept.txt' AS BLOB))",
                    [sql_integer(now_ms()).unwrap()],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(state);
        let file = transcript(directory.path(), "[00:00:01] heard");

        linked(&store, &file);
        let another = file.with_file_name("2026-11-04T11-00-00Z.txt");
        fs::copy(&file, &another).unwrap();
        let refused = recorded(&store, &another, now_ms(), 0, None);

        assert!(matches!(refused, Err(HistoryError::Full)));
        assert!(store.is_history_full().unwrap());
        let size = section_size(&store.lock(), HistorySection::SystemTranscription).unwrap();
        assert_eq!(size, MAX_TRANSCRIPTS);
        // A session then makes no file in the folder, and goes unsaved.
        let folder = directory.path().join("full");
        let session = store.create_transcript(&folder);
        assert!(matches!(
            session,
            Err(CreateError::History(HistoryError::Full))
        ));
        assert_eq!(fs::read_dir(&folder).map_or(0, |files| files.count()), 0);
    }

    #[test]
    fn a_session_whose_folder_is_away_keeps_its_entry() {
        let (directory, store) = opened();
        let away = directory.path().join("Unmounted/2026-11-04T10-00-00Z.txt");

        let ended = recorded(&store, &away, now_ms(), 1_000, None).unwrap();

        let entry = ended.expect("the entry stays for when its volume is back");
        assert_eq!(entry.duration_ms, Some(1_000));
        assert_eq!(store.transcripts().unwrap(), [(entry.id, away)]);
    }

    #[test]
    fn a_stop_whose_folder_cant_be_read_keeps_its_outcome_and_takes_its_excerpt_later() {
        let (directory, store) = opened();
        let (path, id) = session(&directory, &store);
        let folder = path.parent().unwrap().to_path_buf();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).unwrap();

        let ended = store.finish_transcript(id, 123_000, None);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        store.maintenance().unwrap();

        let ended = ended.unwrap().expect("the entry stays");
        assert_eq!(ended.duration_ms, Some(123_000));
        assert_eq!(ended.status, HistoryStatus::Success);
        assert_eq!(
            load(&store.lock(), id).unwrap().text,
            "[00:00:00] Speaker 1: heard"
        );
    }

    #[test]
    fn an_entry_still_recording_cant_be_deleted_or_cleared() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] live");
        let begun = begin(&store, &file, now_ms()).unwrap();

        let deleted = store.delete(begun.id);
        let cleared = store.clear(Some(HistorySection::SystemTranscription));

        assert!(matches!(deleted, Err(HistoryError::Busy)));
        assert!(matches!(cleared, Err(HistoryError::Busy)));
        assert!(file.exists());
        assert!(store.clear(Some(HistorySection::Dictation)).is_ok());
    }

    #[test]
    fn an_entry_whose_session_could_not_end_it_ends_on_the_next_full_sweep() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        let begun = begin(&store, &file, now_ms()).unwrap();
        store.maintenance().unwrap();
        assert_eq!(store.get(begun.id).unwrap().duration_ms, None);
        // Its session went without ending it.
        store.lock().recording.remove(&begun.id);

        store.maintenance().unwrap();

        let ended = store.get(begun.id).unwrap();
        assert!(ended.duration_ms.is_some());
        assert_eq!(ended.text, "[00:00:01] heard");
        assert_eq!(
            ended.error.map(|error| error.message),
            Some(TRANSCRIPTION_INTERRUPTED.to_owned())
        );
    }

    #[test]
    fn a_clear_with_the_drive_away_waits_and_removes_the_file_once_it_is_back() {
        let (directory, store) = opened();
        let file = transcript(directory.path(), "[00:00:01] heard");
        let drive = file.parent().unwrap().to_path_buf();
        linked(&store, &file);
        let away = directory.path().join("away");
        fs::rename(&drive, &away).unwrap();

        let cleared = store
            .clear(Some(HistorySection::SystemTranscription))
            .unwrap();
        fs::rename(&away, &drive).unwrap();
        store.maintenance().unwrap();

        assert_eq!(cleared.deleted_entries, 1);
        assert!(cleared.cleanup_pending);
        assert!(!file.exists());
    }

    #[test]
    fn a_session_end_that_cant_load_its_entry_stops_counting_it_as_running() {
        let (directory, store) = opened();
        let (_file, _path, id) = store
            .create_transcript(&directory.path().join("transcripts"))
            .unwrap();
        tamper(
            &store,
            "DELETE FROM entries WHERE id=?1",
            [sql_integer(id).unwrap()],
        );

        let finished = store.finish_transcript(id, 1_000, None);

        assert!(finished.is_err());
        assert!(!store.lock().recording.contains(&id));
    }

    #[test]
    fn a_recording_blank_only_at_its_start_keeps_its_file_and_entry() {
        let (directory, store) = opened();
        let (mut file, path, id) = store
            .create_transcript(&directory.path().join("transcripts"))
            .unwrap();
        let text = format!("{}[00:00:01] heard", " ".repeat(1_024));
        io::Write::write_all(&mut file, text.as_bytes()).unwrap();

        let finished = store.finish_transcript(id, 1_000, None).unwrap();

        assert!(finished.is_some());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert_eq!(store.transcripts().unwrap(), [(id, path)]);
    }

    #[test]
    fn a_blank_session_whose_folder_cant_be_read_goes_once_it_can() {
        let (directory, store) = opened();
        let (file, path, _) = store
            .create_transcript(&directory.path().join("transcripts"))
            .unwrap();
        drop(file);
        let folder = path.parent().unwrap().to_path_buf();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).unwrap();

        let store = reopened(&directory, store);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        let store = reopened(&directory, store);

        assert!(listed(&store).is_empty());
        assert!(files_in(&folder).is_empty());
    }

    #[test]
    fn a_session_rewritten_by_rename_still_goes_with_its_entry() {
        let (directory, store) = opened();
        let (path, id) = session(&directory, &store);
        let own = File::open(&path).unwrap();
        drop(transcript_file::rewrite(&path, &own, b"[00:00:00] Alex: heard").unwrap());
        let ended = store.finish_transcript(id, 1_000, None).unwrap();

        store.delete(id).unwrap();

        assert_eq!(ended.unwrap().text, "[00:00:00] Alex: heard");
        assert!(files_in(path.parent().unwrap()).is_empty());
    }

    #[test]
    fn an_entry_saved_before_this_store_is_read_whole_and_deleted_with_its_file() {
        let (directory, store) = opened();
        let text = format!("[00:00:01] {}", "heard ".repeat(1_000));
        let file = transcript(directory.path(), &text);
        let id = store
            .insert(system_transcription(&text[..1_024], &file), None)
            .unwrap()
            .id;

        let read = store.get(id).unwrap();
        store.delete(id).unwrap();

        assert_eq!(read.text, text);
        assert!(!file.exists());
    }
}
