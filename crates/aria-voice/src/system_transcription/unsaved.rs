//! Transcripts no history entry names yet: sessions run with history off or
//! full. Each waits in Aria's own `~/.aria/unsaved` folder until the person
//! saves or discards it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use aria_primitives::error_chain;
use aria_settings::rpc::RpcErrorCode;

use super::{FolderError, TranscriptFile, TranscriptFolders};
use crate::history::{HistoryError, OwnedFiles, Store, TranscriptRead};
use crate::transcript_file::{self, OpenError, ReadError, Removal};

/// Under the home folder, beside Aria's other data.
pub(crate) const DIRECTORY: &str = ".aria/unsaved";

/// The unsaved folder, with what history owns there. No file history owns
/// is offered, read or removed.
///
/// `open(home, Some(store))` → every method. `open(home, None)` → `list`
/// and `get` only, and `list` removes nothing.
pub struct UnsavedFolder {
    folders: TranscriptFolders,
    /// `None` while history can't open.
    owned: Option<OwnedFiles>,
    history_full: bool,
}

impl UnsavedFolder {
    pub fn open(home: &Path, history: Option<&Store>) -> Result<Self, UnsavedError> {
        let owned = history.map(Store::owned_files).transpose()?;
        let history_full = history
            .map(Store::is_history_full)
            .transpose()?
            .unwrap_or(false);
        if history.is_none() {
            tracing::warn!("unsaved transcripts reached without history");
        }
        Ok(Self {
            folders: TranscriptFolders::at(home),
            owned,
            history_full,
        })
    }

    /// Every unsaved transcript but `running`'s, oldest first, once blank
    /// ones are gone: a session a crash ended before it heard anything
    /// leaves nothing to offer. A folder check that can't run, as while a
    /// linked drive is away, keeps blank ones.
    ///
    /// `list(Some(running))` → `[2026-11-25T10-00-00Z-1a2b3c4d.txt]`, without the
    /// running session's file or a blank one.
    pub fn list(&self, running: Option<&Path>) -> Result<UnsavedTranscriptList, UnsavedError> {
        let apart = match self.folders.check() {
            Ok(()) => true,
            Err(FolderError::Unchecked(error)) => {
                tracing::warn!(error = %error_chain(&error), "unsaved transcripts listed without a folder check");
                false
            }
            Err(error) => return Err(error.into()),
        };
        if apart {
            self.remove_blank(running)?;
        }
        Ok(UnsavedTranscriptList {
            transcripts: self.transcripts(running)?,
            history_full: self.history_full,
            transcripts_dir: self.folders.transcripts().to_path_buf(),
            unsaved_dir: self.folders.unsaved().to_path_buf(),
        })
    }

    /// The unsaved transcript called `name`, unless history owns its file.
    ///
    /// `get("2026-11-25T10-00-00Z-1a2b3c4d.txt")` → that transcript, or `Missing`
    /// once it's gone.
    pub fn get(&self, name: &str) -> Result<UnsavedTranscript, UnsavedError> {
        let started_at_ms =
            transcript_file::session_start(name).ok_or(UnsavedError::InvalidName)?;
        let path = self.folders.unsaved().join(name);
        if self
            .owned
            .as_ref()
            .is_some_and(|owned| owned.contains(&path))
        {
            return Err(UnsavedError::Owned);
        }
        let metadata = transcript_file::inspect(&path)?;
        let duration_ms = transcript_file::modified_ms(&metadata)
            .map_or(0, |modified| modified.saturating_sub(started_at_ms));
        Ok(UnsavedTranscript {
            name: name.to_owned(),
            path,
            started_at_ms,
            duration_ms,
        })
    }

    /// Discards `name`, unless `running` still writes it. Refused while
    /// history can't open, since only history knows the files it names.
    pub fn discard(&self, name: &str, running: Option<&Path>) -> Result<(), UnsavedError> {
        self.folders.check()?;
        if self.owned.is_none() {
            return Err(UnsavedError::History(HistoryError::Unavailable));
        }
        match self.get(name) {
            Ok(transcript) if running == Some(transcript.path.as_path()) => Err(UnsavedError::Busy),
            Ok(transcript) => transcript.discard(),
            Err(UnsavedError::Missing) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Every unsaved transcript but `running`'s and those history owns,
    /// oldest first.
    fn transcripts(&self, running: Option<&Path>) -> Result<Vec<UnsavedTranscript>, UnsavedError> {
        let entries = match fs::read_dir(self.folders.unsaved()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(UnsavedError::Io(error)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(UnsavedError::Io)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let transcript = match self.get(&name) {
                Ok(transcript) => transcript,
                Err(
                    UnsavedError::InvalidName
                    | UnsavedError::Missing
                    | UnsavedError::Foreign
                    | UnsavedError::Owned,
                ) => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if running != Some(transcript.path.as_path()) {
                found.push(transcript);
            }
        }
        found.sort_by(|a, b| (a.started_at_ms, &a.name).cmp(&(b.started_at_ms, &b.name)));
        Ok(found)
    }

    /// Removes every blank unsaved transcript but `running`'s. Removes
    /// nothing while history can't say what it owns.
    fn remove_blank(&self, running: Option<&Path>) -> Result<(), UnsavedError> {
        if self.owned.is_none() {
            return Ok(());
        }
        for transcript in self.transcripts(running)? {
            remove_if_blank(&transcript.path)?;
        }
        Ok(())
    }
}

/// One file in the unsaved folder: a regular file of ours, named as a session
/// names its file. Only [`UnsavedFolder`] finds one.
#[derive(Debug, PartialEq, Eq)]
pub struct UnsavedTranscript {
    name: String,
    path: PathBuf,
    started_at_ms: u64,
    duration_ms: u64,
}

impl UnsavedTranscript {
    /// The file's name, which says when the session started.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    /// How long the session wrote to it: until its end, unless it still runs.
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    pub fn read(&self) -> Result<UnsavedText, UnsavedError> {
        let mut file = transcript_file::open(&self.path)?;
        let (text, read) = TranscriptRead::of(transcript_file::read_file(&mut file)?);
        Ok(UnsavedText { text, read })
    }

    fn discard(self) -> Result<(), UnsavedError> {
        match transcript_file::unlink(&self.path).map_err(UnsavedError::Io)? {
            Removal::Removed | Removal::Missing | Removal::Unavailable => Ok(()),
            Removal::Foreign => Err(UnsavedError::Foreign),
        }
    }
}

pub struct UnsavedTranscriptList {
    pub transcripts: Vec<UnsavedTranscript>,
    /// Whether Mac Audio history is full, which is why a session with
    /// history on runs unsaved. False while history can't open.
    pub history_full: bool,
    /// Aria's folders, which a copy must stay out of.
    pub transcripts_dir: PathBuf,
    pub unsaved_dir: PathBuf,
}

/// An unsaved transcript's text, as much as one read returns.
pub struct UnsavedText {
    pub text: String,
    /// `Whole` or `Truncated`.
    pub read: TranscriptRead,
}

/// The file a session writes in the unsaved folder.
pub struct UnsavedSession {
    path: PathBuf,
}

impl UnsavedSession {
    /// A new file in `folder` for a session no entry names.
    pub(crate) fn start(folder: &Path) -> io::Result<(TranscriptFile, Self)> {
        let (sink, path) = TranscriptFile::create(folder)?;
        Ok((sink, Self { path }))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the session's file when it heard nothing.
    pub fn finish(self) -> Result<(), UnsavedError> {
        remove_if_blank(&self.path)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UnsavedError {
    #[error("that isn't an unsaved transcript's name")]
    InvalidName,
    #[error("the unsaved transcript is gone")]
    Missing,
    #[error("the unsaved transcript isn't a regular file of yours")]
    Foreign,
    #[error("the unsaved transcript isn't text")]
    NotText,
    #[error("the transcript is still being written")]
    Busy,
    #[error("history owns that transcript's file")]
    Owned,
    #[error(transparent)]
    Folders(#[from] FolderError),
    #[error("the unsaved transcript couldn't be read or moved")]
    Io(#[source] io::Error),
    #[error(transparent)]
    History(#[from] HistoryError),
}

impl From<&UnsavedError> for RpcErrorCode {
    fn from(error: &UnsavedError) -> Self {
        match error {
            UnsavedError::InvalidName | UnsavedError::Owned => Self::InvalidRequest,
            UnsavedError::Missing => Self::HistoryMissing,
            UnsavedError::Foreign => Self::PermissionDenied,
            UnsavedError::NotText => Self::HistoryTranscriptUnreadable,
            UnsavedError::Busy => Self::Busy,
            UnsavedError::Folders(error) => error.into(),
            UnsavedError::Io(_) => Self::Internal,
            UnsavedError::History(error) => error.into(),
        }
    }
}

impl From<OpenError> for UnsavedError {
    fn from(error: OpenError) -> Self {
        match error {
            OpenError::Missing => Self::Missing,
            OpenError::Foreign => Self::Foreign,
            OpenError::Io(error) => Self::Io(error),
        }
    }
}

impl From<ReadError> for UnsavedError {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::Open(error) => error.into(),
            ReadError::NotText => Self::NotText,
            ReadError::Io(error) => Self::Io(error),
        }
    }
}

/// Removes the file at `path` when it holds only whitespace.
fn remove_if_blank(path: &Path) -> Result<(), UnsavedError> {
    let mut file = match transcript_file::open(path) {
        Ok(file) => file,
        Err(OpenError::Missing) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !transcript_file::is_blank(&mut file).map_err(UnsavedError::Io)? {
        return Ok(());
    }
    drop(file);
    transcript_file::unlink(path).map_err(UnsavedError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use aria_platform::sqlite::AtRest;

    use super::*;

    /// The unsaved folder under `home`, without history.
    fn folder(home: &Path, owned: Option<OwnedFiles>) -> UnsavedFolder {
        UnsavedFolder {
            folders: TranscriptFolders::at(home),
            owned,
            history_full: false,
        }
    }

    fn written(folder: &UnsavedFolder, name: &str, text: &str) -> PathBuf {
        fs::create_dir_all(folder.folders.unsaved()).unwrap();
        let path = folder.folders.unsaved().join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[test]
    fn blank_ones_go_and_the_running_file_is_neither_listed_nor_removed() {
        let home = tempfile::tempdir().unwrap();
        let folder = folder(home.path(), Some(OwnedFiles::default()));
        let kept = written(
            &folder,
            "2026-11-25T10-00-00Z-0000000a.txt",
            "[00:00:01] heard",
        );
        let running = written(
            &folder,
            "2026-11-25T11-00-00Z-0000000b.txt",
            "[00:00:01] live",
        );
        let blank = written(&folder, "2026-11-25T09-00-00Z-0000000c.txt", " \n");
        let starting = written(&folder, "2026-11-25T12-00-00Z-0000000d.txt", "");

        folder.remove_blank(Some(&starting)).unwrap();
        let listed = folder.transcripts(Some(&running)).unwrap();

        assert_eq!(
            listed
                .iter()
                .map(UnsavedTranscript::path)
                .collect::<Vec<_>>(),
            [kept.as_path(), starting.as_path()]
        );
        assert!(!blank.exists());
    }

    #[test]
    fn the_list_puts_sessions_in_start_order() {
        let home = tempfile::tempdir().unwrap();
        let folder = folder(home.path(), Some(OwnedFiles::default()));
        let last = written(
            &folder,
            "2026-11-25T10-00-02Z-00000001.txt",
            "[00:00:01] last",
        );
        let middle = written(
            &folder,
            "2026-11-25T10-00-01Z-ffffffff.txt",
            "[00:00:01] middle",
        );
        let first = written(
            &folder,
            "2026-11-25T10-00-00Z-80000000.txt",
            "[00:00:01] first",
        );

        let listed = folder.transcripts(None).unwrap();

        assert_eq!(
            listed
                .iter()
                .map(UnsavedTranscript::path)
                .collect::<Vec<_>>(),
            [first.as_path(), middle.as_path(), last.as_path()]
        );
    }

    #[test]
    fn a_name_discarded_never_reaches_a_later_session() {
        let home = tempfile::tempdir().unwrap();
        let folder = folder(home.path(), Some(OwnedFiles::default()));
        let (sink, first) = UnsavedSession::start(folder.folders.unsaved()).unwrap();
        drop(sink);
        fs::write(first.path(), "[00:00:01] first").unwrap();
        let stale = first
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        folder.discard(&stale, None).unwrap();

        let (sink, second) = UnsavedSession::start(folder.folders.unsaved()).unwrap();
        drop(sink);
        fs::write(second.path(), "[00:00:01] second").unwrap();
        let read = folder.get(&stale);
        folder.discard(&stale, None).unwrap();

        assert_ne!(second.path(), first.path());
        assert!(matches!(read, Err(UnsavedError::Missing)));
        assert_eq!(
            fs::read_to_string(second.path()).unwrap(),
            "[00:00:01] second"
        );
    }

    #[test]
    fn a_file_history_owns_is_never_offered_however_its_folder_is_spelled() {
        let home = tempfile::tempdir().unwrap();
        let folder = folder(home.path(), None);
        let owed = written(
            &folder,
            "2026-11-25T10-00-00Z-0000000a.txt",
            "[00:00:01] heard",
        );
        let older = home.path().join("Older Folder");
        std::os::unix::fs::symlink(folder.folders.unsaved(), &older).unwrap();

        for owned in [owed, older.join("2026-11-25T10-00-00z-0000000A.TXT")] {
            let listed = self::folder(
                home.path(),
                Some(OwnedFiles::locate(std::collections::HashSet::from([
                    owned.clone()
                ]))),
            )
            .transcripts(None)
            .unwrap();

            assert!(listed.is_empty(), "{}", owned.display());
        }
    }

    #[test]
    fn discarding_leaves_nothing_and_a_blank_session_leaves_no_file() {
        let home = tempfile::tempdir().unwrap();
        let folder = folder(home.path(), Some(OwnedFiles::default()));
        written(
            &folder,
            "2026-11-25T10-00-00Z-0000000a.txt",
            "[00:00:01] heard",
        );
        let (sink, session) = UnsavedSession::start(folder.folders.unsaved()).unwrap();
        drop(sink);

        folder
            .discard("2026-11-25T10-00-00Z-0000000a.txt", None)
            .unwrap();
        session.finish().unwrap();

        assert_eq!(fs::read_dir(folder.folders.unsaved()).unwrap().count(), 0);
    }

    #[test]
    fn a_session_end_keeps_a_file_blank_only_at_its_start() {
        let home = tempfile::tempdir().unwrap();
        let folder = folder(home.path(), None);
        let (sink, session) = UnsavedSession::start(folder.folders.unsaved()).unwrap();
        drop(sink);
        let blank_start = format!("{}text past the excerpt", " ".repeat(1_024));
        fs::write(session.path(), &blank_start).unwrap();

        let path = session.path().to_path_buf();
        session.finish().unwrap();

        assert!(path.exists());
    }

    #[test]
    fn a_file_history_names_in_the_unsaved_folder_is_never_listed_or_discarded() {
        for older_name in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let history = Store::open(&home.path().join("history"), &AtRest::for_tests()).unwrap();
            let unsaved = home.path().join(DIRECTORY);
            fs::create_dir_all(&unsaved).unwrap();
            // An older Aria's folder, set to a link to what is now the unsaved one.
            let saved_folder = if older_name {
                let older = home.path().join("Older Transcripts");
                std::os::unix::fs::symlink(&unsaved, &older).unwrap();
                older
            } else {
                unsaved.clone()
            };
            let (mut file, saved, id) = history.create_transcript(&saved_folder).unwrap();
            io::Write::write_all(&mut file, b"[00:00:01] heard").unwrap();
            history.finish_transcript(id, 1_000, None).unwrap();
            let name = saved.file_name().unwrap().to_str().unwrap().to_owned();

            let folder = UnsavedFolder::open(home.path(), Some(&history)).unwrap();
            let listed = folder.list(None).unwrap();
            let discarded = folder.discard(&name, None);

            assert!(listed.transcripts.is_empty(), "{older_name}");
            assert!(discarded.is_err(), "{older_name}");
            assert!(saved.exists(), "{older_name}");
        }
    }

    #[test]
    fn a_folder_check_that_cant_run_lists_unsaved_files_but_removes_none() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        fs::create_dir_all(home.join(".aria")).unwrap();
        // A link to a drive that's away.
        std::os::unix::fs::symlink(home.join("Away/Drive"), home.join(".aria/transcripts"))
            .unwrap();
        let history = Store::open(&home.join("history"), &AtRest::for_tests()).unwrap();
        let unsaved = home.join(DIRECTORY);
        fs::create_dir_all(&unsaved).unwrap();
        let heard = unsaved.join("2026-11-25T10-00-00Z-0000000a.txt");
        let blank = unsaved.join("2026-11-25T11-00-00Z-0000000b.txt");
        fs::write(&heard, "[00:00:01] heard").unwrap();
        fs::write(&blank, "").unwrap();

        let check = TranscriptFolders::at(home).check();
        let folder = UnsavedFolder::open(home, Some(&history)).unwrap();
        let listed = folder.list(None).unwrap();

        assert!(matches!(check, Err(FolderError::Unchecked(_))));
        assert_eq!(listed.transcripts.len(), 2);
        assert!(heard.exists() && blank.exists());
        assert!(
            folder
                .discard("2026-11-25T11-00-00Z-0000000b.txt", None)
                .is_err()
        );
    }
}
