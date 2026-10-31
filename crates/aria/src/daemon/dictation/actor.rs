//! The supervised owner of dictation blocking work.
//!
//! Callers hold a [`DictationHandle`] and submit typed commands; the actor
//! runs each one on the blocking pool, owns every job until it returns, and
//! folds daemon shutdown into each operation's cancellation token.

use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::dictation::{DictationEventEmitter, DictationOutcome, DictationRequest};

use super::super::operation::{
    CallError, Pending, Reply, cancellable_call, operation_cancellation,
};
use super::super::tasks::Supervised;
use super::runtime::{AutoStopRule, DictationRuntime, DictationRuntimeError};

/// A handoff slot, not a work queue: admission control stays in the domain
/// coordinator, so a submitted command is received promptly even while a
/// job runs.
const COMMAND_CAPACITY: usize = 1;

/// How many blocking jobs may run at once. Two, so a stop request can reach
/// the coordinator while a recording runs. At the cap the actor stops
/// receiving, and the bounded channel holds further submitters back.
const MAX_IN_FLIGHT_JOBS: usize = 2;

pub(in crate::daemon) type DictationCallError = CallError<DictationRuntimeError>;

struct DictationCommand {
    request: DictationRequest,
    auto_stop: AutoStopRule,
    cancellation: CancelToken,
    /// Where this press announces its stages. `None` means the daemon's
    /// own emitter, which every press reaches anyway.
    events: Option<DictationEventEmitter>,
    reply: Reply<DictationOutcome, DictationRuntimeError>,
}

/// The application-facing entry point for dictation. Cloneable; the channel
/// behind it is private.
#[derive(Clone)]
pub(in crate::daemon) struct DictationHandle {
    commands: mpsc::Sender<DictationCommand>,
}

impl DictationHandle {
    /// A handle nobody serves; every call reports Unavailable.
    #[cfg(test)]
    pub(in crate::daemon) fn detached() -> Self {
        let (commands, _) = mpsc::channel(COMMAND_CAPACITY);
        Self { commands }
    }

    pub(in crate::daemon) async fn start(
        &self,
        request: DictationRequest,
    ) -> Result<DictationOutcome, DictationCallError> {
        // The guard in `pending` cancels the operation if this future is
        // dropped after the command went out.
        self.submit(request, AutoStopRule::Setting, None)
            .await?
            .wait()
            .await
    }

    /// Hands the press over and returns with what will answer it, waiting
    /// for channel room when the actor is at its job cap. Dropping the
    /// `Pending` cancels the press.
    pub(in crate::daemon) async fn submit(
        &self,
        request: DictationRequest,
        auto_stop: AutoStopRule,
        events: Option<DictationEventEmitter>,
    ) -> Result<Pending<DictationOutcome, DictationRuntimeError>, DictationCallError> {
        let (command, pending) = cancellable_call(|cancellation, reply| DictationCommand {
            request,
            auto_stop,
            cancellation,
            events,
            reply,
        });
        self.commands
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        Ok(pending)
    }

    /// The same call for a caller already on a blocking thread. Never call it
    /// from an async context.
    pub(in crate::daemon) fn start_blocking(
        &self,
        request: DictationRequest,
    ) -> Result<DictationOutcome, DictationCallError> {
        let (command, pending) = cancellable_call(|cancellation, reply| DictationCommand {
            request,
            auto_stop: AutoStopRule::Setting,
            cancellation,
            events: None,
            reply,
        });
        self.commands
            .blocking_send(command)
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait_blocking()
    }
}

pub(in crate::daemon) struct DictationActor {
    commands: mpsc::Receiver<DictationCommand>,
    runtime: Arc<DictationRuntime>,
    jobs: JoinSet<()>,
}

pub(in crate::daemon) fn channel(
    runtime: Arc<DictationRuntime>,
) -> (DictationHandle, DictationActor) {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    (
        DictationHandle { commands },
        DictationActor {
            commands: receiver,
            runtime,
            jobs: JoinSet::new(),
        },
    )
}

impl Supervised for DictationActor {
    /// Runs until daemon shutdown or until every handle is gone, then drains:
    /// no started job outlives this future.
    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                Some(finished) = self.jobs.join_next() => observe(finished),
                command = self.commands.recv(),
                    if self.jobs.len() < MAX_IN_FLIGHT_JOBS => match command {
                    Some(command) => self.start(command, &shutdown),
                    None => break,
                },
            }
        }
        self.drain().await;
    }
}

impl DictationActor {
    fn start(&mut self, command: DictationCommand, shutdown: &CancellationToken) {
        let DictationCommand {
            request,
            auto_stop,
            cancellation,
            events,
            reply,
        } = command;
        let runtime = Arc::clone(&self.runtime);
        let cancellation = operation_cancellation(cancellation, shutdown.clone());
        reply.answer_with(&mut self.jobs, "dictation", move || {
            runtime.execute(request, auto_stop, cancellation, events.as_ref())
        });
    }

    async fn drain(mut self) {
        self.commands.close();
        while let Some(command) = self.commands.recv().await {
            command.reply.unavailable();
        }
        while let Some(finished) = self.jobs.join_next().await {
            observe(finished);
        }
    }
}

fn observe(finished: Result<(), tokio::task::JoinError>) {
    if let Err(error) = finished {
        tracing::error!(%error, "a dictation job ended unexpectedly");
    }
}
