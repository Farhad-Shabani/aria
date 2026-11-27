//! Aria's two transcript folders, and the file each new session writes.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use aria_settings::rpc::RpcErrorCode;

use super::TranscriptSaving;
use super::sink::TranscriptFile;
use super::unsaved::{self, UnsavedSession};
use crate::history::{CreateError, HistoryError, OwnedFiles, Store};
use crate::transcript_file::{self, is_within};

/// Aria's transcripts folder and unsaved folder under one home. Only Aria
/// writes in either.
///
/// `TranscriptFolders::at("/Users/me")` → `/Users/me/.aria/transcripts` and
/// `/Users/me/.aria/unsaved`.
pub struct TranscriptFolders {
    transcripts: PathBuf,
    unsaved: PathBuf,
}

/// How a session's file is kept once the session ends.
///
/// `Kept::History { id: 7, .. }` → the caller ends entry 7.
/// `Kept::HistoryOff(session)` → `session.finish()` removes the file if it
/// heard nothing.
pub enum Kept {
    /// History names the file from its start, as entry `id`.
    History { path: PathBuf, id: u64 },
    /// Unsaved, because **Save history** is off.
    HistoryOff(UnsavedSession),
    /// Unsaved, because Mac Audio history is full.
    HistoryFull(UnsavedSession),
    /// The caller's own file, which history never names.
    Output(PathBuf),
}

/// Why Aria can't use its transcript folders.
#[derive(Debug, thiserror::Error)]
pub enum FolderError {
    /// One folder is the other or inside it, so the unsaved list would offer
    /// files history names.
    #[error(
        "~/.aria/transcripts and ~/.aria/unsaved must be separate folders; remove the link between them"
    )]
    Joined,
    /// The check couldn't run, as while a linked drive is away.
    #[error(transparent)]
    Unchecked(io::Error),
}

/// Why [`TranscriptFolders::start`] opened no file.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Folders(#[from] FolderError),
    #[error(transparent)]
    Output(#[from] OutputError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(io::Error),
}

/// Why a file of the person's can't take a transcript: an `--output` file,
/// or where Save puts a copy.
#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("an output path can't contain . or ..")]
    Dotted,
    /// Delete or Discard would remove it.
    #[error(
        "an output file can't go in ~/.aria/transcripts or ~/.aria/unsaved; choose another folder"
    )]
    InAriaFolder,
    /// Deleting its entry, or a removal still pending, would remove it too.
    #[error("that file is a transcript history saved; choose another")]
    Saved,
    #[error("history can't check whether that file is a saved transcript; choose a new file")]
    Unchecked,
    #[error(transparent)]
    Io(io::Error),
}

impl From<&FolderError> for RpcErrorCode {
    fn from(error: &FolderError) -> Self {
        match error {
            FolderError::Joined => Self::InvalidRequest,
            FolderError::Unchecked(_) => Self::Internal,
        }
    }
}

impl From<&StartError> for RpcErrorCode {
    fn from(error: &StartError) -> Self {
        match error {
            StartError::Folders(error) => error.into(),
            StartError::Output(error) => error.into(),
            StartError::History(error) => error.into(),
            StartError::Io(_) => Self::Internal,
        }
    }
}

impl From<&OutputError> for RpcErrorCode {
    fn from(error: &OutputError) -> Self {
        match error {
            OutputError::Dotted | OutputError::InAriaFolder | OutputError::Saved => {
                Self::InvalidRequest
            }
            OutputError::Unchecked => Self::HistoryUnavailable,
            OutputError::Io(_) => Self::Internal,
        }
    }
}

/// Whether **Save history** is on as a session starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveHistory {
    On,
    Off,
}

impl TranscriptFolders {
    pub fn at(home: &Path) -> Self {
        Self {
            transcripts: home.join(aria_settings::TRANSCRIPTS_DIRECTORY),
            unsaved: home.join(unsaved::DIRECTORY),
        }
    }

    /// Opens the file a new session writes. `output` is the caller's own
    /// file, appended to. Without one, history names a new file in the
    /// transcripts folder from its start. With history off or full, the
    /// file waits in the unsaved folder.
    ///
    /// `start(None, Some(store), SaveHistory::On)` →
    /// `Kept::History { path: ~/.aria/transcripts/2026-11-26T10-00-00Z-1a2b3c4d.txt, id: 7 }`.
    pub fn start(
        &self,
        output: Option<PathBuf>,
        history: Option<Arc<Store>>,
        save: SaveHistory,
    ) -> Result<(TranscriptFile, Kept), StartError> {
        self.check()?;
        if let Some(output) = output {
            let owned = history.as_deref().map(Store::owned_files).transpose()?;
            self.check_destination(&output, owned.as_ref())?;
            let (sink, path) = TranscriptFile::append_to(output).map_err(StartError::Io)?;
            return Ok((sink, Kept::Output(path)));
        }
        if save == SaveHistory::Off {
            let (sink, session) = UnsavedSession::start(&self.unsaved).map_err(StartError::Io)?;
            return Ok((sink, Kept::HistoryOff(session)));
        }
        let store = history.ok_or(HistoryError::Unavailable)?;
        match TranscriptFile::create_recorded(&self.transcripts, &store) {
            Ok((sink, path, id)) => Ok((sink, Kept::History { path, id })),
            Err(CreateError::History(HistoryError::Full)) => {
                let (sink, session) =
                    UnsavedSession::start(&self.unsaved).map_err(StartError::Io)?;
                Ok((sink, Kept::HistoryFull(session)))
            }
            Err(CreateError::History(error)) => Err(StartError::History(error)),
            Err(CreateError::File(error)) => Err(StartError::Io(error)),
        }
    }

    /// Removes the staging files a crash left mid-rewrite in both folders.
    /// Call it before any session starts.
    pub fn remove_staging(&self) {
        transcript_file::remove_staging(&self.transcripts);
        transcript_file::remove_staging(&self.unsaved);
    }

    pub(crate) fn transcripts(&self) -> &Path {
        &self.transcripts
    }

    pub(crate) fn unsaved(&self) -> &Path {
        &self.unsaved
    }

    /// Makes both folders, private to their owner, so each has an identity
    /// to compare. Then checks that neither is the other or inside it,
    /// however a link spells either.
    pub(crate) fn check(&self) -> Result<(), FolderError> {
        let joined = self
            .create()
            .and_then(|()| {
                Ok(is_within(&self.transcripts, &self.unsaved)?
                    || is_within(&self.unsaved, &self.transcripts)?)
            })
            .map_err(FolderError::Unchecked)?;
        if joined {
            return Err(FolderError::Joined);
        }
        Ok(())
    }

    fn create(&self) -> io::Result<()> {
        for folder in [&self.transcripts, &self.unsaved] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(folder)?;
        }
        Ok(())
    }

    /// Refuses `destination` in either folder, however a link or letter
    /// case spells it. Also refuses a file history owns. With `owned` `None`
    /// nothing says which files history owns, so it accepts only a new file.
    pub(crate) fn check_destination(
        &self,
        destination: &Path,
        owned: Option<&OwnedFiles>,
    ) -> Result<(), OutputError> {
        if destination
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        {
            return Err(OutputError::Dotted);
        }
        let inside = is_within(destination, &self.transcripts)
            .and_then(|inside| Ok(inside || is_within(destination, &self.unsaved)?))
            .map_err(OutputError::Io)?;
        if inside {
            return Err(OutputError::InAriaFolder);
        }
        let saved = match owned {
            Some(owned) => owned.contains(destination),
            None if destination.symlink_metadata().is_ok() => {
                return Err(OutputError::Unchecked);
            }
            None => false,
        };
        if saved {
            return Err(OutputError::Saved);
        }
        Ok(())
    }
}

impl Kept {
    pub fn path(&self) -> &Path {
        match self {
            Self::History { path, .. } | Self::Output(path) => path,
            Self::HistoryOff(session) | Self::HistoryFull(session) => session.path(),
        }
    }

    pub fn saving(&self) -> TranscriptSaving {
        match self {
            Self::History { .. } => TranscriptSaving::History,
            Self::HistoryOff(_) => TranscriptSaving::HistoryOff,
            Self::HistoryFull(_) => TranscriptSaving::HistoryFull,
            Self::Output(_) => TranscriptSaving::Output,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use aria_platform::sqlite::AtRest;

    use super::*;
    use crate::history::{HistoryContent, HistoryEntry, HistoryQuery};

    fn opened(home: &Path) -> Arc<Store> {
        Store::open(&home.join("history"), &AtRest::for_tests()).unwrap()
    }

    fn listed(history: &Store) -> Vec<HistoryEntry> {
        history
            .list(&HistoryQuery {
                section: None,
                query: String::new(),
                before_id: None,
                limit: 10,
                collapse_chats: false,
            })
            .unwrap()
            .entries
    }

    fn saved_in(history: &Store, folder: &Path) -> PathBuf {
        let (mut file, path, id) = history.create_transcript(folder).unwrap();
        io::Write::write_all(&mut file, b"[00:00:01] heard").unwrap();
        history.finish_transcript(id, 1_000, None).unwrap();
        path
    }

    fn code(started: &Result<(TranscriptFile, Kept), StartError>) -> Option<RpcErrorCode> {
        started.as_ref().err().map(RpcErrorCode::from)
    }

    #[test]
    fn with_history_on_the_entry_names_the_file_from_the_start() {
        let home = tempfile::tempdir().unwrap();
        let history = opened(home.path());
        let transcripts = home.path().join(".aria/transcripts");

        let (_sink, kept) = TranscriptFolders::at(home.path())
            .start(None, Some(Arc::clone(&history)), SaveHistory::On)
            .unwrap();
        let (transcript, saving) = (kept.path().to_path_buf(), kept.saving());

        assert_eq!(saving, TranscriptSaving::History);
        assert_eq!(transcript.parent(), Some(transcripts.as_path()));
        let entries = listed(&history);
        assert!(matches!(
            &entries[..],
            [entry] if matches!(&entry.content, HistoryContent::SystemTranscription { transcript: named, .. } if named == &transcript)
        ));
    }

    #[test]
    fn with_history_off_the_file_waits_unsaved_and_gets_no_entry() {
        let home = tempfile::tempdir().unwrap();
        let history = opened(home.path());
        let folders = TranscriptFolders::at(home.path());

        let (_sink, kept) = folders
            .start(None, Some(Arc::clone(&history)), SaveHistory::Off)
            .unwrap();
        let (transcript, saving) = (kept.path().to_path_buf(), kept.saving());

        assert_eq!(saving, TranscriptSaving::HistoryOff);
        assert_eq!(transcript.parent(), Some(folders.unsaved()));
        assert_eq!(fs::read_dir(folders.transcripts()).unwrap().count(), 0);
        assert!(listed(&history).is_empty());
    }

    #[test]
    fn a_session_history_cant_record_doesnt_start_or_leave_a_file() {
        let home = tempfile::tempdir().unwrap();
        let folders = TranscriptFolders::at(home.path());

        let refused = folders.start(None, None, SaveHistory::On);

        assert_eq!(code(&refused), Some(RpcErrorCode::HistoryUnavailable));
        assert_eq!(
            fs::read_dir(folders.transcripts()).map_or(0, |files| files.count()),
            0
        );
    }

    #[test]
    fn a_file_the_caller_named_gets_no_entry() {
        let home = tempfile::tempdir().unwrap();
        let history = opened(home.path());
        let output = home.path().join("meeting.txt");

        let (_sink, kept) = TranscriptFolders::at(home.path())
            .start(
                Some(output.clone()),
                Some(Arc::clone(&history)),
                SaveHistory::On,
            )
            .unwrap();
        let (transcript, saving) = (kept.path().to_path_buf(), kept.saving());

        assert_eq!(saving, TranscriptSaving::Output);
        assert_eq!(transcript, output);
        assert!(listed(&history).is_empty());
    }

    #[test]
    fn no_output_file_goes_in_a_folder_only_aria_writes() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let alias = home.join("alias");
        std::os::unix::fs::symlink(home.join(".aria/transcripts"), &alias).unwrap();

        for inside in [
            home.join(".aria/transcripts/meeting.txt"),
            home.join(".aria/unsaved/meeting.txt"),
            home.join(".aria/unsaved/new/meeting.txt"),
            alias.join("meeting.txt"),
            home.join("new/../.aria/unsaved/meeting.txt"),
        ] {
            let refused =
                TranscriptFolders::at(home).start(Some(inside.clone()), None, SaveHistory::On);
            assert_eq!(
                code(&refused),
                Some(RpcErrorCode::InvalidRequest),
                "{inside:?}"
            );
        }
        let outside = home.join("Meetings/meeting.txt");
        assert!(
            TranscriptFolders::at(home)
                .start(Some(outside), None, SaveHistory::On)
                .is_ok()
        );
    }

    #[test]
    fn linked_aria_folders_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        fs::create_dir_all(home.join(".aria/transcripts")).unwrap();
        std::os::unix::fs::symlink(home.join(".aria/transcripts"), home.join(".aria/unsaved"))
            .unwrap();

        let refused = TranscriptFolders::at(home).check();

        assert!(
            matches!(refused, Err(error) if RpcErrorCode::from(&error) == RpcErrorCode::InvalidRequest)
        );
        let apart = tempfile::tempdir().unwrap();
        assert!(TranscriptFolders::at(apart.path()).check().is_ok());
    }

    #[test]
    fn an_output_file_history_names_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let history = opened(home.path());
        let saved = saved_in(&history, &home.path().join("Meetings"));
        let folders = TranscriptFolders::at(home.path());

        let owned = folders.start(Some(saved), Some(Arc::clone(&history)), SaveHistory::On);

        assert_eq!(code(&owned), Some(RpcErrorCode::InvalidRequest));
        let other = home.path().join("Meetings/other.txt");
        assert!(
            folders
                .start(Some(other), Some(history), SaveHistory::On)
                .is_ok()
        );
    }

    #[test]
    fn an_output_file_waiting_to_be_removed_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let history = opened(home.path());
        let meetings = home.path().join("Meetings");
        let saved = saved_in(&history, &meetings);
        let id = listed(&history)[0].id;
        fs::set_permissions(&meetings, fs::Permissions::from_mode(0o500)).unwrap();
        // The entry is deleted, and its file waits.
        history.delete(id).unwrap();

        let owned = TranscriptFolders::at(home.path()).start(
            Some(saved.clone()),
            Some(Arc::clone(&history)),
            SaveHistory::On,
        );
        fs::set_permissions(&meetings, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(listed(&history).is_empty());
        assert!(saved.exists());
        assert_eq!(code(&owned), Some(RpcErrorCode::InvalidRequest));
    }
}
