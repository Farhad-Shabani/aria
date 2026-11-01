//! The supervised owner of settings store work.
//!
//! Callers hold a [`SettingsHandle`]; the actor runs each store operation
//! on the blocking pool and owns every job until it returns. Store operations
//! are quick and not cancellable, so commands carry no cancellation token.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::AssistantId;
use crate::settings::{SettingsPatch, SettingsSnapshot, SettingsStore, StoreError, UpdateResult};

use super::operation::{self, CallError, Pending, call};
use super::subscriptions::Subscriptions;
use super::tasks::Supervised;
use crate::settings::SettingsStatus;

/// A handoff slot, not a work queue.
const COMMAND_CAPACITY: usize = 1;

/// How many blocking jobs may run at once. At the cap the actor stops
/// receiving, and the bounded channel holds further submitters back.
const MAX_IN_FLIGHT_JOBS: usize = 2;

enum SettingsCommand {
    Get {
        reply: Reply<SettingsSnapshot>,
    },
    Update {
        expected_revision: u64,
        patch: SettingsPatch,
        reply: Reply<UpdateResult>,
    },
    Check {
        patch: SettingsPatch,
        reply: Reply<()>,
    },
}

type Reply<T> = operation::Reply<T, StoreError>;

pub(super) type SettingsCallError = CallError<StoreError>;

/// The application-facing entry point for shared settings. Cloneable; the
/// channel behind it is private.
#[derive(Clone)]
pub(super) struct SettingsHandle {
    commands: mpsc::Sender<SettingsCommand>,
}

impl SettingsHandle {
    pub(super) async fn get(&self) -> Result<SettingsSnapshot, SettingsCallError> {
        let (command, pending) = call(|reply| SettingsCommand::Get { reply });
        self.submit(command, pending).await
    }

    pub(super) async fn update(
        &self,
        expected_revision: u64,
        patch: SettingsPatch,
    ) -> Result<UpdateResult, SettingsCallError> {
        let (command, pending) = call(|reply| SettingsCommand::Update {
            expected_revision,
            patch,
            reply,
        });
        self.submit(command, pending).await
    }

    pub(super) async fn check(&self, patch: SettingsPatch) -> Result<(), SettingsCallError> {
        let (command, pending) = call(|reply| SettingsCommand::Check { patch, reply });
        self.submit(command, pending).await
    }

    async fn submit<T>(
        &self,
        command: SettingsCommand,
        pending: Pending<T, StoreError>,
    ) -> Result<T, SettingsCallError> {
        self.commands
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }
}

pub(super) struct SettingsActor {
    commands: mpsc::Receiver<SettingsCommand>,
    store: Arc<SettingsStore>,
    jobs: JoinSet<()>,
}

pub(super) fn channel(store: Arc<SettingsStore>) -> (SettingsHandle, SettingsActor) {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    (
        SettingsHandle { commands },
        SettingsActor {
            commands: receiver,
            store,
            jobs: JoinSet::new(),
        },
    )
}

impl Supervised for SettingsActor {
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

impl SettingsActor {
    fn start(&mut self, command: SettingsCommand) {
        match command {
            SettingsCommand::Get { reply } => {
                self.spawn_job(reply, |store| {
                    let mut snapshot = store.snapshot()?;
                    snapshot
                        .settings
                        .assistant_id
                        .get_or_insert(AssistantId::DEFAULT);
                    Ok(snapshot)
                });
            }
            SettingsCommand::Update {
                expected_revision,
                patch,
                reply,
            } => {
                self.spawn_job(reply, move |store| {
                    let mut result = store.update(expected_revision, patch)?;
                    let snapshot = match &mut result {
                        UpdateResult::Updated { snapshot, .. }
                        | UpdateResult::Conflict(snapshot) => snapshot,
                    };
                    snapshot
                        .settings
                        .assistant_id
                        .get_or_insert(AssistantId::DEFAULT);
                    Ok(result)
                });
            }
            SettingsCommand::Check { patch, reply } => {
                self.spawn_job(reply, move |store| store.check(patch));
            }
        }
    }

    fn spawn_job<T>(
        &mut self,
        reply: Reply<T>,
        operation: impl FnOnce(&SettingsStore) -> Result<T, StoreError> + Send + 'static,
    ) where
        T: Send + 'static,
    {
        let store = Arc::clone(&self.store);
        reply.answer_with(&mut self.jobs, "settings", move || operation(&store));
    }

    async fn drain(mut self) {
        self.commands.close();
        while let Some(command) = self.commands.recv().await {
            match command {
                SettingsCommand::Get { reply } => reply.unavailable(),
                SettingsCommand::Update { reply, .. } => reply.unavailable(),
                SettingsCommand::Check { reply, .. } => reply.unavailable(),
            }
        }
        while let Some(finished) = self.jobs.join_next().await {
            observe(finished);
        }
    }
}

/// Pushes each change of the settings status to every subscriber, whether
/// the poll thread's reload or a write made it.
pub(super) struct StatusBroadcast {
    status: watch::Receiver<SettingsStatus>,
    subscriptions: Arc<Subscriptions>,
}

impl StatusBroadcast {
    pub(super) fn new(
        status: watch::Receiver<SettingsStatus>,
        subscriptions: Arc<Subscriptions>,
    ) -> Self {
        Self {
            status,
            subscriptions,
        }
    }
}

impl Supervised for StatusBroadcast {
    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                changed = self.status.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let status = self.status.borrow_and_update().clone();
                    self.subscriptions.broadcast(&status);
                }
            }
        }
    }
}

fn observe(finished: Result<(), tokio::task::JoinError>) {
    if let Err(error) = finished {
        tracing::error!(%error, "a settings job ended unexpectedly");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_snapshot_resolves_the_default_assistant_without_persisting_it() {
        let home = tempfile::tempdir().unwrap();
        let store = Arc::new(SettingsStore::open(home.path().join("settings.toml")).unwrap());
        let (handle, actor) = channel(Arc::clone(&store));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(actor.run(shutdown.clone()));
        let first = handle.get().await.unwrap();
        assert_eq!(first.settings.assistant_id, Some(AssistantId::DEFAULT));
        let update = handle
            .update(
                first.revision,
                SettingsPatch {
                    mute_sound: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let UpdateResult::Updated { snapshot, .. } = update else {
            panic!("update conflicts")
        };
        assert_eq!(snapshot.settings.assistant_id, Some(AssistantId::DEFAULT));
        assert_eq!(store.snapshot().unwrap().settings.assistant_id, None);
        let written = snapshot.revision;
        let conflict = handle
            .update(first.revision, SettingsPatch::default())
            .await
            .unwrap();
        let UpdateResult::Conflict(snapshot) = conflict else {
            panic!("stale revision accepted")
        };
        assert_eq!(snapshot.settings.assistant_id, Some(AssistantId::DEFAULT));
        let update = handle
            .update(
                written,
                SettingsPatch {
                    assistant_id: Some(AssistantId::Codex),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let UpdateResult::Updated { snapshot, .. } = update else {
            panic!("update conflicts")
        };
        assert_eq!(snapshot.settings.assistant_id, Some(AssistantId::Codex));
        assert_eq!(
            handle.get().await.unwrap().settings.assistant_id,
            Some(AssistantId::Codex)
        );
        shutdown.cancel();
        task.await.unwrap();
    }
}
