//! The supervised owner of speech blocking work.
//!
//! Callers hold a [`SpeechHandle`]; the actor runs each utterance on the
//! blocking pool, owns every job until it returns, and folds daemon shutdown
//! into each operation's cancellation token. Stop rides its own channel so a
//! full speak slot can never starve it.

use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::speech::{SpeechOutcome, SpeechRequest};

use super::super::operation::{CallError, Reply, cancellable_call, operation_cancellation};
use super::super::tasks::Supervised;
use super::runtime::{SpeechRuntime, SpeechRuntimeError};

/// A handoff slot, not a work queue: admission control stays in the domain
/// coordinator.
const COMMAND_CAPACITY: usize = 1;

/// How many blocking jobs may run at once. Two, so a replacement utterance
/// can reach the coordinator while one plays. At the cap the actor stops
/// receiving speaks, and the bounded channel holds further submitters back.
const MAX_IN_FLIGHT_JOBS: usize = 2;

pub(in crate::daemon) type SpeechCallError = CallError<SpeechRuntimeError>;

struct SpeakCommand {
    request: SpeechRequest,
    cancellation: CancelToken,
    reply: Reply<SpeechOutcome, SpeechRuntimeError>,
}

/// Stopping answers from the actor loop itself, so it carries a bare reply
/// rather than the reply of a job that could fail or panic.
struct StopCommand {
    reply: oneshot::Sender<bool>,
}

/// The application-facing entry point for speech. Cloneable; the channels
/// behind it are private.
#[derive(Clone)]
pub(in crate::daemon) struct SpeechHandle {
    speaks: mpsc::Sender<SpeakCommand>,
    stops: mpsc::Sender<StopCommand>,
}

impl SpeechHandle {
    pub(in crate::daemon) async fn speak(
        &self,
        request: SpeechRequest,
    ) -> Result<SpeechOutcome, SpeechCallError> {
        let (command, pending) = cancellable_call(|cancellation, reply| SpeakCommand {
            request,
            cancellation,
            reply,
        });
        // The guard in `pending` cancels the operation if this future is
        // dropped after the command went out.
        self.speaks
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }

    pub(in crate::daemon) async fn stop(&self) -> Result<bool, SpeechCallError> {
        let (reply, response) = oneshot::channel();
        self.stops
            .send(StopCommand { reply })
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        response.await.map_err(|_closed| CallError::Unavailable)
    }
}

pub(in crate::daemon) struct SpeechActor {
    speaks: mpsc::Receiver<SpeakCommand>,
    stops: mpsc::Receiver<StopCommand>,
    runtime: Arc<SpeechRuntime>,
    jobs: JoinSet<()>,
}

pub(in crate::daemon) fn channel(runtime: Arc<SpeechRuntime>) -> (SpeechHandle, SpeechActor) {
    let (speaks, speak_receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (stops, stop_receiver) = mpsc::channel(COMMAND_CAPACITY);
    (
        SpeechHandle { speaks, stops },
        SpeechActor {
            speaks: speak_receiver,
            stops: stop_receiver,
            runtime,
            jobs: JoinSet::new(),
        },
    )
}

impl Supervised for SpeechActor {
    /// Runs until daemon shutdown or until every handle is gone, then drains:
    /// no started job outlives this future.
    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                Some(stop) = self.stops.recv() => self.stop(stop),
                Some(finished) = self.jobs.join_next() => observe(finished),
                speak = self.speaks.recv(),
                    if self.jobs.len() < MAX_IN_FLIGHT_JOBS => match speak {
                    Some(command) => self.speak(command, &shutdown),
                    None => break,
                },
            }
        }
        self.drain().await;
    }
}

impl SpeechActor {
    fn speak(&mut self, command: SpeakCommand, shutdown: &CancellationToken) {
        let SpeakCommand {
            request,
            cancellation,
            reply,
        } = command;
        let runtime = Arc::clone(&self.runtime);
        let cancellation = operation_cancellation(cancellation, shutdown.clone());
        reply.answer_with(&mut self.jobs, "speech", move || {
            runtime.execute(request, cancellation)
        });
    }

    /// Stopping signals and returns, so it runs here rather than on the
    /// blocking pool, and never occupies a job slot.
    fn stop(&self, command: StopCommand) {
        let _ = command.reply.send(self.runtime.stop());
    }

    async fn drain(mut self) {
        self.speaks.close();
        self.stops.close();
        while let Some(command) = self.speaks.recv().await {
            command.reply.unavailable();
        }
        // A queued stop still means "silence the utterance", so it runs.
        while let Some(stop) = self.stops.recv().await {
            self.stop(stop);
        }
        while let Some(finished) = self.jobs.join_next().await {
            observe(finished);
        }
    }
}

fn observe(finished: Result<(), tokio::task::JoinError>) {
    if let Err(error) = finished {
        tracing::error!(%error, "a speech job ended unexpectedly");
    }
}
