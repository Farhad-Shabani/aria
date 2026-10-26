//! The supervised owner of preference store work.
//!
//! Callers hold a [`PreferencesHandle`]; the actor runs each store operation
//! on the blocking pool and owns every job until it returns. Store operations
//! are quick and not cancellable, so commands carry no cancellation token.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::Environment;
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
    environment: Environment,
    jobs: JoinSet<()>,
}

pub(super) fn channel(
    store: Arc<PreferenceStore>,
    environment: Environment,
) -> (PreferencesHandle, PreferencesActor) {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    (
        PreferencesHandle { commands },
        PreferencesActor {
            commands: receiver,
            store,
            environment,
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
                self.spawn_job(reply, |store, environment| {
                    let mut snapshot = store.snapshot()?;
                    resolve_assistant_id(&mut snapshot, environment);
                    Ok(snapshot)
                });
            }
            PreferencesCommand::Update {
                expected_revision,
                patch,
                reply,
            } => {
                self.spawn_job(reply, move |store, environment| {
                    let mut result = store.update(expected_revision, patch)?;
                    let snapshot = match &mut result {
                        UpdateResult::Updated(snapshot) | UpdateResult::Conflict(snapshot) => {
                            snapshot
                        }
                    };
                    resolve_assistant_id(snapshot, environment);
                    Ok(result)
                });
            }
        }
    }

    fn spawn_job<T>(
        &mut self,
        reply: Reply<T>,
        operation: impl FnOnce(&PreferenceStore, &Environment) -> Result<T, PreferenceStoreError>
        + Send
        + 'static,
    ) where
        T: Send + 'static,
    {
        let store = Arc::clone(&self.store);
        let environment = self.environment.clone();
        reply.answer_with(&mut self.jobs, "preferences", move || {
            operation(&store, &environment)
        });
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

fn resolve_assistant_id(snapshot: &mut PreferenceSnapshot, environment: &Environment) {
    match snapshot.preferences.resolve_assistant_id(environment) {
        Ok(assistant_id) => snapshot.preferences.assistant_id = Some(assistant_id),
        Err(error) => {
            tracing::warn!(error = %crate::primitives::error_chain(&error), "assistant configuration is invalid");
        }
    }
}

fn observe(finished: Result<(), tokio::task::JoinError>) {
    if let Err(error) = finished {
        tracing::error!(%error, "a preferences job ended unexpectedly");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AssistantId;

    #[tokio::test]
    async fn every_snapshot_resolves_the_assistant_without_persisting_the_fallback() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".aria")).unwrap();
        std::fs::write(
            home.path().join(".aria/config.yaml"),
            "chat:\n  assistant: codex\n",
        )
        .unwrap();
        let store = Arc::new(PreferenceStore::open(home.path().join("preferences.json")).unwrap());
        let (handle, actor) = channel(
            Arc::clone(&store),
            Environment::in_home(home.path().to_path_buf()),
        );
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(actor.run(shutdown.clone()));
        let snapshot = handle.get().await.unwrap();
        assert_eq!(snapshot.preferences.assistant_id, Some(AssistantId::Codex));
        let update = handle
            .update(
                0,
                PreferencePatch {
                    mute_output: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let UpdateResult::Updated(snapshot) = update else {
            panic!("update conflicts")
        };
        assert_eq!(snapshot.preferences.assistant_id, Some(AssistantId::Codex));
        assert_eq!(store.snapshot().unwrap().preferences.assistant_id, None);
        let conflict = handle.update(0, PreferencePatch::default()).await.unwrap();
        let UpdateResult::Conflict(snapshot) = conflict else {
            panic!("stale revision accepted")
        };
        assert_eq!(snapshot.preferences.assistant_id, Some(AssistantId::Codex));
        let update = handle
            .update(
                1,
                PreferencePatch {
                    assistant_id: Some(AssistantId::ClaudeCode),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let UpdateResult::Updated(snapshot) = update else {
            panic!("update conflicts")
        };
        assert_eq!(
            snapshot.preferences.assistant_id,
            Some(AssistantId::ClaudeCode)
        );
        assert_eq!(
            handle.get().await.unwrap().preferences.assistant_id,
            Some(AssistantId::ClaudeCode)
        );
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_assistant_configuration_is_lazy_and_an_explicit_choice_repairs_it() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".aria")).unwrap();
        std::fs::write(
            home.path().join(".aria/config.yaml"),
            "chat:\n  assistant: invalid\n",
        )
        .unwrap();
        let store = Arc::new(PreferenceStore::open(home.path().join("preferences.json")).unwrap());
        let (handle, actor) = channel(
            Arc::clone(&store),
            Environment::in_home(home.path().to_path_buf()),
        );
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(actor.run(shutdown.clone()));
        assert_eq!(handle.get().await.unwrap().preferences.assistant_id, None);
        let unrelated = handle
            .update(
                0,
                PreferencePatch {
                    mute_output: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(unrelated, UpdateResult::Updated(_)));
        assert_eq!(store.snapshot().unwrap().revision, 1);
        let update = handle
            .update(
                1,
                PreferencePatch {
                    assistant_id: Some(AssistantId::Codex),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(update, UpdateResult::Updated(_)));
        assert_eq!(
            handle.get().await.unwrap().preferences.assistant_id,
            Some(AssistantId::Codex)
        );
        shutdown.cancel();
        task.await.unwrap();
    }
}
