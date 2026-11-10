//! The supervised owner of chat blocking work.
//!
//! Callers hold a [`ChatHandle`]. An ask or a prompt run is answered with a ticket at once and runs
//! on the blocking pool. Saved text to type is answered once typed. Asks on one chat run
//! one after another, since a resumed CLI session can't take two turns at once. Every other job
//! runs side by side. Typing takes turns in the runtime, so two texts never type into the same
//! field at once. A run that types nothing waits for nobody. A cancel ends one job by ticket, or
//! every ask in one chat.
//! Deleting a chat takes its lane too, and only when no ask runs or waits there, so no
//! reply saves after the delete. Renaming one takes the lanes of both its names, so no ask
//! starts under either name halfway through. Daemon shutdown cancels what is running.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use aria_assistant::chat::{DeliverCall, PendingAsk, PromptRequest, SessionKey, Ticket};
use aria_platform::delivery::{DeliveryError, DeliveryOutcome};
use aria_settings::{ChatKey, ChatName};

use super::super::operation::{
    CallError, Reply, call, cancellable_call, operation_cancellation, panic_message,
};
use super::super::tasks::Supervised;
use super::error::ChatError;
use super::runtime::{AskJob, ChatJob, ChatRuntime};

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
    /// Answered once the chat is gone, or at once while it replies.
    Delete(DeleteJob),
    /// Answered once the chat has its new name, or at once while either
    /// name has an ask.
    Rename(RenameJob),
}

struct DeliverJob {
    call: DeliverCall,
    cancellation: CancelToken,
    reply: Reply<DeliveryOutcome, ChatError>,
}

struct DeleteJob {
    chat: ChatKey,
    reply: Reply<bool, ChatError>,
}

struct RenameJob {
    chat: ChatKey,
    name: ChatName,
    reply: Reply<ChatKey, ChatError>,
}

impl RenameJob {
    /// The lane of the chat's new name.
    fn target(&self) -> Lane {
        Lane::Session(SessionKey::named(&ChatKey {
            assistant_id: self.chat.assistant_id,
            name: self.name.clone(),
        }))
    }
}

/// What waits in a lane and runs there. A delivery's, a delete's, or a rename's ticket is never
/// handed out. It only keys the job's lane. A delete or a rename never waits: admission refuses it
/// while a lane it needs is taken.
enum Job {
    Run(ChatJob),
    Deliver(DeliverJob),
    Delete(DeleteJob),
    Rename(RenameJob),
}

impl Job {
    fn tap_cancels(&self) -> bool {
        match self {
            Self::Run(job) => job.tap_cancels(),
            Self::Deliver(_) | Self::Delete(_) | Self::Rename(_) => false,
        }
    }

    /// A lane besides its own that the job holds while it runs.
    fn second_lane(&self, lane: &Lane) -> Option<Lane> {
        match self {
            Self::Rename(job) => Some(job.target()).filter(|target| target != lane),
            Self::Run(_) | Self::Deliver(_) | Self::Delete(_) => None,
        }
    }
}

enum Cancel {
    Ticket(Ticket),
    /// The newest job in flight that a tap of the Aria key may end. Two at
    /// once, a chat and a prompt run, take one tap each, newest first.
    Tapped,
    /// The chat's running ask and every ask waiting behind it.
    Chat(SessionKey),
}

/// Commands on jobs already taken. They travel apart from submissions, so
/// one never waits behind the other in the handoff slot.
enum Control {
    /// Answered with how many jobs it ended.
    Cancel(Cancel, Reply<u32, Infallible>),
    /// Answered with the chat's asks in the order they run.
    Pending(SessionKey, Reply<Vec<PendingAsk>, Infallible>),
}

/// The application-facing entry point for chat. Cloneable; the channels
/// behind it are private.
#[derive(Clone)]
pub(in crate::daemon) struct ChatHandle {
    submissions: mpsc::Sender<SubmitCommand>,
    controls: mpsc::Sender<Control>,
}

impl ChatHandle {
    /// Answers as soon as the ask is taken, with the named chat it
    /// continues; the reply follows as an event.
    pub(in crate::daemon) async fn ask(
        &self,
        job: AskJob,
    ) -> Result<(Ticket, Option<ChatKey>), ChatCallError> {
        let chat = job.chat();
        let ticket = self.submit(ChatJob::Ask(job)).await?;
        Ok((ticket, chat))
    }

    /// Answers as soon as the run is taken; the outcome follows as an
    /// event, as for a chat.
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

    /// Deletes a chat's asks from history and forgets what resumes
    /// it, and answers whether the daemon knew it. Refused with
    /// [`ChatError::Replying`] while an ask on it runs or waits.
    pub(in crate::daemon) async fn delete_chat(
        &self,
        chat: ChatKey,
    ) -> Result<bool, CallError<ChatError>> {
        let (command, pending) = call(|reply| SubmitCommand::Delete(DeleteJob { chat, reply }));
        self.submissions
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }

    /// Moves a chat's asks, folder, and assistant session to `name`, and
    /// answers the chat under it. Refused with [`ChatError::Replying`] while
    /// an ask on either name runs or waits.
    pub(in crate::daemon) async fn rename_chat(
        &self,
        chat: ChatKey,
        name: ChatName,
    ) -> Result<ChatKey, CallError<ChatError>> {
        let (command, pending) =
            call(|reply| SubmitCommand::Rename(RenameJob { chat, name, reply }));
        self.submissions
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }

    /// Ends the job, running or waiting. `false` when it was neither.
    pub(in crate::daemon) async fn cancel(&self, ticket: Ticket) -> Result<bool, ChatCallError> {
        Ok(self.send_cancel(Cancel::Ticket(ticket)).await? > 0)
    }

    /// Ends the newest job a tap may end. `false` when none is in flight.
    pub(in crate::daemon) async fn cancel_tapped(&self) -> Result<bool, ChatCallError> {
        Ok(self.send_cancel(Cancel::Tapped).await? > 0)
    }

    /// Ends the chat's running ask and every ask waiting behind it.
    /// Answers how many it ended; one already told to stop isn't counted.
    pub(in crate::daemon) async fn cancel_chat(
        &self,
        chat: &ChatKey,
    ) -> Result<u32, ChatCallError> {
        self.send_cancel(Cancel::Chat(SessionKey::named(chat)))
            .await
    }

    /// The chat's asks still running or waiting, in the order they run.
    pub(in crate::daemon) async fn pending(
        &self,
        chat: &ChatKey,
    ) -> Result<Vec<PendingAsk>, ChatCallError> {
        let key = SessionKey::named(chat);
        self.send_control(|reply| Control::Pending(key, reply))
            .await
    }

    async fn send_cancel(&self, target: Cancel) -> Result<u32, ChatCallError> {
        self.send_control(|reply| Control::Cancel(target, reply))
            .await
    }

    async fn send_control<T>(
        &self,
        command: impl FnOnce(Reply<T, Infallible>) -> Control,
    ) -> Result<T, ChatCallError> {
        let (command, pending) = call(command);
        self.controls
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }

    /// A handle nobody serves; every call reports Unavailable.
    #[cfg(test)]
    pub(in crate::daemon) fn detached() -> Self {
        let (submissions, _) = mpsc::channel(COMMAND_CAPACITY);
        let (controls, _) = mpsc::channel(COMMAND_CAPACITY);
        Self {
            submissions,
            controls,
        }
    }
}

pub(in crate::daemon) struct ChatActor {
    submissions: mpsc::Receiver<SubmitCommand>,
    controls: mpsc::Receiver<Control>,
    runtime: Arc<ChatRuntime>,
    next_ticket: u64,
    /// Jobs waiting behind the one running in their lane.
    lanes: HashMap<Lane, VecDeque<(Ticket, Job)>>,
    running: HashMap<Lane, Running>,
    /// Each job hands back the lanes it held, so they can move on.
    jobs: JoinSet<Held>,
}

/// A job's own lane, and the second one a rename holds.
struct Held {
    lane: Lane,
    second: Option<Lane>,
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
            Job::Run(ChatJob::Ask(job)) => Self::Session(job.key()),
            Job::Delete(job) => Self::Session(SessionKey::named(&job.chat)),
            Job::Rename(job) => Self::Session(SessionKey::named(&job.chat)),
            Job::Run(ChatJob::Prompt(_)) | Job::Deliver(_) => Self::Alone(ticket),
        }
    }
}

/// The one job running in a lane, and what a cancel pulls to end it.
struct Running {
    ticket: Ticket,
    kind: Kind,
    tap_cancels: bool,
    token: CancelToken,
}

/// What a running job is, which decides what a chat's cancel or
/// listing sees of it.
enum Kind {
    Ask { text: String },
    Prompt,
    Deliver,
    Delete,
    Rename,
}

impl Kind {
    fn of(job: &Job) -> Self {
        match job {
            Job::Run(ChatJob::Ask(job)) => Self::Ask {
                text: job.text().to_owned(),
            },
            Job::Run(ChatJob::Prompt(_)) => Self::Prompt,
            Job::Deliver(_) => Self::Deliver,
            Job::Delete(_) => Self::Delete,
            Job::Rename(_) => Self::Rename,
        }
    }
}

pub(in crate::daemon) fn channel(runtime: Arc<ChatRuntime>) -> (ChatHandle, ChatActor) {
    let (submissions, submission_receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (controls, control_receiver) = mpsc::channel(COMMAND_CAPACITY);
    (
        ChatHandle {
            submissions,
            controls,
        },
        ChatActor {
            submissions: submission_receiver,
            controls: control_receiver,
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
                Some(command) = self.controls.recv() => self.control(command),
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
            SubmitCommand::Delete(job) => Job::Delete(job),
            SubmitCommand::Rename(job) => Job::Rename(job),
        };
        let lane = Lane::of(ticket, &job);
        let second = job.second_lane(&lane);
        // Nothing waits in a lane with no job running, so this covers both.
        let taken = |lane: &Lane| self.running.contains_key(lane);
        match job {
            Job::Delete(job) if taken(&lane) => job.reply.fail(ChatError::Replying),
            Job::Rename(job) if taken(&lane) || second.as_ref().is_some_and(taken) => {
                job.reply.fail(ChatError::Replying);
            }
            job if taken(&lane) => self.lanes.entry(lane).or_default().push_back((ticket, job)),
            job => self.start(lane, ticket, job, shutdown),
        }
    }

    fn start(&mut self, key: Lane, ticket: Ticket, job: Job, shutdown: &CancellationToken) {
        let token = match &job {
            Job::Run(_) | Job::Delete(_) | Job::Rename(_) => CancelToken::new(),
            Job::Deliver(deliver) => deliver.cancellation.clone(),
        };
        let second = job.second_lane(&key);
        for lane in std::iter::once(&key).chain(&second) {
            self.running.insert(
                lane.clone(),
                Running {
                    ticket,
                    kind: Kind::of(&job),
                    tap_cancels: job.tap_cancels(),
                    token: token.clone(),
                },
            );
        }
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
                Job::Delete(DeleteJob {
                    chat,
                    reply,
                }) => {
                    reply.run("chat delete", || {
                        runtime.delete_chat(&chat)
                    });
                }
                Job::Rename(RenameJob { chat, name, reply }) => {
                    reply.run("chat rename", || runtime.rename_chat(&chat, name));
                }
            }
            Held { lane: key, second }
        });
    }

    fn control(&mut self, command: Control) {
        match command {
            Control::Cancel(target, reply) => reply.answer(self.cancel(target)),
            Control::Pending(key, reply) => reply.answer(self.pending(&Lane::Session(key))),
        }
    }

    /// A running job is pulled by its token and ends itself; a waiting one
    /// is lifted out of its lane and ended here, since no job will.
    fn cancel(&mut self, target: Cancel) -> u32 {
        match target {
            Cancel::Ticket(ticket) => u32::from(self.cancel_ticket(ticket)),
            Cancel::Tapped => self
                .tapped()
                .map_or(0, |ticket| u32::from(self.cancel_ticket(ticket))),
            Cancel::Chat(key) => self.cancel_lane(&Lane::Session(key)),
        }
    }

    fn cancel_ticket(&mut self, ticket: Ticket) -> bool {
        if let Some(job) = self.running.values().find(|job| job.ticket == ticket) {
            job.token.cancel();
            return true;
        }
        match self.take_waiting(ticket) {
            Some(job) => {
                self.end_waiting(ticket, job);
                true
            }
            None => false,
        }
    }

    /// Pulls the running ask's token, then ends every job waiting behind it.
    fn cancel_lane(&mut self, lane: &Lane) -> u32 {
        let running = self
            .running
            .get(lane)
            .filter(|job| matches!(job.kind, Kind::Ask { .. }) && !job.token.is_cancelled());
        let mut ended = 0;
        if let Some(job) = running {
            job.token.cancel();
            ended += 1;
        }
        for (ticket, job) in self.lanes.remove(lane).into_iter().flatten() {
            self.end_waiting(ticket, job);
            ended += 1;
        }
        ended
    }

    /// Ends a job lifted out of its lane before it started.
    fn end_waiting(&self, ticket: Ticket, job: Job) {
        match job {
            Job::Run(job) => self.runtime.cancel_unstarted(ticket, job.assistant_id()),
            Job::Deliver(job) => job
                .reply
                .fail(ChatError::Delivery(DeliveryError::Cancelled)),
            Job::Delete(job) => job.reply.fail(ChatError::Replying),
            Job::Rename(job) => job.reply.fail(ChatError::Replying),
        }
    }

    /// The lane's running ask, then the asks waiting behind it.
    fn pending(&self, lane: &Lane) -> Vec<PendingAsk> {
        let running = self.running.get(lane).and_then(|job| match &job.kind {
            Kind::Ask { text } => Some(PendingAsk {
                ticket: job.ticket,
                text: text.clone(),
                running: true,
            }),
            Kind::Prompt | Kind::Deliver | Kind::Delete | Kind::Rename => None,
        });
        let waiting = self
            .lanes
            .get(lane)
            .into_iter()
            .flatten()
            .filter_map(|(ticket, job)| match job {
                Job::Run(ChatJob::Ask(job)) => Some(PendingAsk {
                    ticket: *ticket,
                    text: job.text().to_owned(),
                    running: false,
                }),
                Job::Run(ChatJob::Prompt(_))
                | Job::Deliver(_)
                | Job::Delete(_)
                | Job::Rename(_) => None,
            });
        running.into_iter().chain(waiting).collect()
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

    /// A finished job frees its lanes for the next job waiting in each.
    fn finished(
        &mut self,
        finished: Result<Held, tokio::task::JoinError>,
        shutdown: &CancellationToken,
    ) {
        let held = match finished {
            Ok(held) => held,
            Err(error) => {
                tracing::error!(%error, "a chat job ended unexpectedly");
                return;
            }
        };
        for key in std::iter::once(held.lane).chain(held.second) {
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
    }

    /// Running jobs are cancelled through the shutdown token and waited out. Waiting jobs are
    /// dropped. Subscribers are gone by now, so nobody could hear a failure event for them. A
    /// waiting delivery's caller hears Unavailable.
    async fn drain(mut self) {
        self.submissions.close();
        self.controls.close();
        while let Some(command) = self.submissions.recv().await {
            match command {
                SubmitCommand::Run { reply, .. } => reply.unavailable(),
                SubmitCommand::Deliver(job) => job.reply.unavailable(),
                SubmitCommand::Delete(job) => job.reply.unavailable(),
                SubmitCommand::Rename(job) => job.reply.unavailable(),
            }
        }
        while let Some(command) = self.controls.recv().await {
            match command {
                Control::Cancel(_, reply) => reply.unavailable(),
                Control::Pending(_, reply) => reply.unavailable(),
            }
        }
        let waiting: usize = self.lanes.values().map(VecDeque::len).sum();
        if waiting > 0 {
            tracing::warn!(waiting, "dropping chat jobs that never started");
        }
        for (_, job) in self.lanes.drain().flat_map(|(_, lane)| lane) {
            match job {
                Job::Run(_) => {}
                Job::Deliver(job) => job.reply.unavailable(),
                Job::Delete(job) => job.reply.unavailable(),
                Job::Rename(job) => job.reply.unavailable(),
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

    use aria_assistant::chat::{ChatRequest, ChatStageKind};
    use aria_platform::delivery::OutputMode;
    use aria_settings::rpc::RpcErrorCode;

    use super::super::runtime::tests::{
        DELIVERY, DELIVERY_CANCELLED, ParkedAssistant, request, runtime_with,
    };
    use super::*;

    const PATIENCE: Duration = Duration::from_secs(5);

    fn job(text: &str, workdir: &Path, new_session: bool) -> AskJob {
        request(text, workdir, new_session).into()
    }

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

    /// Chats park in the assistant, then reply. Prompt runs fail at once.
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
    async fn asks_on_one_chat_take_turns_while_others_overlap() {
        let directory = tempfile::tempdir().expect("test directory");
        let elsewhere = tempfile::tempdir().expect("another directory");
        let harness = harness(directory.path());

        let first = harness
            .handle
            .ask(job("1", directory.path(), false))
            .await
            .expect("the first is taken")
            .0;
        let second = harness
            .handle
            .ask(job("2", directory.path(), false))
            .await
            .expect("the second is taken")
            .0;
        let third = harness
            .handle
            .ask(job("3", elsewhere.path(), false))
            .await
            .expect("the third is taken")
            .0;
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
            "the second starts once its chat is free"
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

    fn replied_within(harness: &Harness) -> Option<Ticket> {
        (0..3)
            .filter_map(|_| harness.events.recv_timeout(PATIENCE).ok())
            .find(|stage| matches!(stage.kind, ChatStageKind::Replied(..)))
            .map(|stage| stage.ticket)
    }

    fn ended_as(stage: &aria_assistant::chat::ChatStage, ticket: Ticket) -> Option<RpcErrorCode> {
        match &stage.kind {
            ChatStageKind::Failed { code, .. } if stage.ticket == ticket => Some(*code),
            _ => None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waiting_chat_is_cancelled_without_touching_the_running_one() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = harness(directory.path());

        harness
            .handle
            .ask(job("1", directory.path(), false))
            .await
            .expect("the first is taken");
        let second = harness
            .handle
            .ask(job("2", directory.path(), false))
            .await
            .expect("the second is taken")
            .0;
        assert_eq!(started_within(&harness.started, PATIENCE), Some(1));

        assert!(harness.handle.cancel(second).await.expect("answered"));
        assert!(!harness.handle.cancel(second).await.expect("answered"));

        // The running chat's opening stage may sit ahead of the refusal.
        let refused = (0..3)
            .filter_map(|_| harness.events.recv_timeout(PATIENCE).ok())
            .find_map(|stage| ended_as(&stage, second));
        assert_eq!(refused, Some(RpcErrorCode::Cancelled));

        harness.release.send(()).expect("the first is released");
        let replied = replied_within(&harness);
        assert_eq!(replied, Some(Ticket(1)));
        assert_eq!(
            started_within(&harness.started, Duration::from_millis(200)),
            None,
            "the cancelled chat never starts"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_ask_is_cancelled_and_frees_its_chat() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = harness(directory.path());

        let first = harness
            .handle
            .ask(job("1", directory.path(), false))
            .await
            .expect("the first is taken")
            .0;
        assert_eq!(started_within(&harness.started, PATIENCE), Some(1));

        assert!(harness.handle.cancel(first).await.expect("answered"));
        let ended = (0..3)
            .filter_map(|_| harness.events.recv_timeout(PATIENCE).ok())
            .find_map(|stage| ended_as(&stage, first));
        assert_eq!(ended, Some(RpcErrorCode::Cancelled));

        harness
            .handle
            .ask(job("2", directory.path(), false))
            .await
            .expect("the second is taken");
        assert_eq!(
            started_within(&harness.started, PATIENCE),
            Some(2),
            "the chat is free again"
        );
    }

    fn plans() -> ChatKey {
        ChatKey {
            assistant_id: aria_settings::config::AssistantId::ClaudeCode,
            name: aria_settings::ChatName::new("plans").expect("a valid name"),
        }
    }

    fn named_job(chat: &ChatKey, text: &str, workdir: &Path) -> AskJob {
        AskJob::from(ChatRequest {
            assistant_id: chat.assistant_id,
            name: Some(chat.name.clone()),
            ..request(text, workdir, false)
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_chat_still_replying_refuses_its_delete_until_it_replies() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = harness(directory.path());
        let chat = plans();
        harness
            .handle
            .ask(named_job(&chat, "1", directory.path()))
            .await
            .expect("the chat is taken");
        assert_eq!(started_within(&harness.started, PATIENCE), Some(1));

        let refused = harness
            .handle
            .delete_chat(chat.clone())
            .await
            .expect_err("the chat still replies");
        assert_eq!(RpcErrorCode::from(&refused), RpcErrorCode::Busy);

        harness.release.send(()).expect("the chat is released");
        assert!(replied_within(&harness).is_some());
        let deleted = tokio::time::timeout(PATIENCE, async {
            loop {
                match harness.handle.delete_chat(chat.clone()).await {
                    Err(CallError::Runtime(ChatError::Replying)) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    other => break other,
                }
            }
        })
        .await
        .expect("the lane frees once the reply is out");
        assert!(deleted.is_ok(), "{deleted:?}");

        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rename_waits_for_neither_name_and_frees_both_when_done() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = harness(directory.path());
        let named = |name: &str| ChatKey {
            assistant_id: aria_settings::config::AssistantId::ClaudeCode,
            name: ChatName::new(name).expect("a valid name"),
        };
        aria_assistant::chat::ChatFolders::open(
            directory
                .path()
                .join(super::super::runtime::CHAT_FOLDERS_FILE),
        )
        .start(SessionKey::named(&named("trip")), directory.path())
        .expect("the chat has a folder");
        harness
            .handle
            .ask(named_job(&named("trip"), "1", directory.path()))
            .await
            .expect("the ask is taken");
        assert_eq!(started_within(&harness.started, PATIENCE), Some(1));

        for (chat, name) in [("trip", "elsewhere"), ("plans", "trip")] {
            let refused = harness
                .handle
                .rename_chat(named(chat), ChatName::new(name).expect("a valid name"))
                .await
                .expect_err("an ask runs under one of the names");
            assert_eq!(
                RpcErrorCode::from(&refused),
                RpcErrorCode::Busy,
                "{chat} to {name}"
            );
        }
        harness.release.send(()).expect("the ask is released");
        assert!(replied_within(&harness).is_some());
        let renamed = tokio::time::timeout(PATIENCE, async {
            loop {
                match harness
                    .handle
                    .rename_chat(
                        named("trip"),
                        ChatName::new("weekend").expect("a valid name"),
                    )
                    .await
                {
                    Err(CallError::Runtime(ChatError::Replying)) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    other => break other,
                }
            }
        })
        .await
        .expect("the lane frees once the reply is out");
        assert_eq!(renamed.expect("the chat is renamed"), named("weekend"));

        harness
            .handle
            .ask(named_job(&named("weekend"), "2", directory.path()))
            .await
            .expect("the ask is taken");
        assert_eq!(
            started_within(&harness.started, PATIENCE),
            Some(2),
            "the new name's lane is free"
        );
        harness.release.send(()).expect("the ask is released");

        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_chats_pending_asks_list_the_running_one_then_those_waiting() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = harness(directory.path());
        let chat = plans();
        for text in ["1", "2", "3"] {
            harness
                .handle
                .ask(named_job(&chat, text, directory.path()))
                .await
                .expect("the ask is taken");
        }
        assert_eq!(started_within(&harness.started, PATIENCE), Some(1));

        let pending = harness.handle.pending(&chat).await.expect("answered");

        let pending: Vec<_> = pending
            .iter()
            .map(|ask| (ask.ticket, ask.text.as_str(), ask.running))
            .collect();
        assert_eq!(
            pending,
            [
                (Ticket(1), "1", true),
                (Ticket(2), "2", false),
                (Ticket(3), "3", false)
            ]
        );
        let other = ChatKey {
            name: aria_settings::ChatName::new("other").expect("a valid name"),
            ..chat
        };
        assert!(
            harness
                .handle
                .pending(&other)
                .await
                .expect("answered")
                .is_empty()
        );
        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_chat_ends_its_running_and_waiting_asks_and_no_other() {
        let directory = tempfile::tempdir().expect("test directory");
        let elsewhere = tempfile::tempdir().expect("another directory");
        let harness = harness(directory.path());
        let chat = plans();
        for text in ["1", "2"] {
            harness
                .handle
                .ask(named_job(&chat, text, directory.path()))
                .await
                .expect("the ask is taken");
        }
        let other = harness
            .handle
            .ask(job("3", elsewhere.path(), false))
            .await
            .expect("the other ask is taken")
            .0;
        let mut running = [
            started_within(&harness.started, PATIENCE),
            started_within(&harness.started, PATIENCE),
        ];
        running.sort();
        assert_eq!(running, [Some(1), Some(3)]);

        let cancelled = harness.handle.cancel_chat(&chat).await.expect("answered");

        assert_eq!(cancelled, 2);
        let mut ended: Vec<_> = (0..6)
            .filter_map(|_| harness.events.recv_timeout(PATIENCE).ok())
            .filter_map(|stage| match stage.kind {
                ChatStageKind::Failed { code, .. } => Some((stage.ticket, code)),
                _ => None,
            })
            .take(2)
            .collect();
        ended.sort_by_key(|(ticket, _)| *ticket);
        assert_eq!(
            ended,
            [
                (Ticket(1), RpcErrorCode::Cancelled),
                (Ticket(2), RpcErrorCode::Cancelled)
            ]
        );
        assert_eq!(
            harness.handle.cancel_chat(&chat).await.expect("answered"),
            0,
            "nothing is left to stop"
        );
        harness.release.send(()).expect("the other ask is released");
        let replied = replied_within(&harness);
        assert_eq!(replied, Some(other), "the other chat replies");
        assert_eq!(
            started_within(&harness.started, Duration::from_millis(200)),
            None,
            "the waiting ask never starts"
        );
        harness.shutdown.cancel();
        harness.actor.await.expect("the actor drains");
    }

    fn pressed_job(text: &str, workdir: &Path) -> AskJob {
        AskJob::from(ChatRequest {
            tap_cancels: true,
            ..request(text, workdir, false)
        })
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
            .ask(pressed_job("1", directory.path()))
            .await
            .expect("the older is taken")
            .0;
        let newer = harness
            .handle
            .ask(pressed_job("2", elsewhere.path()))
            .await
            .expect("the newer is taken")
            .0;
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
            .ask(pressed_job("1", directory.path()))
            .await
            .expect("the pressed chat is taken")
            .0;
        let typed = harness
            .handle
            .ask(job("2", elsewhere.path(), false))
            .await
            .expect("the typed chat is taken")
            .0;
        assert!(started_within(&harness.started, PATIENCE).is_some());
        assert!(started_within(&harness.started, PATIENCE).is_some());

        assert!(harness.handle.cancel_tapped().await.expect("answered"));
        assert_eq!(
            ended_within(&harness, pressed),
            Some(RpcErrorCode::Cancelled),
            "the older pressed chat ends, not the newer typed one"
        );

        let waiting = harness
            .handle
            .ask(pressed_job("3", elsewhere.path()))
            .await
            .expect("the waiting pressed chat is taken")
            .0;
        assert!(harness.handle.cancel_tapped().await.expect("answered"));
        assert_eq!(
            ended_within(&harness, waiting),
            Some(RpcErrorCode::Cancelled),
            "the pressed chat waiting behind the typed one ends"
        );
        assert!(
            !harness.handle.cancel_tapped().await.expect("answered"),
            "a typed chat is not a tap's to end"
        );

        harness
            .release
            .send(())
            .expect("the typed chat is released");
        let replied = replied_within(&harness);
        assert_eq!(replied, Some(typed));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drops_waiting_chats_and_waits_out_the_running_one() {
        let directory = tempfile::tempdir().expect("test directory");
        let harness = harness(directory.path());

        harness
            .handle
            .ask(job("1", directory.path(), false))
            .await
            .expect("the first is taken");
        harness
            .handle
            .ask(job("2", directory.path(), false))
            .await
            .expect("the second is taken");
        assert_eq!(started_within(&harness.started, PATIENCE), Some(1));

        harness.shutdown.cancel();
        harness.release.send(()).expect("the first is released");
        harness.actor.await.expect("the actor drains");

        assert_eq!(
            started_within(&harness.started, Duration::from_millis(200)),
            None,
            "the waiting chat never starts"
        );
        assert!(
            harness
                .handle
                .ask(job("3", directory.path(), false))
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
