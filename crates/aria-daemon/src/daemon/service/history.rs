use std::sync::Arc;
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

use crate::ipc::{protocol, wire};
use aria_voice::history::{
    ClearHistoryResult, HistoryEntry, HistoryError, HistoryPage, HistoryQuery, HistorySection,
    PromptResult, RetryOptions, Title,
};

use super::super::dictation::DictationRuntime;
use super::super::history::HistoryStore;
use super::super::operation::{Pending, Reply, call, cancellable_call, operation_cancellation};
use super::super::server::ActivityGuard;
use super::super::tasks::Supervised;

const MAX_JOBS: usize = 8;

pub(in crate::daemon) struct HistoryRpc {
    commands: mpsc::Sender<Command>,
}

pub(in crate::daemon) struct HistoryWorker {
    commands: mpsc::Receiver<Command>,
    store: Arc<HistoryStore>,
    runtime: Arc<DictationRuntime>,
}

enum Command {
    List(
        HistoryQuery,
        Reply<HistoryPage, HistoryError>,
        Option<Arc<ActivityGuard>>,
    ),
    Get(
        u64,
        Reply<HistoryEntry, HistoryError>,
        Option<Arc<ActivityGuard>>,
    ),
    Rename(
        u64,
        Option<Title>,
        Reply<HistoryEntry, HistoryError>,
        Option<Arc<ActivityGuard>>,
    ),
    Delete(u64, Reply<(), HistoryError>, Option<Arc<ActivityGuard>>),
    Results(
        u64,
        Reply<Vec<PromptResult>, HistoryError>,
        Option<Arc<ActivityGuard>>,
    ),
    Clear(
        Option<HistorySection>,
        Reply<ClearHistoryResult, HistoryError>,
        Option<Arc<ActivityGuard>>,
    ),
    Retry(
        u64,
        RetryOptions,
        CancelToken,
        Reply<HistoryEntry, HistoryError>,
        Option<Arc<ActivityGuard>>,
    ),
}

impl HistoryRpc {
    pub(in crate::daemon) fn new(
        store: Arc<HistoryStore>,
        runtime: Arc<DictationRuntime>,
    ) -> (Self, HistoryWorker) {
        let (commands, receiver) = mpsc::channel(MAX_JOBS);
        (
            Self { commands },
            HistoryWorker {
                commands: receiver,
                store,
                runtime,
            },
        )
    }

    async fn submit<T>(
        &self,
        method: &'static str,
        command: Command,
        pending: Pending<T, HistoryError>,
    ) -> Result<T, Status> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => super::failed(method, &HistoryError::Busy),
                mpsc::error::TrySendError::Closed(_) => {
                    super::failed(method, &HistoryError::Unavailable)
                }
            })?;
        let served = pending
            .wait()
            .await
            .map_err(|error| super::failed(method, &error))?;
        super::served(method);
        Ok(served)
    }
}

impl Supervised for HistoryWorker {
    async fn run(mut self, shutdown: CancellationToken) {
        let mut jobs = JoinSet::new();
        let mut maintenance = tokio::time::interval(Duration::from_secs(60));
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                    if result.is_err() {
                        tracing::error!("history maintenance task failed");
                    }
                },
                command = self.commands.recv(), if jobs.len() < MAX_JOBS => {
                    let Some(command) = command else { break; };
                    let store = Arc::clone(&self.store);
                    match command {
                        Command::List(query, reply, activity) => reply.answer_with(&mut jobs, "history list", move || {
                            let _activity = activity;
                            store.list(query)
                        }),
                        Command::Get(id, reply, activity) => reply.answer_with(&mut jobs, "history get", move || {
                            let _activity = activity;
                            store.get(id)
                        }),
                        Command::Rename(id, title, reply, activity) => reply.answer_with(&mut jobs, "history rename", move || {
                            let _activity = activity;
                            store.rename(id, title.as_ref())
                        }),
                        Command::Delete(id, reply, activity) => reply.answer_with(&mut jobs, "history delete", move || {
                            let _activity = activity;
                            store.delete(id)
                        }),
                        Command::Results(id, reply, activity) => reply.answer_with(&mut jobs, "history results", move || {
                            let _activity = activity;
                            store.results(id)
                        }),
                        Command::Clear(section, reply, activity) => reply.answer_with(&mut jobs, "history clear", move || {
                            let _activity = activity;
                            store.clear(section)
                        }),
                        Command::Retry(id, options, cancel, reply, activity) => {
                            let runtime = Arc::clone(&self.runtime);
                            let cancel = operation_cancellation(cancel, shutdown.clone());
                            reply.answer_with(&mut jobs, "history retry", move || {
                                let _activity = activity;
                                runtime.retry_history(id, options, cancel)
                            });
                        }
                    }
                }
                _ = maintenance.tick(), if jobs.is_empty() => {
                    let store = Arc::clone(&self.store);
                    jobs.spawn_blocking(move || {
                        if let Err(error) = store.maintenance() {
                            tracing::warn!(%error, "history maintenance failed");
                        }
                    });
                }
            }
        }
        self.commands.close();
        while let Some(command) = self.commands.recv().await {
            match command {
                Command::List(_, reply, _) => reply.unavailable(),
                Command::Get(_, reply, _) | Command::Retry(_, _, _, reply, _) => {
                    reply.unavailable();
                }
                Command::Rename(_, _, reply, _) => reply.unavailable(),
                Command::Delete(_, reply, _) => reply.unavailable(),
                Command::Results(_, reply, _) => reply.unavailable(),
                Command::Clear(_, reply, _) => reply.unavailable(),
            }
        }
        while let Some(result) = jobs.join_next().await {
            if result.is_err() {
                tracing::error!("history maintenance task failed during shutdown");
            }
        }
    }
}

#[tonic::async_trait]
impl wire::history_service_server::HistoryService for HistoryRpc {
    async fn list(
        &self,
        mut request: Request<wire::ListHistoryRequest>,
    ) -> Result<Response<wire::ListHistoryResponse>, Status> {
        const METHOD: &str = "HistoryService/List";
        let activity = ActivityGuard::take(&mut request);
        let query = HistoryQuery::try_from(request.into_inner())
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let (command, pending) = call(|reply| Command::List(query, reply, activity));
        self.submit(METHOD, command, pending)
            .await
            .map(|page| Response::new(page.into()))
    }

    async fn get(
        &self,
        mut request: Request<wire::GetHistoryRequest>,
    ) -> Result<Response<wire::HistoryEntry>, Status> {
        const METHOD: &str = "HistoryService/Get";
        let activity = ActivityGuard::take(&mut request);
        let id = protocol::history_id(request.into_inner().id)
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let (command, pending) = call(|reply| Command::Get(id, reply, activity));
        self.submit(METHOD, command, pending)
            .await
            .map(|entry| Response::new(entry.into()))
    }

    async fn delete(
        &self,
        mut request: Request<wire::DeleteHistoryRequest>,
    ) -> Result<Response<wire::DeleteHistoryResponse>, Status> {
        const METHOD: &str = "HistoryService/Delete";
        let activity = ActivityGuard::take(&mut request);
        let id = protocol::history_id(request.into_inner().id)
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let (command, pending) = call(|reply| Command::Delete(id, reply, activity));
        self.submit(METHOD, command, pending).await?;
        Ok(Response::new(wire::DeleteHistoryResponse {}))
    }

    async fn get_thread(
        &self,
        mut request: Request<wire::GetThreadRequest>,
    ) -> Result<Response<wire::HistoryThread>, Status> {
        const METHOD: &str = "HistoryService/GetThread";
        let activity = ActivityGuard::take(&mut request);
        let id = protocol::history_id(request.into_inner().entry_id)
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let (command, pending) = call(|reply| Command::Results(id, reply, activity));
        let results = self.submit(METHOD, command, pending).await?;
        Ok(Response::new(results.into()))
    }

    async fn clear(
        &self,
        mut request: Request<wire::ClearHistoryRequest>,
    ) -> Result<Response<wire::ClearHistoryResponse>, Status> {
        const METHOD: &str = "HistoryService/Clear";
        let activity = ActivityGuard::take(&mut request);
        let section = request
            .into_inner()
            .section
            .map(protocol::history_section)
            .transpose()
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let (command, pending) = call(|reply| Command::Clear(section, reply, activity));
        let result = self.submit(METHOD, command, pending).await?;
        Ok(Response::new(result.into()))
    }

    async fn rename(
        &self,
        mut request: Request<wire::RenameHistoryRequest>,
    ) -> Result<Response<wire::RenameHistoryResponse>, Status> {
        const METHOD: &str = "HistoryService/Rename";
        let activity = ActivityGuard::take(&mut request);
        let (id, title) = <(u64, Option<Title>)>::try_from(request.into_inner())
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let (command, pending) = call(|reply| Command::Rename(id, title, reply, activity));
        self.submit(METHOD, command, pending)
            .await
            .map(|entry| Response::new(entry.into()))
    }

    async fn regenerate(
        &self,
        mut request: Request<wire::RegenerateHistoryRequest>,
    ) -> Result<Response<wire::HistoryEntry>, Status> {
        const METHOD: &str = "HistoryService/Regenerate";
        let activity = ActivityGuard::take(&mut request);
        let (id, options) = <(u64, RetryOptions)>::try_from(request.into_inner())
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let (command, pending) =
            cancellable_call(|cancel, reply| Command::Retry(id, options, cancel, reply, activity));
        self.submit(METHOD, command, pending)
            .await
            .map(|entry| Response::new(entry.into()))
    }
}
