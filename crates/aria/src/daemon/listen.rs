//! Daemon ownership of the listen session.
//!
//! A mutexed slot rather than an actor: a listen session must outlive the
//! RPC that started it, so nothing here ties cancellation to a request.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use aria_kit::cancel::CancelToken;

use crate::audio::Stopper;
use crate::config::{Environment, ListenConfig, SpeakerConfig};
use crate::listen::{
    ListenError, ListenEventEmitter, SystemAudioCapture, TranscriptFile, spawn_session,
};
use crate::primitives::error_chain;
use crate::rpc::{RpcErrorCode, RpcFailure};

use super::host::DaemonLifetime;

pub(super) struct ListenState {
    lifetime: DaemonLifetime,
    environment: Environment,
    events: ListenEventEmitter,
    vocabulary: Arc<super::vocabulary::Runtime>,
    active: Mutex<Slot>,
}

enum Slot {
    Idle,
    Running(ActiveListen),
    /// The session ended on its own after Start had answered. Status
    /// reports why when it failed.
    Ended {
        transcript: PathBuf,
        error: Option<String>,
    },
}

struct ActiveListen {
    stopper: Stopper,
    /// Checked around a model load, so a stop during the load ends the
    /// session once it returns instead of starting transcription.
    cancel: CancelToken,
    thread: JoinHandle<Result<(), ListenError>>,
    transcript: PathBuf,
}

pub(super) struct ListenStatus {
    pub(super) listening: bool,
    pub(super) transcript: PathBuf,
    pub(super) error: Option<String>,
}

pub(super) struct StopOutcome {
    pub(super) was_listening: bool,
    pub(super) transcript: PathBuf,
}

impl ListenState {
    pub(super) fn new(
        lifetime: DaemonLifetime,
        environment: Environment,
        events: ListenEventEmitter,
        vocabulary: Arc<super::vocabulary::Runtime>,
    ) -> Self {
        Self {
            lifetime,
            environment,
            events,
            vocabulary,
            active: Mutex::new(Slot::Idle),
        }
    }

    /// Accepts a session; capture and model initialization run on worker threads.
    /// `output` overrides the configured transcripts directory with an exact
    /// file for this session.
    pub(super) fn start(&self, output: Option<PathBuf>) -> Result<(PathBuf, bool), RpcFailure> {
        if self.lifetime == DaemonLifetime::OnDemand {
            return Err(RpcFailure::new(
                RpcErrorCode::Unsupported,
                "listen needs the persistent aria daemon; start `aria daemon`",
            ));
        }
        let mut slot = self.lock();
        harvest(&mut slot);
        if let Slot::Running(active) = &*slot {
            return Ok((active.transcript.clone(), true));
        }

        // Read per Start, so a broken listen section never blocks the daemon.
        let config = ListenConfig::load(&self.environment)
            .map_err(|error| RpcFailure::new(RpcErrorCode::Internal, error_chain(&error)))?;
        let speaker = SpeakerConfig::load(&self.environment)
            .map_err(|error| RpcFailure::new(RpcErrorCode::Internal, error_chain(&error)))?;
        let (sink, transcript) = match output {
            Some(path) => TranscriptFile::create_at(path),
            None => TranscriptFile::create(&config.transcripts_dir),
        }
        .map_err(|error| RpcFailure::new(RpcErrorCode::Internal, error_chain(&error)))?;
        let (capture, recording) = SystemAudioCapture::spawn().map_err(failure)?;
        let stopper = capture.stopper();
        let cancel = CancelToken::new();
        // Listen has no focused project: the user's own word list only. The
        // snapshot is let go at once, so background vocabulary work does
        // not wait on a session that runs for hours.
        let vocabulary = Arc::clone(&self.vocabulary.snapshot(None).matcher);
        let thread = spawn_session(
            config,
            speaker,
            vocabulary,
            self.events.clone(),
            capture,
            recording,
            sink,
            cancel.clone(),
        )
        .map_err(|error| {
            stopper.stop();
            RpcFailure::new(RpcErrorCode::Internal, error_chain(&error))
        })?;
        tracing::info!(transcript = %transcript.display(), "listen session started");
        *slot = Slot::Running(ActiveListen {
            stopper,
            cancel,
            thread,
            transcript: transcript.clone(),
        });
        Ok((transcript, false))
    }

    /// Stops the running session, reporting a session that failed instead of
    /// stopping cleanly as that failure.
    pub(super) fn stop(&self) -> Result<StopOutcome, RpcFailure> {
        // The lock is held through the join so a concurrent Start cannot
        // open a second capture while this one winds down.
        let mut slot = self.lock();
        let previous = std::mem::replace(&mut *slot, Slot::Idle);
        match previous {
            Slot::Idle => Ok(StopOutcome {
                was_listening: false,
                transcript: PathBuf::new(),
            }),
            Slot::Ended { transcript, .. } => Ok(StopOutcome {
                was_listening: false,
                transcript,
            }),
            Slot::Running(active) => {
                let transcript = active.transcript.clone();
                join(active).map(|()| {
                    tracing::info!(transcript = %transcript.display(), "listen session stopped");
                    StopOutcome {
                        was_listening: true,
                        transcript,
                    }
                })
            }
        }
    }

    pub(super) fn status(&self) -> ListenStatus {
        let mut slot = self.lock();
        harvest(&mut slot);
        match &*slot {
            Slot::Idle => ListenStatus {
                listening: false,
                transcript: PathBuf::new(),
                error: None,
            },
            Slot::Running(active) => ListenStatus {
                listening: true,
                transcript: active.transcript.clone(),
                error: None,
            },
            Slot::Ended { transcript, error } => ListenStatus {
                listening: false,
                transcript: transcript.clone(),
                error: error.clone(),
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
impl Drop for ListenState {
    fn drop(&mut self) {
        let previous = std::mem::replace(&mut *self.lock(), Slot::Idle);
        if let Slot::Running(active) = previous
            && let Err(failure) = join(active)
        {
            tracing::warn!(
                code = failure.code.label(),
                message = failure.message,
                "listen session failed during daemon shutdown"
            );
        }
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
                "listen session ended with an error"
            );
            Some(failure.message)
        }
    };
    *slot = Slot::Ended { transcript, error };
}

fn join(active: ActiveListen) -> Result<(), RpcFailure> {
    active.cancel.cancel();
    active.stopper.stop();
    match active.thread.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(failure(error)),
        Err(_panic) => Err(RpcFailure::new(
            RpcErrorCode::Internal,
            "the listen session thread panicked",
        )),
    }
}

fn failure(error: ListenError) -> RpcFailure {
    RpcFailure::new(error.code(), error_chain(&error))
}
