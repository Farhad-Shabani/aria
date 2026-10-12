//! The supervised owner of chat blocking work.
//!
//! Callers hold a [`ChatHandle`]; an ask is answered with a ticket at once
//! and runs on the blocking pool. Asks run side by side, except that asks
//! on one conversation run one after another: a resumed CLI session cannot
//! take two turns at once. A cancel ends one ask by ticket; daemon shutdown
//! cancels what is running.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::chat::{ChatRequest, SessionKey, Ticket};

use super::super::operation::{CallError, Reply, call, operation_cancellation, panic_message};
use super::super::tasks::Supervised;
use super::runtime::ChatRuntime;

/// A handoff slot, not a work queue: the lanes inside the actor are.
const COMMAND_CAPACITY: usize = 1;

/// An ask is answered with its ticket or not at all; nothing it does
/// before answering can fail.
pub(in crate::daemon) type ChatCallError = CallError<Infallible>;

struct AskCommand {
    request: ChatRequest,
    reply: Reply<Ticket, Infallible>,
}

/// Answered with whether the ticket was still pending.
struct CancelCommand {
    ticket: Ticket,
    reply: Reply<bool, Infallible>,
}

/// The application-facing entry point for chat. Cloneable; the channels
/// behind it are private. Cancels travel apart from asks, so one never
/// waits behind the other in the handoff slot.
#[derive(Clone)]
pub(in crate::daemon) struct ChatHandle {
    asks: mpsc::Sender<AskCommand>,
    cancels: mpsc::Sender<CancelCommand>,
}

impl ChatHandle {
    /// Answers as soon as the ask is taken; the reply follows as an event.
    pub(in crate::daemon) async fn ask(
        &self,
        request: ChatRequest,
    ) -> Result<Ticket, ChatCallError> {
        let (command, pending) = call(|reply| AskCommand { request, reply });
        self.asks
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }

    /// Ends the ask, running or waiting. `false` when it was neither.
    pub(in crate::daemon) async fn cancel(&self, ticket: Ticket) -> Result<bool, ChatCallError> {
        let (command, pending) = call(|reply| CancelCommand { ticket, reply });
        self.cancels
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }
}

pub(in crate::daemon) struct ChatActor {
    asks: mpsc::Receiver<AskCommand>,
    cancels: mpsc::Receiver<CancelCommand>,
    runtime: Arc<ChatRuntime>,
    next_ticket: u64,
    /// Asks waiting behind the one running on their session.
    lanes: HashMap<SessionKey, VecDeque<(Ticket, ChatRequest)>>,
    running: HashMap<SessionKey, Running>,
    /// Each job hands back its session key, so the lane can move on.
    jobs: JoinSet<SessionKey>,
}

/// The one ask on a session, and what a cancel pulls to end it.
struct Running {
    ticket: Ticket,
    token: CancelToken,
}

pub(in crate::daemon) fn channel(runtime: Arc<ChatRuntime>) -> (ChatHandle, ChatActor) {
    let (asks, ask_receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (cancels, cancel_receiver) = mpsc::channel(COMMAND_CAPACITY);
    (
        ChatHandle { asks, cancels },
        ChatActor {
            asks: ask_receiver,
            cancels: cancel_receiver,
            runtime,
            next_ticket: 1,
            lanes: HashMap::new(),
            running: HashMap::new(),
            jobs: JoinSet::new(),
        },
    )
}

impl Supervised for ChatActor {
    /// Runs until daemon shutdown or until every handle is gone, then drains:
    /// no started job outlives this future.
    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                Some(finished) = self.jobs.join_next() => self.finished(finished, &shutdown),
                Some(command) = self.cancels.recv() => self.cancel(command),
                ask = self.asks.recv() => match ask {
                    Some(command) => self.ask(command, &shutdown),
                    None => break,
                },
            }
        }
        self.drain().await;
    }
}

impl ChatActor {
    fn ask(&mut self, command: AskCommand, shutdown: &CancellationToken) {
        let AskCommand { request, reply } = command;
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        reply.answer(ticket);
        let key = request.key();
        if self.running.contains_key(&key) {
            self.lanes
                .entry(key)
                .or_default()
                .push_back((ticket, request));
        } else {
            self.start(key, ticket, request, shutdown);
        }
    }

    fn start(
        &mut self,
        key: SessionKey,
        ticket: Ticket,
        request: ChatRequest,
        shutdown: &CancellationToken,
    ) {
        let token = CancelToken::new();
        self.running.insert(
            key.clone(),
            Running {
                ticket,
                token: token.clone(),
            },
        );
        let runtime = Arc::clone(&self.runtime);
        let cancellation = operation_cancellation(token, shutdown.clone());
        self.jobs.spawn_blocking(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.execute(ticket, request, cancellation);
            }));
            if let Err(panic) = outcome {
                tracing::error!(%ticket, panic = panic_message(panic.as_ref()), "a chat job panicked");
            }
            key
        });
    }

    /// A running ask is pulled by its token and ends itself; a waiting one
    /// is lifted out of its lane and ended here, since no job will.
    fn cancel(&mut self, command: CancelCommand) {
        let CancelCommand { ticket, reply } = command;
        if let Some(job) = self.running.values().find(|job| job.ticket == ticket) {
            job.token.cancel();
            reply.answer(true);
            return;
        }
        match self.take_waiting(ticket) {
            Some(request) => {
                self.runtime.cancel_unstarted(ticket, request.agent);
                reply.answer(true);
            }
            None => reply.answer(false),
        }
    }

    /// Lifts an ask out of its lane, dropping a lane that empties.
    fn take_waiting(&mut self, ticket: Ticket) -> Option<ChatRequest> {
        let (key, index) = self.lanes.iter().find_map(|(key, lane)| {
            let index = lane.iter().position(|(queued, _)| *queued == ticket)?;
            Some((key.clone(), index))
        })?;
        let lane = self.lanes.get_mut(&key)?;
        let taken = lane.remove(index).map(|(_, request)| request);
        if lane.is_empty() {
            self.lanes.remove(&key);
        }
        taken
    }

    /// A finished job frees its session for the next ask waiting on it.
    fn finished(
        &mut self,
        finished: Result<SessionKey, tokio::task::JoinError>,
        shutdown: &CancellationToken,
    ) {
        let key = match finished {
            Ok(key) => key,
            Err(error) => {
                tracing::error!(%error, "a chat job ended unexpectedly");
                return;
            }
        };
        self.running.remove(&key);
        let next = self
            .lanes
            .get(&key)
            .and_then(|lane| lane.front())
            .map(|(ticket, _)| *ticket)
            .and_then(|ticket| Some((ticket, self.take_waiting(ticket)?)));
        if let Some((ticket, request)) = next
            && !shutdown.is_cancelled()
        {
            self.start(key, ticket, request, shutdown);
        }
    }

    /// Running jobs are cancelled through the shutdown token and waited
    /// out; waiting asks are dropped. Subscribers are gone by now, so
    /// nobody could hear a failure event for them.
    async fn drain(mut self) {
        self.asks.close();
        self.cancels.close();
        while let Some(command) = self.asks.recv().await {
            command.reply.unavailable();
        }
        while let Some(command) = self.cancels.recv().await {
            command.reply.unavailable();
        }
        let waiting: usize = self.lanes.values().map(VecDeque::len).sum();
        if waiting > 0 {
            tracing::warn!(waiting, "dropping asks that never started");
        }
        self.lanes.clear();
        while let Some(finished) = self.jobs.join_next().await {
            if let Err(error) = finished {
                tracing::error!(%error, "a chat job ended unexpectedly");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel as std_channel};
    use std::time::Duration;

    use crate::chat::ChatStageKind;
    use crate::rpc::RpcErrorCode;

    use super::super::runtime::tests::{ParkedAgent, request, runtime_with};
    use super::*;

    const PATIENCE: Duration = Duration::from_secs(5);

    struct Harness {
        handle: ChatHandle,
        started: Receiver<u64>,
        release: std::sync::mpsc::Sender<()>,
        events: Receiver<crate::chat::ChatStage>,
        shutdown: CancellationToken,
        actor: tokio::task::JoinHandle<()>,
    }

    fn harness(directory: &Path) -> Harness {
        let (started, started_receiver) = std_channel();
        let (release, release_receiver) = std_channel();
        let release_receiver = Arc::new(Mutex::new(release_receiver));
        let (runtime, events) = runtime_with(
            directory,
            Box::new(move |_agent| {
                Box::new(ParkedAgent {
                    started: started.clone(),
                    release: Arc::clone(&release_receiver),
                })
            }),
        );
        let (handle, actor) = channel(Arc::new(runtime));
        let shutdown = CancellationToken::new();
        let actor = tokio::spawn(actor.run(shutdown.clone()));
        Harness {
            handle,
            started: started_receiver,
            release,
            events,
            shutdown,
            actor,
        }
    }

    fn started_within(started: &Receiver<u64>, patience: Duration) -> Option<u64> {
        started.recv_timeout(patience).ok()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn asks_on_one_conversation_take_turns_while_others_overlap() {
        let directory = tempfile::tempdir().expect("test directory");
        let elsewhere = tempfile::tempdir().expect("another directory");
        let harness = harness(directory.path());

        let first = harness
            .handle
            .ask(request("1", directory.path(), false))
            .await
            .expect("the first is taken");
        let second = harness
            .handle
            .ask(request("2", directory.path(), false))
            .await
            .expect("the second is taken");
        let third = harness
            .handle
            .ask(request("3", elsewhere.path(), false))
            .await
            .expect("the third is taken");
        assert_eq!((first, second, third), (Ticket(1), Ticket(2), Ticket(3)));

        let mut running = vec![
            started_within(&harness.started, PATIENCE).expect("one starts"),
            started_within(&harness.started, PATIENCE).expect("another starts"),
        ];
        running.sort_unstable();
        assert_eq!(running, [1, 3], "the second waits behind the first");
        assert_eq!(
            started_within(&harness.started, Duration::from_millis(200)),
            None,
            "the second does not start while the first runs"
        );

        harness.release.send(()).expect("the first is released");
        harness.release.send(()).expect("the third is released");
        assert_eq!(
            started_within(&harness.started, PATIENCE),
            Some(2),
            "the second starts once its conversation is free"
        );
        harness.release.send(()).expect("the second is released");

        let mut replies: Vec<_> = (0..6)
            .map(|_| harness.events.recv_timeout(PATIENCE).expect("an event"))
            .filter_map(|stage| match stage.kind {
                ChatStageKind::Replied(reply) => Some((stage.ticket, reply.text)),
                _ => None,
            })
            .collect();
        replies.sort_unstable();
        assert_eq!(
            replies,
            [
                (Ticket(1), "reply 1".to_owned()),
                (Ticket(2), "reply 2".to_owned()),
                (Ticket(3), "reply 3".to_owned()),
            ]
        );

        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    fn ended_as(stage: &crate::chat::ChatStage, ticket: Ticket) -> Option<RpcErrorCode> {
        match &stage.kind {
            ChatStageKind::Failed { code, .. } if stage.ticket == ticket => Some(*code),
            _ => None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waiting_ask_is_cancelled_without_touching_the_running_one() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = harness(directory.path());

        harness
            .handle
            .ask(request("1", directory.path(), false))
            .await
            .expect("the first is taken");
        let second = harness
            .handle
            .ask(request("2", directory.path(), false))
            .await
            .expect("the second is taken");
        assert_eq!(started_within(&harness.started, PATIENCE), Some(1));

        assert!(harness.handle.cancel(second).await.expect("answered"));
        assert!(!harness.handle.cancel(second).await.expect("answered"));

        // The running ask's opening stage may sit ahead of the refusal.
        let refused = (0..3)
            .filter_map(|_| harness.events.recv_timeout(PATIENCE).ok())
            .find_map(|stage| ended_as(&stage, second));
        assert_eq!(refused, Some(RpcErrorCode::Cancelled));

        harness.release.send(()).expect("the first is released");
        let replied = (0..3)
            .filter_map(|_| harness.events.recv_timeout(PATIENCE).ok())
            .find(|stage| matches!(stage.kind, ChatStageKind::Replied(..)))
            .map(|stage| stage.ticket);
        assert_eq!(replied, Some(Ticket(1)));
        assert_eq!(
            started_within(&harness.started, Duration::from_millis(200)),
            None,
            "the cancelled ask never starts"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_ask_is_cancelled_and_frees_its_conversation() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = harness(directory.path());

        let first = harness
            .handle
            .ask(request("1", directory.path(), false))
            .await
            .expect("the first is taken");
        assert_eq!(started_within(&harness.started, PATIENCE), Some(1));

        assert!(harness.handle.cancel(first).await.expect("answered"));
        let ended = (0..3)
            .filter_map(|_| harness.events.recv_timeout(PATIENCE).ok())
            .find_map(|stage| ended_as(&stage, first));
        assert_eq!(ended, Some(RpcErrorCode::Cancelled));

        harness
            .handle
            .ask(request("2", directory.path(), false))
            .await
            .expect("the second is taken");
        assert_eq!(
            started_within(&harness.started, PATIENCE),
            Some(2),
            "the conversation is free again"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drops_waiting_asks_and_waits_out_the_running_one() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = harness(directory.path());

        harness
            .handle
            .ask(request("1", directory.path(), false))
            .await
            .expect("the first is taken");
        harness
            .handle
            .ask(request("2", directory.path(), false))
            .await
            .expect("the second is taken");
        assert_eq!(started_within(&harness.started, PATIENCE), Some(1));

        harness.shutdown.cancel();
        harness.release.send(()).expect("the first is released");
        harness.actor.await.expect("the actor drains");

        assert_eq!(
            started_within(&harness.started, Duration::from_millis(200)),
            None,
            "the waiting ask never starts"
        );
        assert!(
            harness
                .handle
                .ask(request("3", directory.path(), false))
                .await
                .is_err(),
            "the handle is closed"
        );
    }
}
