//! The desktop session every Linux delivery shares. One thread owns the
//! clipboard and sends the paste keys, so one paste's write and keys cannot
//! interleave with another's, and a stuck display cannot hold up a caller.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arboard::{ClearExtLinux, Clipboard, GetExtLinux, LinuxClipboardKind, SetExtLinux};
use tokio_util::sync::CancellationToken;

use super::DisplayServer;
use super::portal::{self, PortalKeyboard};
use super::x11::X11Keyboard;

const THREAD: &str = "aria-input";
/// Bounds queued jobs when the display thread stalls.
const QUEUE: usize = 4;
const CANCEL_POLL: Duration = Duration::from_millis(50);
/// How long a caller that gave up waits for keys already being posted.
const POSTING_GRACE: Duration = Duration::from_secs(1);
/// How long a retired thread gets to finish. A stalled display never lets it,
/// so it is left behind rather than holding up the daemon.
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);
const JOIN_POLL: Duration = Duration::from_millis(10);
/// How long a written selection gets to read back. Keys sent before it does
/// would paste the previous clipboard.
const READ_BACK_TIMEOUT: Duration = Duration::from_millis(250);
const READ_BACK_POLL: Duration = Duration::from_millis(10);
const BOTH_SELECTIONS: [LinuxClipboardKind; 2] =
    [LinuxClipboardKind::Clipboard, LinuxClipboardKind::Primary];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    NoDisplay,
    /// The display server offers no way to send keys.
    Unsupported,
    /// The user declined keyboard control. Not asked again while this thread runs.
    NotPermitted,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyboardState {
    Ready,
    Connecting,
    Refused(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SendFailure {
    /// The chord cannot have reached the focused app.
    NothingSent,
    /// The chord may have reached the focused app.
    PartlySent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobError {
    /// Refused before anything was written.
    Refused(Refusal),
    ClipboardRejected,
    /// CLIPBOARD holds the text, but a terminal would paste stale PRIMARY.
    PrimaryRejected,
    /// The text was written and no paste can have happened.
    NotSent,
    TimedOut,
    Cancelled,
    /// Keys may have reached the focused app, and nothing confirms it.
    Unconfirmed,
    /// The thread stopped answering after it may have sent keys.
    Stalled,
}

impl JobError {
    /// Whether the thread can no longer be trusted with another job. A thread
    /// that answered stays, because it owns the text on the selections.
    fn is_stall(self) -> bool {
        match self {
            Self::TimedOut | Self::Stalled => true,
            Self::Refused(_)
            | Self::ClipboardRejected
            | Self::PrimaryRejected
            | Self::NotSent
            | Self::Cancelled
            | Self::Unconfirmed => false,
        }
    }
}

impl From<SendFailure> for JobError {
    fn from(failure: SendFailure) -> Self {
        match failure {
            SendFailure::NothingSent => Self::NotSent,
            SendFailure::PartlySent => Self::Unconfirmed,
        }
    }
}

/// What a failed paste does with the text already on the selections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterFailure {
    Keep,
    Withdraw,
}

/// How long one delivery may take, and whether it should stop.
pub(super) struct Budget<'control> {
    deadline: Instant,
    should_cancel: &'control dyn Fn() -> bool,
}

impl<'control> Budget<'control> {
    pub(super) fn new(timeout: Duration, should_cancel: &'control dyn Fn() -> bool) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            should_cancel,
        }
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn is_cancelled(&self) -> bool {
        (self.should_cancel)()
    }
}

/// Starts its thread on first use.
/// Missing display variables require a daemon restart before delivery can work.
pub(super) struct Desktop {
    server: DisplayServer,
    /// Where the portal's restore token is kept.
    home: PathBuf,
    worker: Mutex<Option<Arc<Worker>>>,
}

impl Desktop {
    pub(super) fn new(home: &Path, server: DisplayServer) -> Self {
        tracing::info!(?server, "display server");
        if server == DisplayServer::Absent {
            tracing::warn!(
                "no display in the service environment; restart the service from a desktop session"
            );
        }
        Self {
            server,
            home: home.to_path_buf(),
            worker: Mutex::new(None),
        }
    }

    /// Where the keyboard stands, asking for a connection when it has none.
    /// Never waits for the connection.
    pub(super) fn keyboard(&self) -> KeyboardState {
        match self.worker() {
            Ok(worker) => worker.keyboard(),
            Err(refusal) => KeyboardState::Refused(refusal),
        }
    }

    pub(super) fn await_keyboard(&self, budget: &Budget<'_>) -> Result<(), JobError> {
        let worker = self.worker().map_err(JobError::Refused)?;
        match worker.keyboard() {
            KeyboardState::Refused(refusal) => Err(JobError::Refused(refusal)),
            KeyboardState::Ready | KeyboardState::Connecting => worker.link.wait(budget),
        }
    }

    pub(super) fn copy(&self, text: &str, budget: &Budget<'_>) -> Result<(), JobError> {
        self.worker()
            .map_err(JobError::Refused)?
            .run_job(JobKind::Copy, text, budget)
    }

    /// Abandoning a job before its keys are sent prevents the paste.
    /// Clipboard cleanup is attempted but can fail.
    pub(super) fn paste(
        &self,
        text: &str,
        after_failure: AfterFailure,
        budget: &Budget<'_>,
    ) -> Result<(), JobError> {
        self.worker().map_err(JobError::Refused)?.run_job(
            JobKind::Paste { after_failure },
            text,
            budget,
        )
    }

    fn worker(&self) -> Result<Arc<Worker>, Refusal> {
        if self.server == DisplayServer::Absent {
            return Err(Refusal::NoDisplay);
        }
        let mut slot = lock(&self.worker);
        if let Some(worker) = slot.as_ref() {
            if worker.is_usable() {
                return Ok(Arc::clone(worker));
            }
            if !worker.has_stopped() {
                tracing::warn!("the input thread is stuck on the display; restart the service");
                return Err(Refusal::Failed);
            }
        }
        let worker = Arc::new(Worker::spawn(self.server, self.home.clone())?);
        let retired = slot.replace(Arc::clone(&worker));
        // Its bounded join must not hold up other callers.
        drop(slot);
        drop(retired);
        Ok(worker)
    }
}

enum LinkState {
    Idle,
    Connecting,
    Ready(Arc<PasteKeys>),
    Refused(Refusal),
}

/// The connection to whatever sends the paste keys.
struct KeyboardLink {
    state: Mutex<LinkState>,
    changed: Condvar,
}

impl KeyboardLink {
    fn new() -> Self {
        Self {
            state: Mutex::new(LinkState::Idle),
            changed: Condvar::new(),
        }
    }

    /// Runs `connect` unless a connection is up, under way, or declined.
    /// Availability failures are retried: the display or portal may have started.
    fn request(&self, connect: impl FnOnce() -> Result<(), Refusal>) -> KeyboardState {
        let mut state = lock(&self.state);
        let retry = match &*state {
            LinkState::Idle => true,
            LinkState::Refused(refusal) => *refusal != Refusal::NotPermitted,
            LinkState::Connecting | LinkState::Ready(_) => false,
        };
        if retry {
            if let Err(refusal) = connect() {
                return KeyboardState::Refused(refusal);
            }
            *state = LinkState::Connecting;
        }
        match &*state {
            LinkState::Ready(_) => KeyboardState::Ready,
            LinkState::Refused(refusal) => KeyboardState::Refused(*refusal),
            LinkState::Idle | LinkState::Connecting => KeyboardState::Connecting,
        }
    }

    fn settle(&self, server: DisplayServer, connected: Result<PasteKeys, Refusal>) {
        let settled = match connected {
            Ok(keys) => {
                tracing::info!(?server, "paste keystrokes ready");
                LinkState::Ready(Arc::new(keys))
            }
            Err(refusal) => {
                tracing::info!(?server, ?refusal, "paste keystrokes unavailable");
                LinkState::Refused(refusal)
            }
        };
        *lock(&self.state) = settled;
        self.changed.notify_all();
    }

    /// The connection died or failed a key. The next delivery asks for another.
    fn lose(&self) {
        let mut state = lock(&self.state);
        if matches!(&*state, LinkState::Ready(_)) {
            tracing::info!("paste keystrokes lost");
            *state = LinkState::Idle;
        }
    }

    fn keys(&self) -> Option<Arc<PasteKeys>> {
        match &*lock(&self.state) {
            LinkState::Ready(keys) => Some(Arc::clone(keys)),
            LinkState::Idle | LinkState::Connecting | LinkState::Refused(_) => None,
        }
    }

    fn wait(&self, budget: &Budget<'_>) -> Result<(), JobError> {
        let mut state = lock(&self.state);
        loop {
            match &*state {
                LinkState::Ready(_) => return Ok(()),
                LinkState::Refused(refusal) => return Err(JobError::Refused(*refusal)),
                LinkState::Idle | LinkState::Connecting => {}
            }
            if budget.is_cancelled() {
                return Err(JobError::Cancelled);
            }
            let remaining = budget.remaining();
            if remaining.is_zero() {
                return Err(JobError::TimedOut);
            }
            state = self
                .changed
                .wait_timeout(state, remaining.min(CANCEL_POLL))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

#[derive(Clone, Copy)]
enum JobKind {
    Copy,
    Paste { after_failure: AfterFailure },
}

enum Request {
    Connect,
    Job {
        kind: JobKind,
        text: String,
        claim: Arc<Claim>,
        reply: mpsc::Sender<Result<(), JobError>>,
    },
    Stop,
}

struct Worker {
    link: Arc<KeyboardLink>,
    // Dropped before the join, so the thread sees the queue close.
    requests: Option<SyncSender<Request>>,
    /// Stops queued jobs and the wait for portal consent. Once cancelled, this
    /// thread is not trusted with another job.
    shutdown: CancellationToken,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(server: DisplayServer, home: PathBuf) -> Result<Self, Refusal> {
        let link = Arc::new(KeyboardLink::new());
        let (requests, queue) = mpsc::sync_channel(QUEUE);
        let shutdown = CancellationToken::new();
        let thread = thread::Builder::new()
            .name(THREAD.to_owned())
            .spawn({
                let link = Arc::clone(&link);
                let shutdown = shutdown.clone();
                move || serve(server, &home, &shutdown, &link, &queue)
            })
            .map_err(|error| {
                tracing::warn!(%error, "failed to start the input thread");
                Refusal::Failed
            })?;
        Ok(Self {
            link,
            requests: Some(requests),
            shutdown,
            thread: Some(thread),
        })
    }

    fn is_usable(&self) -> bool {
        !self.shutdown.is_cancelled() && !self.has_stopped()
    }

    fn has_stopped(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }

    fn keyboard(&self) -> KeyboardState {
        self.link.request(|| self.send(Request::Connect))
    }

    fn run_job(&self, kind: JobKind, text: &str, budget: &Budget<'_>) -> Result<(), JobError> {
        if budget.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        let claim = Arc::new(Claim::default());
        let (reply, replies) = mpsc::channel();
        self.send(Request::Job {
            kind,
            text: text.to_owned(),
            claim: Arc::clone(&claim),
            reply,
        })
        .map_err(JobError::Refused)?;
        let result = await_job(&replies, &claim, budget);
        if result.is_err_and(JobError::is_stall) {
            self.retire();
        }
        result
    }

    /// A full queue means the thread is busy, and it sees the cancel next.
    fn retire(&self) {
        self.shutdown.cancel();
        if let Some(requests) = &self.requests {
            let _ = requests.try_send(Request::Stop);
        }
    }

    fn send(&self, request: Request) -> Result<(), Refusal> {
        let Some(requests) = &self.requests else {
            return Err(Refusal::Failed);
        };
        requests.try_send(request).map_err(|error| {
            if let TrySendError::Full(_) = error {
                tracing::warn!("the input thread is backed up");
                self.retire();
            }
            Refusal::Failed
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        drop(self.requests.take());
        self.shutdown.cancel();
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + JOIN_TIMEOUT;
        while !thread.is_finished() && Instant::now() < deadline {
            thread::sleep(JOIN_POLL);
        }
        if !thread.is_finished() {
            tracing::warn!("the input thread is stuck on the display; leaving it behind");
        } else if thread.join().is_err() {
            tracing::warn!("the input thread panicked");
        }
    }
}

/// Keys already being posted cannot be recalled, so they get a bounded wait
/// for their result after cancellation or timeout.
fn await_job(
    replies: &Receiver<Result<(), JobError>>,
    claim: &Claim,
    budget: &Budget<'_>,
) -> Result<(), JobError> {
    loop {
        let remaining = budget.remaining();
        match replies.recv_timeout(remaining.min(CANCEL_POLL)) {
            Ok(result) => return result,
            // A job the thread never claimed cannot have sent keys.
            Err(RecvTimeoutError::Disconnected) if claim.abandon() => {
                return Err(JobError::Refused(Refusal::Failed));
            }
            Err(RecvTimeoutError::Disconnected) => return Err(JobError::Stalled),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let stop = if budget.is_cancelled() {
            JobError::Cancelled
        } else if budget.remaining().is_zero() {
            JobError::TimedOut
        } else {
            continue;
        };
        if claim.abandon() {
            return Err(stop);
        }
        return replies
            .recv_timeout(POSTING_GRACE)
            .unwrap_or(Err(JobError::Stalled));
    }
}

fn serve(
    server: DisplayServer,
    home: &Path,
    shutdown: &CancellationToken,
    link: &Arc<KeyboardLink>,
    queue: &Receiver<Request>,
) {
    let mut selections = Selections::default();
    while let Ok(request) = queue.recv() {
        if shutdown.is_cancelled() {
            break;
        }
        match request {
            Request::Connect => connect(server, home, shutdown, link),
            Request::Job {
                kind,
                text,
                claim,
                reply,
            } => {
                let Some(result) = run_job(kind, &text, &claim, link, &mut selections) else {
                    continue;
                };
                let _ = reply.send(result);
            }
            Request::Stop => break,
        }
    }
}

/// The portal's consent dialog can stay open for minutes, so it waits off this
/// thread, leaving copies to run.
fn connect(
    server: DisplayServer,
    home: &Path,
    shutdown: &CancellationToken,
    link: &Arc<KeyboardLink>,
) {
    match server {
        DisplayServer::X11 => {
            link.settle(
                server,
                X11Keyboard::connect().map(|keyboard| PasteKeys::X11(Box::new(keyboard))),
            );
        }
        DisplayServer::Wayland => {
            // Weak: the link holds the keys, and only dropping the keys ends
            // this watcher.
            let watched = Arc::downgrade(link);
            let link = Arc::clone(link);
            portal::connect_in_background(
                home,
                shutdown,
                move || {
                    if let Some(link) = watched.upgrade() {
                        link.lose();
                    }
                },
                move |connected| link.settle(server, connected.map(PasteKeys::Portal)),
            );
        }
        DisplayServer::Absent => link.settle(server, Err(Refusal::NoDisplay)),
    }
}

/// An abandoned job returns no reply.
fn run_job(
    kind: JobKind,
    text: &str,
    claim: &Claim,
    link: &KeyboardLink,
    selections: &mut Selections,
) -> Option<Result<(), JobError>> {
    if claim.is_settled() {
        return None;
    }
    let JobKind::Paste { after_failure } = kind else {
        if selections
            .write(text, LinuxClipboardKind::Clipboard)
            .is_err()
        {
            return Some(Err(JobError::ClipboardRejected));
        }
        if !claim.post() {
            selections.clear(&[LinuxClipboardKind::Clipboard]);
            return None;
        }
        return Some(Ok(()));
    };
    let Some(keys) = link.keys() else {
        return Some(Err(JobError::Refused(Refusal::Failed)));
    };
    let withdraw_after =
        |selections: &mut Selections, written: &[LinuxClipboardKind], error: JobError| {
            if after_failure == AfterFailure::Withdraw {
                selections.clear(written);
            }
            error
        };
    if selections
        .write(text, LinuxClipboardKind::Clipboard)
        .is_err()
    {
        return Some(Err(JobError::ClipboardRejected));
    }
    if selections.write(text, LinuxClipboardKind::Primary).is_err() {
        let written = [LinuxClipboardKind::Clipboard];
        return Some(Err(withdraw_after(
            selections,
            &written,
            JobError::PrimaryRejected,
        )));
    }
    if let Err(error) = selections.read_back(text) {
        return Some(Err(withdraw_after(selections, &BOTH_SELECTIONS, error)));
    }
    if !claim.post() {
        selections.clear(&BOTH_SELECTIONS);
        return None;
    }
    Some(keys.send().map_err(|failure| {
        link.lose();
        withdraw_after(selections, &BOTH_SELECTIONS, failure.into())
    }))
}

/// The clipboard connection stays open once made: on X11, dropping the last
/// one gives up the selections.
#[derive(Default)]
struct Selections {
    clipboard: Option<Clipboard>,
}

impl Selections {
    // Excluding from history asks clipboard managers not to record the
    // transcript. It is a convention, not a guarantee.
    fn write(&mut self, text: &str, selection: LinuxClipboardKind) -> Result<(), arboard::Error> {
        self.connected()?
            .set()
            .clipboard(selection)
            .exclude_from_history()
            .text(text)
            .inspect_err(|error| {
                tracing::warn!(%error, ?selection, "the clipboard rejected the transcript");
            })
    }

    /// Waits for both selections to serve `text`. On X11 and XWayland, `arboard`
    /// answers from its own copy, so this proves only that it owns them.
    fn read_back(&mut self, text: &str) -> Result<(), JobError> {
        let deadline = Instant::now() + READ_BACK_TIMEOUT;
        loop {
            let on_clipboard = self.holds(text, LinuxClipboardKind::Clipboard);
            if on_clipboard && self.holds(text, LinuxClipboardKind::Primary) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    on_clipboard,
                    "the selections did not read back; the paste keys were not sent"
                );
                return Err(if on_clipboard {
                    JobError::NotSent
                } else {
                    JobError::ClipboardRejected
                });
            }
            thread::sleep(READ_BACK_POLL);
        }
    }

    fn holds(&mut self, text: &str, selection: LinuxClipboardKind) -> bool {
        self.connected()
            .and_then(|clipboard| clipboard.get().clipboard(selection).text())
            .is_ok_and(|held| held == text)
    }

    fn clear(&mut self, written: &[LinuxClipboardKind]) {
        for &selection in written {
            let cleared = self
                .connected()
                .and_then(|clipboard| clipboard.clear_with().clipboard(selection));
            if let Err(error) = cleared {
                tracing::warn!(%error, ?selection, "the transcript stayed on a selection");
            }
        }
    }

    fn connected(&mut self) -> Result<&mut Clipboard, arboard::Error> {
        match &mut self.clipboard {
            Some(clipboard) => Ok(clipboard),
            unconnected => {
                let clipboard = Clipboard::new().inspect_err(|error| {
                    tracing::warn!(%error, "failed to reach the clipboard");
                })?;
                Ok(unconnected.insert(clipboard))
            }
        }
    }
}

enum PasteKeys {
    X11(Box<X11Keyboard>),
    Portal(PortalKeyboard),
}

impl PasteKeys {
    fn send(&self) -> Result<(), SendFailure> {
        match self {
            Self::X11(keyboard) => keyboard.paste(),
            Self::Portal(keyboard) => keyboard.paste(),
        }
    }
}

/// Posting and abandonment compete for one claim. If abandonment wins,
/// the worker cannot send the paste keys later.
#[derive(Default)]
struct Claim(AtomicBool);

impl Claim {
    fn post(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    fn abandon(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    fn is_settled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every write here is a single assignment, so a panic leaves no torn
    // state behind.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(10);
    const LONG: Duration = Duration::from_secs(5);

    fn budget(timeout: Duration) -> Budget<'static> {
        Budget::new(timeout, &|| false)
    }

    #[test]
    fn a_link_under_way_is_not_asked_for_again() {
        let link = KeyboardLink::new();
        let mut asked = 0;
        let mut connect = || {
            asked += 1;
            Ok(())
        };
        assert_eq!(link.request(&mut connect), KeyboardState::Connecting);
        assert_eq!(link.request(&mut connect), KeyboardState::Connecting);
        assert_eq!(asked, 1, "a connection under way is not asked for again");
    }

    #[test]
    fn a_failed_request_leaves_the_link_able_to_retry() {
        let link = KeyboardLink::new();
        assert_eq!(
            link.request(|| Err(Refusal::Failed)),
            KeyboardState::Refused(Refusal::Failed)
        );
        assert_eq!(link.request(|| Ok(())), KeyboardState::Connecting);
    }

    #[test]
    fn a_declined_keyboard_is_not_asked_for_again() {
        let link = KeyboardLink::new();
        link.settle(DisplayServer::Wayland, Err(Refusal::NotPermitted));
        assert_eq!(
            link.request(|| panic!("the user already declined")),
            KeyboardState::Refused(Refusal::NotPermitted)
        );
    }

    #[test]
    fn an_unavailable_keyboard_is_asked_for_again() {
        let link = KeyboardLink::new();
        link.settle(DisplayServer::Wayland, Err(Refusal::Unsupported));
        assert_eq!(link.request(|| Ok(())), KeyboardState::Connecting);
    }

    #[test]
    fn losing_a_link_without_keys_changes_nothing() {
        let link = KeyboardLink::new();
        link.settle(DisplayServer::Wayland, Err(Refusal::NotPermitted));
        link.lose();
        assert_eq!(
            link.request(|| panic!("the user already declined")),
            KeyboardState::Refused(Refusal::NotPermitted)
        );
    }

    #[test]
    fn waiting_on_a_link_honours_its_budget() {
        let link = KeyboardLink::new();
        assert_eq!(link.wait(&budget(SHORT)), Err(JobError::TimedOut));
        assert_eq!(
            link.wait(&Budget::new(LONG, &|| true)),
            Err(JobError::Cancelled)
        );
        link.settle(DisplayServer::X11, Err(Refusal::Unsupported));
        assert_eq!(
            link.wait(&budget(SHORT)),
            Err(JobError::Refused(Refusal::Unsupported))
        );
    }

    #[test]
    fn only_a_chord_that_may_have_landed_is_unconfirmed() {
        assert_eq!(JobError::from(SendFailure::NothingSent), JobError::NotSent);
        assert_eq!(
            JobError::from(SendFailure::PartlySent),
            JobError::Unconfirmed
        );
    }

    #[test]
    fn a_retired_worker_is_not_replaced_until_it_stops() {
        let display = Desktop::new(Path::new("/nonexistent"), DisplayServer::X11);
        let (finish, finished) = mpsc::channel();
        let old = Arc::new(Worker {
            link: Arc::new(KeyboardLink::new()),
            requests: None,
            shutdown: CancellationToken::new(),
            thread: Some(thread::spawn(move || {
                let _ = finished.recv();
            })),
        });
        *lock(&display.worker) = Some(Arc::clone(&old));
        old.retire();
        assert!(old.shutdown.is_cancelled());
        assert!(matches!(display.worker(), Err(Refusal::Failed)));
        assert!(Arc::ptr_eq(lock(&display.worker).as_ref().unwrap(), &old));
        finish.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !old.thread.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline, "the released worker should stop");
            thread::yield_now();
        }
        let replacement = display.worker().expect("a stopped worker can be replaced");
        assert!(!Arc::ptr_eq(&old, &replacement));
    }

    #[test]
    fn retiring_an_idle_worker_stops_its_thread() {
        let worker = Worker::spawn(DisplayServer::X11, PathBuf::from("/nonexistent")).unwrap();
        worker.retire();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.thread.as_ref().unwrap().is_finished() {
            assert!(
                Instant::now() < deadline,
                "retirement should wake an idle worker"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn a_claim_settles_once() {
        let posted = Claim::default();
        assert!(posted.post());
        assert!(!posted.abandon(), "keys being posted cannot be abandoned");

        let abandoned = Claim::default();
        assert!(abandoned.abandon());
        assert!(!abandoned.post(), "an abandoned job never posts");
    }

    #[test]
    fn without_a_display_nothing_is_sent() {
        let display = Desktop::new(Path::new("/nonexistent"), DisplayServer::Absent);
        let refused = Err(JobError::Refused(Refusal::NoDisplay));
        assert_eq!(
            display.keyboard(),
            KeyboardState::Refused(Refusal::NoDisplay)
        );
        assert_eq!(display.await_keyboard(&budget(SHORT)), refused);
        assert_eq!(
            display.paste("text", AfterFailure::Withdraw, &budget(SHORT)),
            refused
        );
        assert_eq!(display.copy("text", &budget(SHORT)), refused);
    }

    #[test]
    fn a_job_nobody_answers_is_abandoned_at_its_deadline() {
        let (_reply, replies) = mpsc::channel();
        let claim = Claim::default();
        assert_eq!(
            await_job(&replies, &claim, &budget(SHORT)),
            Err(JobError::TimedOut)
        );
        assert!(!claim.post(), "the thread must not post it later");
    }

    #[test]
    fn a_cancel_abandons_a_job_before_its_keys() {
        let (_reply, replies) = mpsc::channel();
        let claim = Claim::default();
        assert_eq!(
            await_job(&replies, &claim, &Budget::new(LONG, &|| true)),
            Err(JobError::Cancelled)
        );
        assert!(!claim.post());
    }

    #[test]
    fn a_thread_gone_before_claiming_the_job_sent_nothing() {
        let (reply, replies) = mpsc::channel();
        drop(reply);
        assert_eq!(
            await_job(&replies, &Claim::default(), &budget(SHORT)),
            Err(JobError::Refused(Refusal::Failed))
        );
    }

    #[test]
    fn a_thread_that_answered_is_not_retired() {
        assert!(!JobError::Unconfirmed.is_stall());
        assert!(!JobError::NotSent.is_stall());
        assert!(JobError::TimedOut.is_stall());
        assert!(JobError::Stalled.is_stall());
    }

    #[test]
    fn a_thread_gone_after_claiming_the_job_counts_as_stalled() {
        let (reply, replies) = mpsc::channel();
        drop(reply);
        let claim = Claim::default();
        assert!(claim.post());
        assert_eq!(
            await_job(&replies, &claim, &budget(SHORT)),
            Err(JobError::Stalled)
        );
    }

    #[test]
    fn a_ready_result_wins_over_cancellation() {
        let (reply, replies) = mpsc::channel();
        let claim = Claim::default();
        assert!(claim.post());
        reply.send(Ok(())).expect("the receiver is alive");
        assert_eq!(
            await_job(&replies, &claim, &Budget::new(SHORT, &|| true)),
            Ok(())
        );
    }
}
