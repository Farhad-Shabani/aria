//! The supervised owner of preference store work.
//!
//! Callers hold a [`PreferencesHandle`]; the actor runs each store operation
//! on the blocking pool and owns every job until it returns. Store operations
//! are quick and not cancellable, so commands carry no cancellation token.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::preferences::{
    PreferencePatch, PreferenceSnapshot, PreferenceStore, PreferenceStoreError, UpdateResult,
};

use super::operation::{self, CallError, Pending, call};
use super::tasks::Supervised;

/// A handoff slot, not a work queue.
const COMMAND_CAPACITY: usize = 1;

/// How many blocking jobs may run at once. At the cap the actor stops
/// receiving, and the bounded channel holds further submitters back.
const MAX_IN_FLIGHT_JOBS: usize = 2;

enum PreferencesCommand {
    Get {
        reply: Reply<PreferenceSnapshot>,
    },
    Update {
        expected_revision: u64,
        patch: PreferencePatch,
        reply: Reply<UpdateResult>,
    },
}

type Reply<T> = operation::Reply<T, PreferenceStoreError>;

pub(super) type PreferencesCallError = CallError<PreferenceStoreError>;

/// The application-facing entry point for shared preferences. Cloneable; the
/// channel behind it is private.
#[derive(Clone)]
pub(super) struct PreferencesHandle {
    commands: mpsc::Sender<PreferencesCommand>,
}

impl PreferencesHandle {
    pub(super) async fn get(&self) -> Result<PreferenceSnapshot, PreferencesCallError> {
        let (command, pending) = call(|reply| PreferencesCommand::Get { reply });
        self.submit(command, pending).await
    }

    pub(super) async fn update(
        &self,
        expected_revision: u64,
        patch: PreferencePatch,
    ) -> Result<UpdateResult, PreferencesCallError> {
        let (command, pending) = call(|reply| PreferencesCommand::Update {
            expected_revision,
            patch,
            reply,
        });
        self.submit(command, pending).await
    }

    async fn submit<T>(
        &self,
        command: PreferencesCommand,
        pending: Pending<T, PreferenceStoreError>,
    ) -> Result<T, PreferencesCallError> {
        self.commands
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }
}

pub(super) struct PreferencesActor {
    commands: mpsc::Receiver<PreferencesCommand>,
    store: Arc<PreferenceStore>,
    jobs: JoinSet<()>,
}

pub(super) fn channel(store: Arc<PreferenceStore>) -> (PreferencesHandle, PreferencesActor) {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    (
        PreferencesHandle { commands },
        PreferencesActor {
            commands: receiver,
            store,
            jobs: JoinSet::new(),
        },
    )
}

impl Supervised for PreferencesActor {
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
                    Some(command) => self.start(command),
                    None => break,
                },
            }
        }
        self.drain().await;
    }
}

impl PreferencesActor {
    fn start(&mut self, command: PreferencesCommand) {
        match command {
            PreferencesCommand::Get { reply } => {
                self.spawn_job(reply, PreferenceStore::snapshot);
            }
            PreferencesCommand::Update {
                expected_revision,
                patch,
                reply,
            } => {
                self.spawn_job(reply, move |store| store.update(expected_revision, patch));
            }
        }
    }

    fn spawn_job<T>(
        &mut self,
        reply: Reply<T>,
        operation: impl FnOnce(&PreferenceStore) -> Result<T, PreferenceStoreError> + Send + 'static,
    ) where
        T: Send + 'static,
    {
        let store = Arc::clone(&self.store);
        reply.answer_with(&mut self.jobs, "preferences", move || operation(&store));
    }

    async fn drain(mut self) {
        self.commands.close();
        while let Some(command) = self.commands.recv().await {
            match command {
                PreferencesCommand::Get { reply } => reply.unavailable(),
                PreferencesCommand::Update { reply, .. } => reply.unavailable(),
            }
        }
        while let Some(finished) = self.jobs.join_next().await {
            observe(finished);
        }
    }
}

fn observe(finished: Result<(), tokio::task::JoinError>) {
    if let Err(error) = finished {
        tracing::error!(%error, "a preferences job ended unexpectedly");
    }
}
