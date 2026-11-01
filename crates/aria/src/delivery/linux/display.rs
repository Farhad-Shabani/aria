//! Keeps clipboard ownership alive between deliveries.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arboard::{ClearExtLinux, Clipboard, LinuxClipboardKind, SetExtLinux};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    NoDisplay,
    /// The display server offers no way to send keys.
    Unsupported,
    /// The user declined keyboard control. Asked again after a restart.
    NotPermitted,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobError {
    /// Refused before anything was written.
    Refused(Refusal),
    ClipboardRejected,
    /// CLIPBOARD holds the text, but a terminal would paste stale PRIMARY.
    PrimaryRejected,
    TimedOut,
    Cancelled,
    /// Keys may have reached the focused app, and nothing confirms it.
    Unconfirmed,
}

/// Starts its thread on first use.
pub(super) struct DisplayThread {
    server: DisplayServer,
    /// Where the portal's restore token is kept.
    home: PathBuf,
    worker: Mutex<Option<Arc<Worker>>>,
}

impl DisplayThread {
    pub(super) fn new(server: DisplayServer, home: &Path) -> Self {
        Self {
            server,
            home: home.to_path_buf(),
            worker: Mutex::new(None),
        }
    }

    /// Connects without waiting; a declined permission is returned immediately.
    pub(super) fn start_connecting(&self) -> Result<(), Refusal> {
        self.worker()?.start_connecting()
    }

    pub(super) fn await_keyboard(
        &self,
        wait: Duration,
        should_cancel: &impl Fn() -> bool,
    ) -> Result<(), JobError> {
        self.worker()
            .map_err(JobError::Refused)?
            .await_keyboard(wait, should_cancel)
    }

    pub(super) fn copy(
        &self,
        text: &str,
        timeout: Duration,
        should_cancel: &impl Fn() -> bool,
    ) -> Result<(), JobError> {
        self.worker().map_err(JobError::Refused)?.run_job(
            JobKind::Copy,
            text,
            timeout,
            should_cancel,
        )
    }

    /// Abandoning a job before its keys are sent prevents the paste.
    /// Clipboard cleanup is attempted but can fail.
    pub(super) fn paste(
        &self,
        text: &str,
        timeout: Duration,
        keep_copy: bool,
        should_cancel: &impl Fn() -> bool,
    ) -> Result<(), JobError> {
        self.worker().map_err(JobError::Refused)?.run_job(
            JobKind::Paste { keep_copy },
            text,
            timeout,
            should_cancel,
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
            if worker
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
            {
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

enum Connection {
    Idle,
    Connecting,
    Ready(Arc<Keyboard>),
    Refused(Refusal),
}

struct Shared {
    connection: Mutex<Connection>,
    changed: Condvar,
}

impl Shared {
    fn set(&self, connection: Connection) {
        *lock(&self.connection) = connection;
        self.changed.notify_all();
    }

    fn settle(&self, server: DisplayServer, connected: Result<Keyboard, Refusal>) {
        match connected {
            Ok(keyboard) => {
                tracing::info!(?server, "paste keystrokes ready");
                self.set(Connection::Ready(Arc::new(keyboard)));
            }
            Err(refusal) => {
                tracing::info!(?server, ?refusal, "paste keystrokes unavailable");
                self.set(Connection::Refused(refusal));
            }
        }
    }

    fn keyboard(&self) -> Option<Arc<Keyboard>> {
        match &*lock(&self.connection) {
            Connection::Ready(keyboard) => Some(Arc::clone(keyboard)),
            Connection::Idle | Connection::Connecting | Connection::Refused(_) => None,
        }
    }
}

#[derive(Clone, Copy)]
enum JobKind {
    Copy,
    Paste { keep_copy: bool },
}

enum Request {
    Connect,
    Job {
        kind: JobKind,
        text: String,
        claim: Arc<Claim>,
        reply: mpsc::Sender<Result<(), JobError>>,
    },
}

struct Worker {
    shared: Arc<Shared>,
    // Dropped before the join, so the thread sees the queue close.
    requests: Option<SyncSender<Request>>,
    /// Stops queued jobs and interrupts portal consent.
    shutdown: CancellationToken,
    /// A job outlasted its caller, so this thread is not trusted with another.
    stalled: AtomicBool,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(server: DisplayServer, home: PathBuf) -> Result<Self, Refusal> {
        let shared = Arc::new(Shared {
            connection: Mutex::new(Connection::Idle),
            changed: Condvar::new(),
        });
        let (requests, queue) = mpsc::sync_channel(QUEUE);
        let shutdown = CancellationToken::new();
        let thread = thread::Builder::new()
            .name(THREAD.to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                let shutdown = shutdown.clone();
                move || serve(server, &home, &shutdown, &shared, &queue)
            })
            .map_err(|error| {
                tracing::warn!(%error, "failed to start the input thread");
                Refusal::Failed
            })?;
        Ok(Self {
            shared,
            requests: Some(requests),
            shutdown,
            stalled: AtomicBool::new(false),
            thread: Some(thread),
        })
    }

    fn is_usable(&self) -> bool {
        !self.stalled.load(Ordering::Acquire)
            && self
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
    }

    fn start_connecting(&self) -> Result<(), Refusal> {
        let mut connection = lock(&self.shared.connection);
        let retry = match &*connection {
            Connection::Idle => true,
            // Retry availability failures because the display or portal may have started.
            Connection::Refused(refusal) => *refusal != Refusal::NotPermitted,
            Connection::Connecting | Connection::Ready(_) => false,
        };
        if retry {
            self.send(Request::Connect)?;
            *connection = Connection::Connecting;
        }
        match &*connection {
            Connection::Refused(refusal) => Err(*refusal),
            Connection::Idle | Connection::Connecting | Connection::Ready(_) => Ok(()),
        }
    }

    fn await_keyboard(
        &self,
        wait: Duration,
        should_cancel: &impl Fn() -> bool,
    ) -> Result<(), JobError> {
        self.start_connecting().map_err(JobError::Refused)?;
        let deadline = Instant::now() + wait;
        let mut connection = lock(&self.shared.connection);
        loop {
            match &*connection {
                Connection::Ready(_) => return Ok(()),
                Connection::Refused(refusal) => return Err(JobError::Refused(*refusal)),
                Connection::Idle | Connection::Connecting => {}
            }
            if should_cancel() {
                return Err(JobError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(JobError::TimedOut);
            }
            connection = self
                .shared
                .changed
                .wait_timeout(connection, remaining.min(CANCEL_POLL))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    fn run_job(
        &self,
        kind: JobKind,
        text: &str,
        timeout: Duration,
        should_cancel: &impl Fn() -> bool,
    ) -> Result<(), JobError> {
        if should_cancel() {
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
        let result = await_job(&replies, &claim, timeout, should_cancel);
        if matches!(result, Err(JobError::TimedOut | JobError::Unconfirmed)) {
            self.retire();
        }
        result
    }

    fn retire(&self) {
        self.stalled.store(true, Ordering::Release);
        self.shutdown.cancel();
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
    timeout: Duration,
    should_cancel: &impl Fn() -> bool,
) -> Result<(), JobError> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match replies.recv_timeout(remaining.min(CANCEL_POLL)) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Disconnected) => return Err(JobError::Unconfirmed),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let stop = if should_cancel() {
            JobError::Cancelled
        } else if Instant::now() >= deadline {
            JobError::TimedOut
        } else {
            continue;
        };
        if claim.abandon() {
            return Err(stop);
        }
        return replies
            .recv_timeout(POSTING_GRACE)
            .unwrap_or(Err(JobError::Unconfirmed));
    }
}

fn serve(
    server: DisplayServer,
    home: &Path,
    shutdown: &CancellationToken,
    shared: &Arc<Shared>,
    queue: &Receiver<Request>,
) {
    let mut selections = Selections::default();
    while !shutdown.is_cancelled() {
        let request = match queue.recv_timeout(CANCEL_POLL) {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if shutdown.is_cancelled() {
            break;
        }
        match request {
            Request::Connect => connect(server, home, shutdown, shared),
            Request::Job {
                kind,
                text,
                claim,
                reply,
            } => {
                let Some(result) = run_job(kind, &text, &claim, shared, &mut selections) else {
                    continue;
                };
                let _ = reply.send(result);
            }
        }
    }
}

/// The portal's consent dialog can stay open for minutes, so it waits off this
/// thread, leaving copies to run.
fn connect(server: DisplayServer, home: &Path, shutdown: &CancellationToken, shared: &Arc<Shared>) {
    match server {
        DisplayServer::X11 => {
            shared.settle(
                server,
                X11Keyboard::connect().map(|keyboard| Keyboard::X11(Box::new(keyboard))),
            );
        }
        DisplayServer::Wayland => {
            let shared = Arc::clone(shared);
            portal::connect_in_background(home, shutdown, move |connected| {
                shared.settle(server, connected.map(Keyboard::Portal));
            });
        }
        DisplayServer::Absent => shared.settle(server, Err(Refusal::NoDisplay)),
    }
}

/// An abandoned job returns no reply.
fn run_job(
    kind: JobKind,
    text: &str,
    claim: &Claim,
    shared: &Shared,
    selections: &mut Selections,
) -> Option<Result<(), JobError>> {
    if claim.is_settled() {
        return None;
    }
    let keyboard = match kind {
        JobKind::Copy => None,
        JobKind::Paste { .. } => match shared.keyboard() {
            Some(keyboard) => Some(keyboard),
            None => return Some(Err(JobError::Refused(Refusal::Failed))),
        },
    };
    if let Err(error) = write_selections(kind, text, selections) {
        return Some(Err(error));
    }
    if !claim.post() {
        selections.clear();
        return None;
    }
    let Some(keyboard) = keyboard else {
        return Some(Ok(()));
    };
    let posted = keyboard.paste();
    if posted.is_err() {
        selections.clear();
        shared.set(Connection::Refused(Refusal::Failed));
    }
    Some(posted.map_err(|_| JobError::Unconfirmed))
}

fn write_selections(
    kind: JobKind,
    text: &str,
    selections: &mut impl SelectionStore,
) -> Result<(), JobError> {
    selections
        .write(text, LinuxClipboardKind::Clipboard)
        .map_err(|_| JobError::ClipboardRejected)?;
    if let JobKind::Paste { keep_copy } = kind
        && selections.write(text, LinuxClipboardKind::Primary).is_err()
    {
        if !keep_copy {
            selections.clear();
        }
        return Err(JobError::PrimaryRejected);
    }
    Ok(())
}

trait SelectionStore {
    fn write(&mut self, text: &str, selection: LinuxClipboardKind) -> Result<(), arboard::Error>;
    fn clear(&mut self);
}

/// The clipboard connection stays open once made. The owner serves every
/// paste, so the transcript lasts only as long as this thread.
#[derive(Default)]
struct Selections {
    clipboard: Option<Clipboard>,
}

impl SelectionStore for Selections {
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

    fn clear(&mut self) {
        for selection in [LinuxClipboardKind::Clipboard, LinuxClipboardKind::Primary] {
            let cleared = self
                .connected()
                .and_then(|clipboard| clipboard.clear_with().clipboard(selection));
            if let Err(error) = cleared {
                tracing::warn!(%error, ?selection, "the transcript stayed on a selection");
            }
        }
    }
}

impl Selections {
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

enum Keyboard {
    X11(Box<X11Keyboard>),
    Portal(PortalKeyboard),
}

impl Keyboard {
    fn paste(&self) -> Result<(), Refusal> {
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

    #[derive(Default)]
    struct ClipboardWithoutPrimary {
        text: Option<String>,
    }

    impl SelectionStore for ClipboardWithoutPrimary {
        fn write(
            &mut self,
            text: &str,
            selection: LinuxClipboardKind,
        ) -> Result<(), arboard::Error> {
            match selection {
                LinuxClipboardKind::Clipboard => {
                    self.text = Some(text.to_owned());
                    Ok(())
                }
                _ => Err(arboard::Error::ClipboardNotSupported),
            }
        }

        fn clear(&mut self) {
            self.text = None;
        }
    }

    #[test]
    fn failed_insert_cleans_up_before_the_next_copy() {
        let mut selections = ClipboardWithoutPrimary::default();
        assert_eq!(
            write_selections(
                JobKind::Paste { keep_copy: false },
                "first",
                &mut selections
            ),
            Err(JobError::PrimaryRejected)
        );
        assert_eq!(selections.text, None, "cleanup finishes before replying");
        assert_eq!(
            write_selections(JobKind::Copy, "second", &mut selections),
            Ok(())
        );
        assert_eq!(selections.text.as_deref(), Some("second"));
    }

    #[test]
    fn insert_or_copy_preserves_the_copy_when_primary_fails() {
        let mut selections = ClipboardWithoutPrimary::default();
        assert_eq!(
            write_selections(JobKind::Paste { keep_copy: true }, "text", &mut selections),
            Err(JobError::PrimaryRejected)
        );
        assert_eq!(selections.text.as_deref(), Some("text"));
    }

    #[test]
    fn a_retired_worker_is_not_replaced_until_it_stops() {
        let display = DisplayThread::new(DisplayServer::X11, Path::new("/nonexistent"));
        let (finish, finished) = mpsc::channel();
        let old = Arc::new(Worker {
            shared: Arc::new(Shared {
                connection: Mutex::new(Connection::Idle),
                changed: Condvar::new(),
            }),
            requests: None,
            shutdown: CancellationToken::new(),
            stalled: AtomicBool::new(false),
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
        let display = DisplayThread::new(DisplayServer::Absent, Path::new("/nonexistent"));
        let refused = Err(JobError::Refused(Refusal::NoDisplay));
        assert_eq!(display.start_connecting(), Err(Refusal::NoDisplay));
        assert_eq!(display.await_keyboard(SHORT, &|| false), refused);
        assert_eq!(display.paste("text", SHORT, false, &|| false), refused);
        assert_eq!(display.copy("text", SHORT, &|| false), refused);
    }

    #[test]
    fn a_job_nobody_answers_is_abandoned_at_its_deadline() {
        let (_reply, replies) = mpsc::channel();
        let claim = Claim::default();
        assert_eq!(
            await_job(&replies, &claim, SHORT, &|| false),
            Err(JobError::TimedOut)
        );
        assert!(!claim.post(), "the thread must not post it later");
    }

    #[test]
    fn a_cancel_abandons_a_job_before_its_keys() {
        let (_reply, replies) = mpsc::channel();
        let claim = Claim::default();
        assert_eq!(
            await_job(&replies, &claim, Duration::from_secs(5), &|| true),
            Err(JobError::Cancelled)
        );
        assert!(!claim.post());
    }

    #[test]
    fn a_vanished_thread_leaves_the_paste_unconfirmed() {
        let (reply, replies) = mpsc::channel();
        drop(reply);
        assert_eq!(
            await_job(&replies, &Claim::default(), SHORT, &|| false),
            Err(JobError::Unconfirmed)
        );
    }

    #[test]
    fn a_ready_result_wins_over_cancellation() {
        let (reply, replies) = mpsc::channel();
        let claim = Claim::default();
        assert!(claim.post());
        reply.send(Ok(())).expect("the receiver is alive");
        assert_eq!(await_job(&replies, &claim, SHORT, &|| true), Ok(()));
    }
}
