//! The supervised owner of chat blocking work.
//!
//! Callers hold a [`ChatHandle`]. An ask or a prompt run is answered with a ticket at once and runs
//! on the blocking pool. Saved text to type is answered once typed. Asks on one conversation run
//! one after another, since a resumed CLI session can't take two turns at once. Every other job
//! runs side by side. Typing takes turns in the runtime, so two texts never type into the same
//! field at once. A run that types nothing waits for nobody. A cancel ends one job by ticket.
//! Daemon shutdown cancels what is running.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use aria_assistant::chat::{ChatRequest, DeliverCall, PromptRequest, SessionKey, Ticket};
use aria_platform::delivery::{DeliveryError, DeliveryOutcome};

use super::super::operation::{
    CallError, Reply, call, cancellable_call, operation_cancellation, panic_message,
};
use super::super::tasks::Supervised;
use super::error::ChatError;
use super::runtime::{ChatJob, ChatRuntime};

/// A handoff slot, not a work queue: the lanes inside the actor are.
const COMMAND_CAPACITY: usize = 1;

/// A job is answered with its ticket or not at all; nothing it does
/// before answering can fail.
pub(in crate::daemon) type ChatCallError = CallError<Infallible>;

/// Typing saved text fails with the same [`ChatError`] a reply's typing does.
pub(in crate::daemon) type DeliverCallError = CallError<ChatError>;

enum SubmitCommand {
    /// Answered with a ticket at once. The outcome follows as events.
    Run {
        job: ChatJob,
        reply: Reply<Ticket, Infallible>,
    },
    /// Answered once the text is typed.
    Deliver(DeliverJob),
}

struct DeliverJob {
    call: DeliverCall,
    cancellation: CancelToken,
    reply: Reply<DeliveryOutcome, ChatError>,
}

/// What waits in a lane and runs there. A delivery's ticket is never handed out. It only keys the
/// delivery's lane.
enum Job {
    Run(ChatJob),
    Deliver(DeliverJob),
}

impl Job {
    fn tap_cancels(&self) -> bool {
        match self {
            Self::Run(job) => job.tap_cancels(),
            Self::Deliver(_) => false,
        }
    }
}

enum Cancel {
    Ticket(Ticket),
    /// The newest job in flight that a tap of the Aria key may end. Two at
    /// once, an ask and a prompt run, take one tap each, newest first.
    Tapped,
}

/// Answered with whether the job was still pending.
struct CancelCommand {
    target: Cancel,
    reply: Reply<bool, Infallible>,
}

/// The application-facing entry point for chat. Cloneable; the channels
/// behind it are private. Cancels travel apart from jobs, so one never
/// waits behind the other in the handoff slot.
#[derive(Clone)]
pub(in crate::daemon) struct ChatHandle {
    submissions: mpsc::Sender<SubmitCommand>,
    cancels: mpsc::Sender<CancelCommand>,
}

impl ChatHandle {
    /// Answers as soon as the ask is taken; the reply follows as an event.
    pub(in crate::daemon) async fn ask(
        &self,
        request: ChatRequest,
    ) -> Result<Ticket, ChatCallError> {
        self.submit(ChatJob::Ask(request)).await
    }

    /// Answers as soon as the run is taken; the outcome follows as an
    /// event, as for an ask.
    pub(in crate::daemon) async fn run_prompt(
        &self,
        request: PromptRequest,
    ) -> Result<Ticket, ChatCallError> {
        self.submit(ChatJob::Prompt(request)).await
    }

    async fn submit(&self, job: ChatJob) -> Result<Ticket, ChatCallError> {
        let (command, pending) = call(|reply| SubmitCommand::Run { job, reply });
        self.submissions
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }

    /// Answers once the text is typed, after any text already being typed.
    /// Dropping the future cancels the delivery.
    pub(in crate::daemon) async fn deliver(
        &self,
        call: DeliverCall,
    ) -> Result<DeliveryOutcome, DeliverCallError> {
        let (command, pending) = cancellable_call(|cancellation, reply| {
            SubmitCommand::Deliver(DeliverJob {
                call,
                cancellation,
                reply,
            })
        });
        self.submissions
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }

    /// Ends the job, running or waiting. `false` when it was neither.
    pub(in crate::daemon) async fn cancel(&self, ticket: Ticket) -> Result<bool, ChatCallError> {
        self.send_cancel(Cancel::Ticket(ticket)).await
    }

    /// Ends the newest job a tap may end. `false` when none is in flight.
    pub(in crate::daemon) async fn cancel_tapped(&self) -> Result<bool, ChatCallError> {
        self.send_cancel(Cancel::Tapped).await
    }

    async fn send_cancel(&self, target: Cancel) -> Result<bool, ChatCallError> {
        let (command, pending) = call(|reply| CancelCommand { target, reply });
        self.cancels
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }

    /// A handle nobody serves; every call reports Unavailable.
    #[cfg(test)]
    pub(in crate::daemon) fn detached() -> Self {
        let (submissions, _) = mpsc::channel(COMMAND_CAPACITY);
        let (cancels, _) = mpsc::channel(COMMAND_CAPACITY);
        Self {
            submissions,
            cancels,
        }
    }
}

pub(in crate::daemon) struct ChatActor {
    submissions: mpsc::Receiver<SubmitCommand>,
    cancels: mpsc::Receiver<CancelCommand>,
    runtime: Arc<ChatRuntime>,
    next_ticket: u64,
    /// Jobs waiting behind the one running in their lane.
    lanes: HashMap<Lane, VecDeque<(Ticket, Job)>>,
    running: HashMap<Lane, Running>,
    /// Each job hands back its lane, so the lane can move on.
    jobs: JoinSet<Lane>,
}

/// Jobs in one lane run one after another.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Lane {
    Session(SessionKey),
    /// A job no other waits behind. Jobs that type take turns only while
    /// typing, in the runtime.
    Alone(Ticket),
}

impl Lane {
    fn of(ticket: Ticket, job: &Job) -> Self {
        match job {
            Job::Run(ChatJob::Ask(request)) => Self::Session(request.key()),
            Job::Run(ChatJob::Prompt(_)) | Job::Deliver(_) => Self::Alone(ticket),
        }
    }
}

/// The one job running in a lane, and what a cancel pulls to end it.
struct Running {
    ticket: Ticket,
    tap_cancels: bool,
    token: CancelToken,
}

pub(in crate::daemon) fn channel(runtime: Arc<ChatRuntime>) -> (ChatHandle, ChatActor) {
    let (submissions, submission_receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (cancels, cancel_receiver) = mpsc::channel(COMMAND_CAPACITY);
    (
        ChatHandle {
            submissions,
            cancels,
        },
        ChatActor {
            submissions: submission_receiver,
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
                submitted = self.submissions.recv() => match submitted {
                    Some(command) => self.admit(command, &shutdown),
                    None => break,
                },
            }
        }
        self.drain().await;
    }
}

impl ChatActor {
    fn admit(&mut self, command: SubmitCommand, shutdown: &CancellationToken) {
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        let job = match command {
            SubmitCommand::Run { job, reply } => {
                reply.answer(ticket);
                Job::Run(job)
            }
            SubmitCommand::Deliver(job) => Job::Deliver(job),
        };
        let lane = Lane::of(ticket, &job);
        if self.running.contains_key(&lane) {
            self.lanes.entry(lane).or_default().push_back((ticket, job));
        } else {
            self.start(lane, ticket, job, shutdown);
        }
    }

    fn start(&mut self, key: Lane, ticket: Ticket, job: Job, shutdown: &CancellationToken) {
        let token = match &job {
            Job::Run(_) => CancelToken::new(),
            Job::Deliver(deliver) => deliver.cancellation.clone(),
        };
        self.running.insert(
            key.clone(),
            Running {
                ticket,
                tap_cancels: job.tap_cancels(),
                token: token.clone(),
            },
        );
        let runtime = Arc::clone(&self.runtime);
        let cancellation = operation_cancellation(token, shutdown.clone());
        self.jobs.spawn_blocking(move || {
            match job {
                Job::Run(job) => {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        runtime.execute(ticket, job, cancellation);
                    }));
                    if let Err(panic) = outcome {
                        tracing::error!(%ticket, panic = panic_message(panic.as_ref()), "a chat job panicked");
                    }
                }
                Job::Deliver(DeliverJob { call, reply, .. }) => {
                    reply.run("chat delivery", || runtime.deliver(&call, &cancellation));
                }
            }
            key
        });
    }

    /// A running job is pulled by its token and ends itself; a waiting one
    /// is lifted out of its lane and ended here, since no job will.
    fn cancel(&mut self, command: CancelCommand) {
        let CancelCommand { target, reply } = command;
        let found = match target {
            Cancel::Ticket(ticket) => self.cancel_ticket(ticket),
            Cancel::Tapped => self
                .tapped()
                .is_some_and(|ticket| self.cancel_ticket(ticket)),
        };
        reply.answer(found);
    }

    fn cancel_ticket(&mut self, ticket: Ticket) -> bool {
        if let Some(job) = self.running.values().find(|job| job.ticket == ticket) {
            job.token.cancel();
            return true;
        }
        match self.take_waiting(ticket) {
            Some(Job::Run(job)) => {
                self.runtime.cancel_unstarted(ticket, job.assistant_id());
                true
            }
            Some(Job::Deliver(job)) => {
                job.reply
                    .fail(ChatError::Delivery(DeliveryError::Cancelled));
                true
            }
            None => false,
        }
    }

    /// The newest job a tap may end, running or waiting. A job already
    /// told to stop is still winding down, so the next tap moves on.
    fn tapped(&self) -> Option<Ticket> {
        let running = self
            .running
            .values()
            .filter(|job| job.tap_cancels && !job.token.is_cancelled())
            .map(|job| job.ticket);
        let waiting = self
            .lanes
            .values()
            .flatten()
            .filter(|(_, job)| job.tap_cancels())
            .map(|(ticket, _)| *ticket);
        running.chain(waiting).max()
    }

    /// Lifts a job out of its lane, dropping a lane that empties.
    fn take_waiting(&mut self, ticket: Ticket) -> Option<Job> {
        let (key, index) = self.lanes.iter().find_map(|(key, lane)| {
            let index = lane.iter().position(|(queued, _)| *queued == ticket)?;
            Some((key.clone(), index))
        })?;
        let lane = self.lanes.get_mut(&key)?;
        let taken = lane.remove(index).map(|(_, job)| job);
        if lane.is_empty() {
            self.lanes.remove(&key);
        }
        taken
    }

    /// A finished job frees its lane for the next job waiting in it.
    fn finished(
        &mut self,
        finished: Result<Lane, tokio::task::JoinError>,
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
        if let Some((ticket, job)) = next
            && !shutdown.is_cancelled()
        {
            self.start(key, ticket, job, shutdown);
        }
    }

    /// Running jobs are cancelled through the shutdown token and waited out. Waiting jobs are
    /// dropped. Subscribers are gone by now, so nobody could hear a failure event for them. A
    /// waiting delivery's caller hears Unavailable.
    async fn drain(mut self) {
        self.submissions.close();
        self.cancels.close();
        while let Some(command) = self.submissions.recv().await {
            match command {
                SubmitCommand::Run { reply, .. } => reply.unavailable(),
                SubmitCommand::Deliver(job) => job.reply.unavailable(),
            }
        }
        while let Some(command) = self.cancels.recv().await {
            command.reply.unavailable();
        }
        let waiting: usize = self.lanes.values().map(VecDeque::len).sum();
        if waiting > 0 {
            tracing::warn!(waiting, "dropping chat jobs that never started");
        }
        for (_, job) in self.lanes.drain().flat_map(|(_, lane)| lane) {
            if let Job::Deliver(job) = job {
                job.reply.unavailable();
            }
        }
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

    use aria_assistant::chat::ChatStageKind;
    use aria_platform::delivery::OutputMode;
    use aria_settings::rpc::RpcErrorCode;

    use super::super::runtime::tests::{
        DELIVERY, DELIVERY_CANCELLED, ParkedAssistant, request, runtime_with,
    };
    use super::*;

    const PATIENCE: Duration = Duration::from_secs(5);

    struct Harness {
        handle: ChatHandle,
        started: Receiver<u64>,
        release: std::sync::mpsc::Sender<()>,
        events: Receiver<aria_assistant::chat::ChatStage>,
        shutdown: CancellationToken,
        actor: tokio::task::JoinHandle<()>,
    }

    impl Harness {
        fn spawn_delivery(
            &self,
        ) -> tokio::task::JoinHandle<Result<DeliveryOutcome, DeliverCallError>> {
            let handle = self.handle.clone();
            tokio::spawn(async move { handle.deliver(deliver_call()).await })
        }
    }

    /// Asks park in the assistant, then reply. Prompt runs fail at once.
    fn harness(directory: &Path) -> Harness {
        harness_with(directory, false)
    }

    /// Every job parks, prompt runs included, and none replies.
    fn parking_harness(directory: &Path) -> Harness {
        harness_with(directory, true)
    }

    fn harness_with(directory: &Path, parks_every_job: bool) -> Harness {
        let (started, started_receiver) = std_channel();
        let (release, release_receiver) = std_channel();
        let release_receiver = Arc::new(Mutex::new(release_receiver));
        let parked = (started.clone(), Arc::clone(&release_receiver));
        let (mut runtime, events) = runtime_with(directory, move |_assistant_id| ParkedAssistant {
            started: started.clone(),
            release: Arc::clone(&release_receiver),
        });
        if parks_every_job {
            runtime = runtime.parking(parked.0, parked.1);
        }
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

    fn ended_as(stage: &aria_assistant::chat::ChatStage, ticket: Ticket) -> Option<RpcErrorCode> {
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

    fn pressed_request(text: &str, workdir: &Path) -> ChatRequest {
        ChatRequest {
            tap_cancels: true,
            ..request(text, workdir, false)
        }
    }

    fn ended_within(harness: &Harness, ticket: Ticket) -> Option<RpcErrorCode> {
        (0..4)
            .filter_map(|_| harness.events.recv_timeout(PATIENCE).ok())
            .find_map(|stage| ended_as(&stage, ticket))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tap_ends_the_newest_run_a_press_started() {
        let directory = tempfile::tempdir().expect("test directory");
        let elsewhere = tempfile::tempdir().expect("another directory");
        let harness = harness(directory.path());

        let older = harness
            .handle
            .ask(pressed_request("1", directory.path()))
            .await
            .expect("the older is taken");
        let newer = harness
            .handle
            .ask(pressed_request("2", elsewhere.path()))
            .await
            .expect("the newer is taken");
        assert!(started_within(&harness.started, PATIENCE).is_some());
        assert!(started_within(&harness.started, PATIENCE).is_some());

        assert!(harness.handle.cancel_tapped().await.expect("answered"));
        assert_eq!(ended_within(&harness, newer), Some(RpcErrorCode::Cancelled));

        assert!(harness.handle.cancel_tapped().await.expect("answered"));
        assert_eq!(ended_within(&harness, older), Some(RpcErrorCode::Cancelled));

        assert!(
            !harness.handle.cancel_tapped().await.expect("answered"),
            "nothing is left for a tap to end"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tap_leaves_a_run_no_press_started_alone() {
        let directory = tempfile::tempdir().expect("test directory");
        let elsewhere = tempfile::tempdir().expect("another directory");
        let harness = harness(directory.path());

        let pressed = harness
            .handle
            .ask(pressed_request("1", directory.path()))
            .await
            .expect("the pressed ask is taken");
        let typed = harness
            .handle
            .ask(request("2", elsewhere.path(), false))
            .await
            .expect("the typed ask is taken");
        assert!(started_within(&harness.started, PATIENCE).is_some());
        assert!(started_within(&harness.started, PATIENCE).is_some());

        assert!(harness.handle.cancel_tapped().await.expect("answered"));
        assert_eq!(
            ended_within(&harness, pressed),
            Some(RpcErrorCode::Cancelled),
            "the older pressed ask ends, not the newer typed one"
        );

        let waiting = harness
            .handle
            .ask(pressed_request("3", elsewhere.path()))
            .await
            .expect("the waiting pressed ask is taken");
        assert!(harness.handle.cancel_tapped().await.expect("answered"));
        assert_eq!(
            ended_within(&harness, waiting),
            Some(RpcErrorCode::Cancelled),
            "the pressed ask waiting behind the typed one ends"
        );
        assert!(
            !harness.handle.cancel_tapped().await.expect("answered"),
            "a typed ask is not a tap's to end"
        );

        harness.release.send(()).expect("the typed ask is released");
        let replied = (0..3)
            .filter_map(|_| harness.events.recv_timeout(PATIENCE).ok())
            .find(|stage| matches!(stage.kind, ChatStageKind::Replied(..)))
            .map(|stage| stage.ticket);
        assert_eq!(replied, Some(typed));
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

    fn prompt(output_mode: Option<OutputMode>) -> PromptRequest {
        PromptRequest {
            text: "hi".to_owned(),
            name: "Polish".to_owned(),
            entry: None,
            kind: aria_settings::PromptKind::Rewrite,
            instructions: aria_assistant::chat::PromptInstructions::File("/p.md".into()),
            assistant_id: aria_settings::config::AssistantId::ClaudeCode,
            model_id: None,
            application: None,
            tap_cancels: false,
            output_mode,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_runs_that_type_think_side_by_side() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = parking_harness(directory.path());

        let first = harness
            .handle
            .run_prompt(prompt(Some(OutputMode::InsertOrCopy)))
            .await
            .expect("the first is taken");
        let second = harness
            .handle
            .run_prompt(prompt(Some(OutputMode::Insert)))
            .await
            .expect("the second is taken");
        let mut started = [
            started_within(&harness.started, PATIENCE),
            started_within(&harness.started, PATIENCE),
        ];
        started.sort();
        assert_eq!(
            started,
            [Some(first.0), Some(second.0)],
            "the second runs while the first's assistant does; only their typing takes turns"
        );

        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    fn deliver_call() -> DeliverCall {
        DeliverCall {
            text: "Polished.".to_owned(),
            application: None,
            output_mode: OutputMode::InsertOrCopy,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saved_text_does_not_wait_for_a_run_still_thinking() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = parking_harness(directory.path());

        let typing = harness
            .handle
            .run_prompt(prompt(Some(OutputMode::InsertOrCopy)))
            .await
            .expect("the typing run is taken");
        assert_eq!(started_within(&harness.started, PATIENCE), Some(typing.0));
        let delivery = harness.spawn_delivery();
        assert_eq!(
            started_within(&harness.started, PATIENCE),
            Some(DELIVERY),
            "the text starts while the run's assistant still works"
        );

        harness.release.send(()).expect("one job is released");
        harness.release.send(()).expect("the other is released");
        let outcome = delivery.await.expect("the call returns");
        assert!(
            matches!(outcome, Ok(DeliveryOutcome::Inserted)),
            "{outcome:?}"
        );

        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_cancels_saved_text_being_typed() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = parking_harness(directory.path());

        let delivery = harness.spawn_delivery();
        assert_eq!(started_within(&harness.started, PATIENCE), Some(DELIVERY));

        harness.shutdown.cancel();
        let outcome = tokio::time::timeout(PATIENCE, delivery)
            .await
            .expect("the delivery ends without being released")
            .expect("the call returns");
        assert!(
            matches!(
                outcome,
                Err(CallError::Runtime(ChatError::Delivery(
                    DeliveryError::Cancelled
                )))
            ),
            "{outcome:?}"
        );
        harness.actor.await.expect("the actor drains");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_that_goes_away_cancels_its_saved_text() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = parking_harness(directory.path());

        let delivery = harness.spawn_delivery();
        assert_eq!(started_within(&harness.started, PATIENCE), Some(DELIVERY));
        delivery.abort();

        assert_eq!(
            started_within(&harness.started, PATIENCE),
            Some(DELIVERY_CANCELLED),
            "the delivery ends without being released"
        );

        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }
}
