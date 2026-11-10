//! Daemon ownership of the system transcription.
//!
//! A mutexed slot rather than an actor: a system transcription must outlive the
//! RPC that started it, so nothing here ties cancellation to a request.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use aria_kit::cancel::CancelToken;

use aria_kit::audio::Stopper;
use aria_primitives::error_chain;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_voice::config::{SpeakerConfig, SystemTranscriptionConfig};
use aria_voice::system_transcription::{
    SystemAudioCapture, SystemTranscriptionError, SystemTranscriptionEventEmitter,
    SystemTranscriptionSnapshot, SystemTranscriptionStage, SystemTranscriptionStarted,
    SystemTranscriptionStopped, TranscriptFile, run_session,
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
        Self {
            lifetime,
            home,
            cache,
            settings,
            events,
            vocabulary,
            history,
            active: Mutex::new(Slot::Idle),
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
        let mut slot = self.lock();
        harvest(&mut slot);
        if let Slot::Running(active) = &*slot {
            return Ok(SystemTranscriptionStarted {
                already_running: true,
                transcript: active.transcript.clone(),
            });
        }

        let internal = |error: aria_settings::config::ConfigError| {
            RpcFailure::new(RpcErrorCode::Internal, error_chain(&error))
        };
        let settings = self.cache.effective();
        let config = SystemTranscriptionConfig::load(&settings, &self.home).map_err(internal)?;
        let speaker = SpeakerConfig::load(&settings.speaker, &self.home);
        // An unreadable settings file refuses the session rather than guess whether to save
        // history.
        let history_enabled = self
            .settings
            .snapshot()
            .map_err(|error| RpcFailure::from(&error))?
            .settings
            .history_enabled;
        let saves_history = saves_history(output.is_some(), history_enabled);
        let (sink, transcript) = match output {
            Some(path) => TranscriptFile::create_at(path),
            None => TranscriptFile::create(&config.transcripts_dir),
        }
        .map_err(|error| RpcFailure::new(RpcErrorCode::Internal, error_chain(&error)))?;
        let (capture, recording) =
            SystemAudioCapture::spawn().map_err(|error| RpcFailure::from(&error))?;
        let stopper = capture.stopper();
        let cancel = CancelToken::new();
        // System transcription has no focused project: the user's own word list only. The
        // snapshot is let go at once, so background vocabulary work does
        // not wait on a session that runs for hours.
        let vocabulary = Arc::clone(&self.vocabulary.snapshot(None).matcher);
        let record =
            saves_history.then(|| HistoryRecord::start(Arc::clone(&self.history), &transcript));
        let thread = thread::Builder::new()
            .name("aria-transcribe".into())
            .spawn({
                let events = self.events.clone();
                let cancel = cancel.clone();
                move || {
                    let result = run_session(
                        &config,
                        &speaker,
                        &vocabulary,
                        &events,
                        capture,
                        recording,
                        sink,
                        &cancel,
                    );
                    finish(&result, record, &events);
                    result
                }
            })
            .map_err(|error| {
                stopper.stop();
                RpcFailure::new(RpcErrorCode::Internal, error_chain(&error))
            })?;
        tracing::info!(transcript = %transcript.display(), "system transcription started");
        *slot = Slot::Running(ActiveSystemTranscription {
            stopper,
            cancel,
            thread,
            transcript: transcript.clone(),
        });
        Ok(SystemTranscriptionStarted {
            already_running: false,
            transcript,
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

    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.active
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

/// History links only a file the daemon named. A file the caller named may hold other text, which
/// deleting the entry would take with it.
fn saves_history(caller_named_file: bool, history_enabled: bool) -> bool {
    !caller_named_file && history_enabled
}

/// What a session saves to history once it ends.
struct HistoryRecord {
    history: Arc<HistoryStore>,
    transcript: PathBuf,
    created_at_ms: u64,
    started: Instant,
}

impl HistoryRecord {
    fn start(history: Arc<HistoryStore>, transcript: &Path) -> Self {
        Self {
            history,
            transcript: transcript.to_path_buf(),
            created_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            started: Instant::now(),
        }
    }
}

/// Saves the session before announcing its end, so whoever reacts to the last stage finds the
/// session already in history.
fn finish(
    result: &Result<(), SystemTranscriptionError>,
    record: Option<HistoryRecord>,
    events: &SystemTranscriptionEventEmitter,
) {
    if let Some(record) = record {
        record.history.save_transcript(
            &record.transcript,
            record.created_at_ms,
            record.started.elapsed().as_millis() as u64,
            result.as_ref().err().map(RpcFailure::from),
        );
    }
    match result {
        Ok(()) => events.emit(SystemTranscriptionStage::Stopped),
        Err(error) => events.emit(SystemTranscriptionStage::Failed(error.into())),
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

    #[test]
    fn a_session_is_in_history_before_its_end_is_announced() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let history = Arc::new(HistoryStore::open(&directory.path().join("history")));
        let transcript = directory.path().join("transcript.md");
        std::fs::write(&transcript, "Standup notes.").expect("the transcript is written");
        let listed_at_stop = Arc::new(Mutex::new(None));
        let events = SystemTranscriptionEventEmitter::forwarded_by({
            let history = Arc::clone(&history);
            let listed_at_stop = Arc::clone(&listed_at_stop);
            move |stage| {
                if stage == SystemTranscriptionStage::Stopped {
                    let page = history
                        .list(HistoryQuery {
                            section: None,
                            query: String::new(),
                            before_id: None,
                            limit: 10,
                            collapse_chats: false,
                        })
                        .expect("history lists");
                    *listed_at_stop.lock().unwrap() = Some(page.entries.len());
                }
            }
        });

        finish(
            &Ok(()),
            Some(HistoryRecord::start(Arc::clone(&history), &transcript)),
            &events,
        );

        assert_eq!(*listed_at_stop.lock().unwrap(), Some(1));
    }

    #[test]
    fn only_a_file_the_daemon_named_with_history_on_is_saved() {
        assert!(saves_history(false, true));
        assert!(!saves_history(true, true));
        assert!(!saves_history(false, false));
    }
}
