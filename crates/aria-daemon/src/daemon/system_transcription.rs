//! Daemon ownership of the system transcription.
//!
//! A mutexed slot rather than an actor: a system transcription must outlive the
//! RPC that started it, so nothing here ties cancellation to a request.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use aria_kit::cancel::CancelToken;

use aria_kit::audio::{RecordingReader, Stopper};
use aria_primitives::error_chain;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_voice::config::{SpeakerConfig, SystemTranscriptionConfig};
use aria_voice::history::TRANSCRIPTION_INTERRUPTED;
use aria_voice::system_transcription::unsaved::{
    UnsavedFolder, UnsavedText, UnsavedTranscriptList,
};
use aria_voice::system_transcription::{
    Kept, SaveHistory, SystemAudioCapture, SystemTranscriptionError,
    SystemTranscriptionEventEmitter, SystemTranscriptionSnapshot, SystemTranscriptionStage,
    SystemTranscriptionStarted, SystemTranscriptionStopped, TranscriptFile, TranscriptFolders,
    TranscriptSaving, run_session,
};

use super::history::HistoryStore;
use super::host::DaemonLifetime;

pub(super) struct SystemTranscriptionState {
    lifetime: DaemonLifetime,
    home: PathBuf,
    cache: Arc<aria_settings::SettingsCache>,
    settings: Arc<aria_settings::SettingsStore>,
    events: SystemTranscriptionEventEmitter,
    vocabulary: Arc<super::vocabulary::Runtime>,
    history: Arc<HistoryStore>,
    active: Mutex<Slot>,
    /// Taken before `active` and held while the unsaved folder removes files,
    /// and by Start while it makes a session's file. So a listing never
    /// removes a file a session is about to write.
    files: Mutex<()>,
}

enum Slot {
    Idle,
    Running(ActiveSystemTranscription),
    /// The session ended on its own after Start had answered. Status
    /// reports why when it failed.
    Ended {
        transcript: PathBuf,
        error: Option<RpcFailure>,
    },
}

struct ActiveSystemTranscription {
    stopper: Stopper,
    /// Checked around a model load, so a stop during the load ends the
    /// session once it returns instead of starting transcription.
    cancel: CancelToken,
    thread: JoinHandle<Result<(), SystemTranscriptionError>>,
    transcript: PathBuf,
    saving: TranscriptSaving,
}

impl SystemTranscriptionState {
    pub(super) fn new(
        lifetime: DaemonLifetime,
        home: PathBuf,
        cache: Arc<aria_settings::SettingsCache>,
        settings: Arc<aria_settings::SettingsStore>,
        events: SystemTranscriptionEventEmitter,
        vocabulary: Arc<super::vocabulary::Runtime>,
        history: Arc<HistoryStore>,
    ) -> Self {
        // No session runs yet, so any staging file is a crash's.
        TranscriptFolders::at(&home).remove_staging();
        Self {
            lifetime,
            home,
            cache,
            settings,
            events,
            vocabulary,
            history,
            active: Mutex::new(Slot::Idle),
            files: Mutex::new(()),
        }
    }

    /// Accepts a session; capture and model initialization run on worker threads.
    /// `output` overrides the configured transcripts directory with an exact
    /// file for this session.
    pub(super) fn start(
        &self,
        output: Option<PathBuf>,
    ) -> Result<SystemTranscriptionStarted, RpcFailure> {
        if self.lifetime == DaemonLifetime::OnDemand {
            return Err(RpcFailure::new(
                RpcErrorCode::Unsupported,
                "transcription needs the persistent aria daemon; start `aria daemon`",
            ));
        }
        let _files = self.lock_files();
        let mut slot = self.lock();
        harvest(&mut slot);
        match &*slot {
            Slot::Running(active) => {
                return Ok(SystemTranscriptionStarted {
                    already_running: true,
                    transcript: active.transcript.clone(),
                    saving: active.saving,
                });
            }
            Slot::Idle | Slot::Ended { .. } => {}
        }
        // `files` keeps another Start out until this one fills the slot.
        drop(slot);

        let internal = |error: aria_settings::config::ConfigError| {
            RpcFailure::new(RpcErrorCode::Internal, error_chain(&error))
        };
        let settings = self.cache.effective();
        let config = SystemTranscriptionConfig::load(&settings, &self.home).map_err(internal)?;
        let speaker = SpeakerConfig::load(&settings.speaker, &self.home);
        // An unreadable settings file refuses the session rather than guess whether to save
        // history.
        let save = if self
            .settings
            .snapshot()
            .map_err(|error| RpcFailure::from(&error))?
            .settings
            .history_enabled
        {
            SaveHistory::On
        } else {
            SaveHistory::Off
        };
        let (sink, kept) = open_transcript(&self.home, &self.history, output, save)?;
        let (transcript, saving) = (kept.path.clone(), kept.saving);
        let cancel = CancelToken::new();
        // System transcription has no focused project: the user's own word list only. The
        // snapshot is let go at once, so background vocabulary work does
        // not wait on a session that runs for hours.
        let vocabulary = Arc::clone(&self.vocabulary.snapshot(None).matcher);
        // The worker runs only a session Start sends it. A spawn that fails
        // drops the kept transcript, which ends it as interrupted.
        let (send, received) = mpsc::sync_channel::<Session>(1);
        let thread = thread::Builder::new()
            .name("aria-transcribe".into())
            .spawn({
                let events = self.events.clone();
                let cancel = cancel.clone();
                move || {
                    // Start ended without sending one, so no session begins.
                    let Ok(session) = received.recv() else {
                        return Ok(());
                    };
                    let result = run_session(
                        &config,
                        &speaker,
                        &vocabulary,
                        &events,
                        session.capture,
                        session.recording,
                        session.sink,
                        &cancel,
                    );
                    finish(&result, session.kept, &events);
                    result
                }
            })
            .map_err(|error| RpcFailure::new(RpcErrorCode::Internal, error_chain(&error)))?;
        // Capture starts only while `active` is held and fills the slot
        // before release, so a Stop never finds it running unseen.
        let mut slot = self.lock();
        debug_assert!(
            !matches!(*slot, Slot::Running(_)),
            "`files` keeps another Start out"
        );
        let (capture, recording) = match SystemAudioCapture::spawn() {
            Ok(spawned) => spawned,
            Err(error) => {
                drop(slot);
                drop(send);
                // The worker had nothing to run, so it has ended.
                let _ = thread.join();
                let failure = RpcFailure::from(&error);
                drop(sink);
                kept.finish(Some(failure.clone()));
                return Err(failure);
            }
        };
        *slot = Slot::Running(ActiveSystemTranscription {
            stopper: capture.stopper(),
            cancel,
            thread,
            transcript: transcript.clone(),
            saving,
        });
        // A worker that already ended drops the session, which stops its
        // capture.
        let _ = send.send(Session {
            capture,
            recording,
            sink,
            kept,
        });
        drop(slot);
        tracing::info!(transcript = %transcript.display(), "system transcription started");
        Ok(SystemTranscriptionStarted {
            already_running: false,
            transcript,
            saving,
        })
    }

    /// Stops the running session, reporting a session that failed instead of
    /// stopping cleanly as that failure.
    pub(super) fn stop(&self) -> Result<SystemTranscriptionStopped, RpcFailure> {
        // The lock is held through the join so a concurrent Start cannot
        // open a second capture while this one winds down.
        let mut slot = self.lock();
        let previous = std::mem::replace(&mut *slot, Slot::Idle);
        match previous {
            Slot::Idle => Ok(SystemTranscriptionStopped {
                was_running: false,
                transcript: PathBuf::new(),
            }),
            Slot::Ended { transcript, .. } => Ok(SystemTranscriptionStopped {
                was_running: false,
                transcript,
            }),
            Slot::Running(active) => {
                let transcript = active.transcript.clone();
                join(active).map(|()| {
                    tracing::info!(transcript = %transcript.display(), "system transcription stopped");
                    SystemTranscriptionStopped {
                        was_running: true,
                        transcript,
                    }
                })
            }
        }
    }

    pub(super) fn status(&self) -> SystemTranscriptionSnapshot {
        let mut slot = self.lock();
        harvest(&mut slot);
        match &*slot {
            Slot::Idle => SystemTranscriptionSnapshot {
                running: false,
                transcript: PathBuf::new(),
                error: None,
            },
            Slot::Running(active) => SystemTranscriptionSnapshot {
                running: true,
                transcript: active.transcript.clone(),
                error: None,
            },
            Slot::Ended { transcript, error } => SystemTranscriptionSnapshot {
                running: false,
                transcript: transcript.clone(),
                error: error.as_ref().map(ToString::to_string),
            },
        }
    }

    /// The unsaved transcripts but the running session's.
    pub(super) fn unsaved(&self) -> Result<UnsavedTranscriptList, RpcFailure> {
        let _files = self.lock_files();
        let running = self.running();
        self.unsaved_folder()?
            .list(running.as_deref())
            .map_err(|error| RpcFailure::from(&error))
    }

    pub(super) fn read_unsaved(&self, name: &str) -> Result<UnsavedText, RpcFailure> {
        self.unsaved_folder()?
            .get(name)
            .and_then(|transcript| transcript.read())
            .map_err(|error| RpcFailure::from(&error))
    }

    pub(super) fn discard_unsaved(&self, name: &str) -> Result<(), RpcFailure> {
        let _files = self.lock_files();
        let running = self.running();
        self.unsaved_folder()?
            .discard(name, running.as_deref())
            .map_err(|error| RpcFailure::from(&error))
    }

    fn unsaved_folder(&self) -> Result<UnsavedFolder, RpcFailure> {
        UnsavedFolder::open(&self.home, self.history.storage().ok().as_deref())
            .map_err(|error| RpcFailure::from(&error))
    }

    fn running(&self) -> Option<PathBuf> {
        let mut slot = self.lock();
        harvest(&mut slot);
        running(&slot).map(Path::to_path_buf)
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_files(&self) -> MutexGuard<'_, ()> {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Daemon shutdown ends capture and preserves the transcript tail.
impl Drop for SystemTranscriptionState {
    fn drop(&mut self) {
        let previous = std::mem::replace(&mut *self.lock(), Slot::Idle);
        if let Slot::Running(active) = previous
            && let Err(failure) = join(active)
        {
            tracing::warn!(
                code = failure.code.label(),
                message = failure.message,
                "system transcription failed during daemon shutdown"
            );
        }
    }
}

/// What the worker runs, sent once the slot holds the capture's stopper.
struct Session {
    capture: SystemAudioCapture,
    recording: RecordingReader,
    sink: TranscriptFile,
    kept: KeptTranscript,
}

/// Opens the session's file. Its entry, if any, ends through `history`.
fn open_transcript(
    home: &Path,
    history: &Arc<HistoryStore>,
    output: Option<PathBuf>,
    save: SaveHistory,
) -> Result<(TranscriptFile, KeptTranscript), RpcFailure> {
    let (sink, kept) = TranscriptFolders::at(home)
        .start(output, history.storage().ok(), save)
        .map_err(|error| RpcFailure::from(&error))?;
    Ok((sink, KeptTranscript::new(kept, Arc::clone(history))))
}

/// A session's file and what keeps it. It ends exactly once: with the
/// session's outcome, or, dropped without one, as interrupted.
struct KeptTranscript {
    path: PathBuf,
    saving: TranscriptSaving,
    history: Arc<HistoryStore>,
    started: Instant,
    /// Taken when it ends.
    kept: Option<Kept>,
}

impl KeptTranscript {
    fn new(kept: Kept, history: Arc<HistoryStore>) -> Self {
        Self {
            path: kept.path().to_path_buf(),
            saving: kept.saving(),
            history,
            started: Instant::now(),
            kept: Some(kept),
        }
    }

    fn finish(mut self, failure: Option<RpcFailure>) {
        self.end(failure);
    }

    fn end(&mut self, failure: Option<RpcFailure>) {
        match self.kept.take() {
            Some(Kept::History { id, .. }) => self.history.finish_transcript(
                id,
                self.started.elapsed().as_millis() as u64,
                failure,
            ),
            Some(Kept::HistoryOff(session) | Kept::HistoryFull(session)) => {
                if let Err(error) = session.finish() {
                    tracing::warn!(error = %error_chain(&error), "a blank unsaved transcript could not be removed");
                }
            }
            Some(Kept::Output(_)) | None => {}
        }
    }
}

impl Drop for KeptTranscript {
    fn drop(&mut self) {
        self.end(Some(RpcFailure::new(
            RpcErrorCode::Internal,
            TRANSCRIPTION_INTERRUPTED,
        )));
    }
}

/// Saves the session before announcing its end, so whoever reacts to the last stage finds the
/// session already in history.
fn finish(
    result: &Result<(), SystemTranscriptionError>,
    kept: KeptTranscript,
    events: &SystemTranscriptionEventEmitter,
) {
    kept.finish(result.as_ref().err().map(RpcFailure::from));
    match result {
        Ok(()) => events.emit(SystemTranscriptionStage::Stopped),
        Err(error) => events.emit(SystemTranscriptionStage::Failed(error.into())),
    }
}

fn running(slot: &Slot) -> Option<&Path> {
    match slot {
        Slot::Running(active) => Some(&active.transcript),
        Slot::Idle | Slot::Ended { .. } => None,
    }
}

/// Moves a session that ended on its own into the reportable `Ended` state.
fn harvest(slot: &mut Slot) {
    let Slot::Running(active) = &*slot else {
        return;
    };
    if !active.thread.is_finished() {
        return;
    }
    let Slot::Running(active) = std::mem::replace(slot, Slot::Idle) else {
        unreachable!("the slot was just observed running");
    };
    let transcript = active.transcript.clone();
    let error = match join(active) {
        Ok(()) => None,
        Err(failure) => {
            tracing::warn!(
                code = failure.code.label(),
                message = failure.message,
                "system transcription ended with an error"
            );
            Some(failure)
        }
    };
    *slot = Slot::Ended { transcript, error };
}

fn join(active: ActiveSystemTranscription) -> Result<(), RpcFailure> {
    active.cancel.cancel();
    active.stopper.stop();
    match active.thread.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(RpcFailure::from(&error)),
        Err(_panic) => Err(RpcFailure::new(
            RpcErrorCode::Internal,
            "the transcription thread panicked",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_voice::history::HistoryQuery;

    fn history(directory: &Path) -> Arc<HistoryStore> {
        Arc::new(HistoryStore::open(
            &directory.join("history"),
            Arc::new(crate::daemon::storage::StorageKey::for_tests()),
        ))
    }

    fn listed(history: &HistoryStore) -> Vec<aria_voice::history::HistoryEntry> {
        history
            .list(HistoryQuery {
                section: None,
                query: String::new(),
                before_id: None,
                limit: 10,
                collapse_chats: false,
            })
            .expect("history lists")
            .entries
    }

    #[test]
    fn a_session_is_in_history_before_its_end_is_announced() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let history = history(directory.path());
        let (_sink, kept) = open_transcript(directory.path(), &history, None, SaveHistory::On)
            .expect("the session opens");
        std::fs::write(&kept.path, "Standup notes.").expect("the transcript is written");
        let listed_at_stop = Arc::new(Mutex::new(None));
        let events = SystemTranscriptionEventEmitter::forwarded_by({
            let history = Arc::clone(&history);
            let listed_at_stop = Arc::clone(&listed_at_stop);
            move |event| {
                if event == SystemTranscriptionStage::Stopped.into() {
                    let excerpts = listed(&history)
                        .into_iter()
                        .map(|entry| entry.text)
                        .collect::<Vec<_>>();
                    *listed_at_stop.lock().unwrap() = Some(excerpts);
                }
            }
        });

        finish(&Ok(()), kept, &events);

        assert_eq!(
            *listed_at_stop.lock().unwrap(),
            Some(vec!["Standup notes.".to_owned()])
        );
    }

    #[test]
    fn a_session_that_ends_without_an_outcome_ends_its_entry_as_interrupted() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let history = history(directory.path());
        let (sink, kept) = open_transcript(directory.path(), &history, None, SaveHistory::On)
            .expect("the session opens");
        std::fs::write(&kept.path, "Standup notes.").expect("the transcript is written");

        drop(sink);
        drop(kept);

        let entries = listed(&history);
        assert!(matches!(
            &entries[..],
            [entry] if entry.duration_ms.is_some()
                && entry.error.as_ref().is_some_and(|error| error.message == TRANSCRIPTION_INTERRUPTED)
        ));
    }

    #[test]
    fn a_session_run_while_history_is_unavailable_is_listed_but_kept_from_discard() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let (sink, kept) = open_transcript(
            directory.path(),
            &Arc::new(HistoryStore::new()),
            None,
            SaveHistory::Off,
        )
        .expect("the session opens");
        std::fs::write(&kept.path, "[00:00:01] heard").expect("the transcript is written");
        drop(sink);
        let name = kept.path.file_name().unwrap().to_str().unwrap().to_owned();
        let path = kept.path.clone();
        finish(
            &Ok(()),
            kept,
            &SystemTranscriptionEventEmitter::forwarded_by(|_| {}),
        );

        let folder = UnsavedFolder::open(directory.path(), None).expect("the unsaved folder");
        let listed = folder.list(None).expect("the unsaved transcripts list");
        let discarded = folder.discard(&name, None);

        assert_eq!(
            listed
                .transcripts
                .iter()
                .map(|transcript| transcript.name())
                .collect::<Vec<_>>(),
            [name.as_str()]
        );
        assert!(
            matches!(discarded, Err(error) if RpcErrorCode::from(&error) == RpcErrorCode::HistoryUnavailable)
        );
        assert!(path.exists());
    }
}
