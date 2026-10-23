use std::collections::{HashMap, VecDeque};

use aria_kit::cancel::{CancelToken, Deadline};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::memory::Lexicon;
use crate::vocabulary::{Directory, MatchError, PersonalRules, State, Status, Term, Vocabulary};
use crate::vocabulary::{
    catalog::{self, Catalog},
    collection::{self, Collector},
    discover, select,
};

use super::tasks::Supervised;

const QUEUE_CAPACITY: usize = 32;
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const WORKER_POLL: Duration = Duration::from_millis(20);
const LIVE_BYTES: usize = 128 * 1024 * 1024;
// Cached sources, discovery output and compilation can coexist with admitted snapshots.
const DISCOVERY_RESERVE: usize = 96 * 1024 * 1024;
const REFRESH_BUDGET: Duration = Duration::from_secs(2);

type Reply = oneshot::Sender<Result<Status, Error>>;

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("vocabulary worker is busy")]
    Busy,
    #[error("vocabulary worker is unavailable")]
    Unavailable,
    #[error("vocabulary exceeds its resource limit")]
    TooLarge,
    #[error("vocabulary refresh requires a persistent daemon")]
    Unsupported,
}

impl From<catalog::Error> for Error {
    fn from(error: catalog::Error) -> Self {
        if !matches!(error, catalog::Error::Cancelled) {
            tracing::warn!(%error, "vocabulary catalog operation failed");
        }
        match error {
            catalog::Error::TooLarge => Self::TooLarge,
            catalog::Error::Cancelled => Self::Busy,
            _ => Self::Unavailable,
        }
    }
}

impl<E: std::error::Error + 'static> From<collection::Error<E>> for Error {
    fn from(error: collection::Error<E>) -> Self {
        match error {
            collection::Error::TooLarge => Self::TooLarge,
            collection::Error::Cancelled => Self::Busy,
            collection::Error::Extractor(_) => {
                tracing::warn!(stage = "extraction", "vocabulary preparation failed");
                Self::Unavailable
            }
            _ => {
                tracing::warn!(stage = "discovery", "vocabulary preparation failed");
                Self::Unavailable
            }
        }
    }
}

impl From<select::Error> for Error {
    fn from(error: select::Error) -> Self {
        tracing::debug!(%error, "vocabulary selection stopped");
        match error {
            select::Error::Cancelled => Self::Busy,
            _ => Self::TooLarge,
        }
    }
}

impl From<MatchError> for Error {
    fn from(error: MatchError) -> Self {
        match error {
            MatchError::TooLarge => Self::TooLarge,
            MatchError::Cancelled => Self::Busy,
            MatchError::InvalidName => Self::Unavailable,
        }
    }
}

struct Entry {
    snapshot: Option<Arc<Vocabulary>>,
    root: Option<PathBuf>,
    personal_revision: u64,
    status: Status,
    used: Instant,
}

struct Registry {
    entries: HashMap<Directory, Entry>,
    pending: HashMap<Option<Directory>, Arc<AtomicBool>>,
    last_attempt: HashMap<Option<Directory>, Instant>,
    personal: Arc<Vocabulary>,
    personal_revision: u64,
    live: Vec<(Weak<Vocabulary>, usize)>,
    removals: HashMap<Directory, RemovalState>,
    retired: Vec<Arc<Vocabulary>>,
}

#[derive(PartialEq, Eq)]
enum RemovalState {
    Pending,
    Failed,
}

pub(super) struct Runtime {
    registry: Mutex<Registry>,
    requests: mpsc::SyncSender<Command>,
    active: Arc<AtomicUsize>,
    interrupt: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    persistent: bool,
}

pub(super) struct Snapshot {
    pub(super) matcher: Arc<Vocabulary>,
    active: Arc<AtomicUsize>,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

enum Command {
    Refresh {
        directory: Option<Directory>,
        cancel: Arc<AtomicBool>,
        reply: Option<Reply>,
    },
    Forget {
        directory: Directory,
        root: Option<PathBuf>,
        reply: Reply,
    },
}

pub(super) struct Worker<C: Collector> {
    collector: C,
    pipeline_key: Option<String>,
    runtime: Arc<Runtime>,
    requests: mpsc::Receiver<Command>,
    directory: PathBuf,
    lexicon: Option<Lexicon>,
    personal: PersonalRules,
}

impl Runtime {
    pub(super) fn new<C: Collector>(
        directory: PathBuf,
        lexicon: Option<Lexicon>,
        persistent: bool,
        collector: C,
    ) -> (Arc<Self>, Worker<C>) {
        let (requests, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let empty = Arc::new(Vocabulary::default());
        let runtime = Arc::new(Self {
            registry: Mutex::new(Registry {
                entries: HashMap::new(),
                pending: HashMap::new(),
                last_attempt: HashMap::new(),
                personal: Arc::clone(&empty),
                personal_revision: 0,
                live: Vec::new(),
                removals: HashMap::new(),
                retired: Vec::new(),
            }),
            requests,
            active: Arc::new(AtomicUsize::new(0)),
            interrupt: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(false)),
            persistent,
        });
        let worker = Worker {
            pipeline_key: collector.pipeline_key(),
            collector,
            runtime: Arc::clone(&runtime),
            requests: receiver,
            directory,
            lexicon,
            personal: PersonalRules::default(),
        };
        (runtime, worker)
    }

    pub(super) fn snapshot(&self, directory: Option<&Directory>) -> Snapshot {
        self.active.fetch_add(1, Ordering::AcqRel);
        self.interrupt.store(true, Ordering::Release);
        let matcher = {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let revision = registry.personal_revision;
            let entry = if registry.removals.is_empty() {
                directory.and_then(|directory| registry.entries.get_mut(directory))
            } else {
                None
            };
            let matcher = entry
                .and_then(|entry| {
                    entry.used = Instant::now();
                    (entry.personal_revision == revision)
                        .then(|| entry.snapshot.clone())
                        .flatten()
                })
                .unwrap_or_else(|| Arc::clone(&registry.personal));
            if let Err(error) = self.enqueue(&mut registry, directory.cloned(), None, false) {
                tracing::debug!(%error, "vocabulary recording refresh not queued");
            }
            matcher
        };
        Snapshot {
            matcher,
            active: Arc::clone(&self.active),
        }
    }

    pub(super) fn prepare(&self, directory: Option<Directory>) {
        let result = match self.registry.lock() {
            Ok(mut registry) => self.enqueue(&mut registry, directory, None, true),
            Err(_) => Err(Error::Unavailable),
        };
        if let Err(error) = result {
            tracing::debug!(%error, "vocabulary preparation not queued");
        }
    }

    pub(super) fn status(&self, directory: Option<&Directory>) -> Status {
        let Some(directory) = directory else {
            return Status {
                state: State::Disabled,
                ..Status::default()
            };
        };
        match self.registry.lock() {
            Ok(registry) if !registry.removals.is_empty() => Status {
                state: State::Unavailable,
                error: "removal".into(),
                ..Status::default()
            },
            Ok(registry) => registry
                .entries
                .get(directory)
                .map(|entry| entry.status.clone())
                .unwrap_or_else(|| {
                    if registry.pending.contains_key(&Some(directory.clone())) {
                        Status {
                            state: State::Refreshing,
                            ..Status::default()
                        }
                    } else {
                        Status::default()
                    }
                }),
            Err(_) => Status {
                error: "registry".to_owned(),
                ..Status::default()
            },
        }
    }

    pub(super) async fn refresh(&self, directory: Directory) -> Result<Status, Error> {
        if !self.persistent {
            return Err(Error::Unsupported);
        }
        if self.active.load(Ordering::Acquire) != 0 {
            return Err(Error::Busy);
        }
        let (reply, response) = oneshot::channel();
        {
            let mut registry = self.registry.lock().map_err(|_| Error::Unavailable)?;
            self.enqueue(&mut registry, Some(directory), Some(reply), true)?;
        }
        response.await.map_err(|_| Error::Unavailable)?
    }

    pub(super) async fn forget(&self, directory: Directory) -> Result<Status, Error> {
        let (reply, response) = oneshot::channel();
        {
            let mut registry = self.registry.lock().map_err(|_| Error::Unavailable)?;
            if registry.removals.get(&directory) == Some(&RemovalState::Pending)
                || (registry.removals.len() >= QUEUE_CAPACITY
                    && !registry.removals.contains_key(&directory))
            {
                return Err(Error::Busy);
            }
            let root = registry
                .entries
                .get(&directory)
                .and_then(|entry| entry.root.clone());
            self.requests
                .try_send(Command::Forget {
                    directory: directory.clone(),
                    root: root.clone(),
                    reply,
                })
                .map_err(send_error)?;
            registry
                .removals
                .insert(directory.clone(), RemovalState::Pending);
            for cancel in registry.pending.values() {
                cancel.store(true, Ordering::Release);
            }
            registry.pending.clear();
            registry.last_attempt.clear();
            for entry in registry.entries.values_mut() {
                if entry.status.state == State::Refreshing {
                    entry.status.state = if entry.snapshot.is_some() {
                        State::Stale
                    } else {
                        State::Unavailable
                    };
                }
            }
            let retired: Vec<_> = registry
                .entries
                .extract_if(|key, entry| {
                    overlaps(key.as_path(), directory.as_path())
                        || root.as_ref().is_some_and(|root| {
                            entry
                                .root
                                .as_ref()
                                .is_some_and(|known| overlaps(known, root))
                        })
                })
                .map(|(_, entry)| entry)
                .collect();
            registry
                .retired
                .extend(retired.into_iter().filter_map(|entry| entry.snapshot));
            self.interrupt.store(true, Ordering::Release);
        };
        response.await.map_err(|_| Error::Unavailable)?
    }

    fn enqueue(
        &self,
        registry: &mut Registry,
        directory: Option<Directory>,
        reply: Option<Reply>,
        force: bool,
    ) -> Result<(), Error> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(Error::Unavailable);
        }
        if directory.is_some() && !registry.removals.is_empty() {
            return Err(Error::Busy);
        }
        if registry.pending.contains_key(&directory) {
            return if reply.is_some() {
                Err(Error::Busy)
            } else {
                Ok(())
            };
        }
        if !force
            && registry
                .last_attempt
                .get(&directory)
                .is_some_and(|at| at.elapsed() < REFRESH_INTERVAL)
        {
            return Ok(());
        }
        if registry.pending.len() >= QUEUE_CAPACITY {
            return Err(Error::Busy);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.requests
            .try_send(Command::Refresh {
                directory: directory.clone(),
                cancel: Arc::clone(&cancel),
                reply,
            })
            .map_err(send_error)?;
        registry.pending.insert(directory.clone(), cancel);
        if registry.last_attempt.len() >= QUEUE_CAPACITY
            && !registry.last_attempt.contains_key(&directory)
            && let Some(oldest) = registry
                .last_attempt
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, _)| key.clone())
        {
            registry.last_attempt.remove(&oldest);
        }
        registry
            .last_attempt
            .insert(directory.clone(), Instant::now());
        if let Some(directory) = directory
            && (registry.entries.len() < QUEUE_CAPACITY
                || registry.entries.contains_key(&directory))
        {
            registry
                .entries
                .entry(directory)
                .or_insert_with(|| Entry {
                    snapshot: None,
                    root: None,
                    personal_revision: 0,
                    status: Status::default(),
                    used: Instant::now(),
                })
                .status
                .state = State::Refreshing;
        }
        Ok(())
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.interrupt.store(true, Ordering::Release);
        if let Ok(registry) = self.registry.lock() {
            for cancel in registry.pending.values() {
                cancel.store(true, Ordering::Release);
            }
        }
    }
}

fn send_error<T>(error: mpsc::TrySendError<T>) -> Error {
    match error {
        mpsc::TrySendError::Full(_) => Error::Busy,
        mpsc::TrySendError::Disconnected(_) => Error::Unavailable,
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl<C: Collector> Supervised for Worker<C> {
    async fn run(self, shutdown: CancellationToken) {
        let runtime = Arc::clone(&self.runtime);
        let mut job = tokio::task::spawn_blocking(move || self.work());
        let result = tokio::select! {
            result = &mut job => result,
            () = shutdown.cancelled() => { runtime.stop(); job.await },
        };
        runtime.stop();
        if result.is_err() {
            tracing::error!("vocabulary worker terminated unexpectedly");
        }
    }
}

impl<C: Collector> Worker<C> {
    pub(super) fn initialize_personal(&mut self) -> Result<(), Error> {
        self.refresh_personal()
    }

    fn work(mut self) {
        let mut catalog = None;
        let mut deferred: VecDeque<(Instant, Command)> = VecDeque::new();
        while !self.runtime.stopping.load(Ordering::Acquire) {
            let command = match self.requests.recv_timeout(WORKER_POLL) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            deferred.retain(|(_, command)| !matches!(command, Command::Refresh { cancel, .. } if cancel.load(Ordering::Acquire)));
            if let Some(command) = command {
                deferred.push_back((Instant::now(), command));
            }
            if self.runtime.active.load(Ordering::Acquire) != 0 {
                continue;
            }
            if let Ok(mut registry) = self.runtime.registry.lock() {
                let retired = std::mem::take(&mut registry.retired);
                drop(registry);
                drop(retired);
            }
            let Some(index) = deferred
                .iter()
                .position(|(ready, _)| *ready <= Instant::now())
            else {
                continue;
            };
            let (_, command) = deferred.remove(index).expect("selected command");
            match command {
                Command::Refresh {
                    directory,
                    cancel,
                    reply,
                } => {
                    if cancel.load(Ordering::Acquire) {
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(Error::Unavailable));
                        }
                        continue;
                    }
                    if self.await_idle(&cancel).is_err() {
                        if self.runtime.stopping.load(Ordering::Acquire) {
                            break;
                        }
                        let result = Err(Error::Unavailable);
                        self.finished(directory.as_ref(), &cancel, &result);
                        if let Some(reply) = reply {
                            let _ = reply.send(result);
                        }
                        continue;
                    }
                    let deadline = Deadline::after(REFRESH_BUDGET);
                    let token = self.control(Arc::clone(&cancel), deadline);
                    let mut result = self
                        .reserve(DISCOVERY_RESERVE, directory.as_ref())
                        .and_then(|()| self.refresh_personal())
                        .and_then(|()| match directory.as_ref() {
                            Some(directory) => {
                                self.refresh_project(directory, &cancel, &token, &mut catalog)
                            }
                            None => Ok(Status {
                                state: State::Ready,
                                ..Status::default()
                            }),
                        });
                    if deadline.expired() {
                        result = Err(Error::TooLarge);
                    }
                    if matches!(result, Err(Error::Busy))
                        && !cancel.load(Ordering::Acquire)
                        && !self.runtime.stopping.load(Ordering::Acquire)
                    {
                        deferred.push_back((
                            Instant::now() + REFRESH_INTERVAL,
                            Command::Refresh {
                                directory,
                                cancel,
                                reply,
                            },
                        ));
                        continue;
                    }
                    self.finished(directory.as_ref(), &cancel, &result);
                    if let Some(reply) = reply {
                        let _ = reply.send(result);
                    }
                }
                Command::Forget {
                    directory,
                    root,
                    reply,
                } => {
                    let cancel = Arc::new(AtomicBool::new(false));
                    if self.await_idle(&cancel).is_err() {
                        break;
                    }
                    let deadline = Deadline::after(REFRESH_BUDGET);
                    let token = self.control(cancel, deadline);
                    let mut result = self.remove(&directory, root.as_deref(), &token, &mut catalog);
                    if deadline.expired() {
                        result = Err(Error::TooLarge);
                    }
                    if matches!(result, Err(Error::Busy))
                        && !self.runtime.stopping.load(Ordering::Acquire)
                    {
                        deferred.push_back((
                            Instant::now() + WORKER_POLL,
                            Command::Forget {
                                directory,
                                root,
                                reply,
                            },
                        ));
                        continue;
                    }
                    if let Ok(mut registry) = self.runtime.registry.lock() {
                        if result.is_ok() {
                            registry.removals.remove(&directory);
                        } else {
                            registry.removals.insert(directory, RemovalState::Failed);
                        }
                    }
                    if result.is_err() {
                        tracing::warn!("vocabulary removal failed");
                    }
                    let _ = reply.send(result);
                }
            }
        }
    }

    fn control(&self, cancel: Arc<AtomicBool>, deadline: Deadline) -> CancelToken {
        let runtime = Arc::clone(&self.runtime);
        CancelToken::from_fn(move || {
            cancel.load(Ordering::Acquire)
                || runtime.stopping.load(Ordering::Acquire)
                || runtime.interrupt.load(Ordering::Acquire)
                || runtime.active.load(Ordering::Acquire) != 0
                || deadline.expired()
        })
    }

    fn remove(
        &self,
        directory: &Directory,
        root: Option<&Path>,
        token: &CancelToken,
        catalog: &mut Option<Catalog>,
    ) -> Result<Status, Error> {
        let resolved = if root.is_none() {
            match discover::resolve(directory.as_path()) {
                Ok(root) => Some(root),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return Err(Error::Unavailable),
            }
        } else {
            None
        };
        let roots = self.catalog(catalog, token)?.remove_overlapping(
            directory.as_path(),
            root.or(resolved.as_deref()),
            token,
        )?;
        let retired: Vec<_> = self
            .runtime
            .registry
            .lock()
            .map_err(|_| Error::Unavailable)?
            .entries
            .extract_if(|key, entry| {
                roots.iter().any(|root| {
                    overlaps(key.as_path(), root)
                        || entry
                            .root
                            .as_ref()
                            .is_some_and(|known| overlaps(known, root))
                })
            })
            .map(|(_, entry)| entry)
            .collect();
        drop(retired);
        Ok(Status::default())
    }

    fn await_idle(&self, cancel: &AtomicBool) -> Result<(), Error> {
        loop {
            if cancel.load(Ordering::Acquire) || self.runtime.stopping.load(Ordering::Acquire) {
                return Err(Error::Unavailable);
            }
            if self.runtime.active.load(Ordering::Acquire) == 0 {
                self.runtime.interrupt.store(false, Ordering::Release);
                if self.runtime.active.load(Ordering::Acquire) == 0 {
                    return Ok(());
                }
            }
            std::thread::sleep(WORKER_POLL);
        }
    }

    fn interrupted(&self, cancel: &AtomicBool) -> Result<(), Error> {
        if cancel.load(Ordering::Acquire) || self.runtime.stopping.load(Ordering::Acquire) {
            Err(Error::Unavailable)
        } else if self.runtime.interrupt.load(Ordering::Acquire)
            || self.runtime.active.load(Ordering::Acquire) != 0
        {
            Err(Error::Busy)
        } else {
            Ok(())
        }
    }

    fn reserve(&self, reservation: usize, protected: Option<&Directory>) -> Result<(), Error> {
        loop {
            let mut registry = self
                .runtime
                .registry
                .lock()
                .map_err(|_| Error::Unavailable)?;
            registry.live.retain(|(entry, _)| entry.strong_count() != 0);
            let bytes = self.personal.estimated_bytes()
                + registry.live.iter().map(|(_, bytes)| bytes).sum::<usize>();
            if bytes <= LIVE_BYTES - reservation {
                return Ok(());
            }
            let oldest = registry
                .entries
                .iter()
                .filter(|(key, entry)| {
                    Some(*key) != protected
                        && entry
                            .snapshot
                            .as_ref()
                            .is_some_and(|snapshot| Arc::strong_count(snapshot) == 1)
                })
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                return Err(Error::TooLarge);
            };
            let entry = registry.entries.get_mut(&oldest).expect("selected entry");
            let retired = entry.snapshot.take();
            entry.status.state = State::Unavailable;
            entry.status.runtime_revision = 0;
            entry.status.terms = 0;
            entry.status.error = "evicted".to_owned();
            registry.last_attempt.remove(&Some(oldest));
            drop(registry);
            drop(retired);
        }
    }

    fn catalog<'a>(
        &self,
        catalog: &'a mut Option<Catalog>,
        token: &CancelToken,
    ) -> Result<&'a mut Catalog, Error> {
        if catalog.is_none() {
            *catalog = Some(Catalog::open(&self.directory, token.clone())?);
        }
        let catalog = catalog.as_mut().ok_or(Error::Unavailable)?;
        catalog.cancel_on(token.clone())?;
        Ok(catalog)
    }

    fn refresh_personal(&mut self) -> Result<(), Error> {
        let interrupt = Arc::clone(&self.runtime.interrupt);
        let cancel = CancelToken::from_fn(move || interrupt.load(Ordering::Acquire));
        let rules = match &self.lexicon {
            Some(lexicon) => lexicon.snapshot_cancellable(&cancel).map_err(|_| {
                if self.runtime.interrupt.load(Ordering::Acquire) {
                    Error::Busy
                } else {
                    Error::Unavailable
                }
            })?,
            None => PersonalRules::default(),
        };
        if rules == self.personal {
            return Ok(());
        }
        let matcher = Arc::new(Vocabulary::compile_with(&[], rules.clone(), &cancel)?);
        let mut registry = self
            .runtime
            .registry
            .lock()
            .map_err(|_| Error::Unavailable)?;
        if self.runtime.interrupt.load(Ordering::Acquire) {
            return Err(Error::Busy);
        }
        registry
            .live
            .retain(|(snapshot, _)| snapshot.strong_count() != 0);
        registry
            .live
            .push((Arc::downgrade(&matcher), matcher.estimated_bytes()));
        let retired = std::mem::replace(&mut registry.personal, matcher);
        registry.personal_revision = registry.personal_revision.wrapping_add(1);
        for entry in registry.entries.values_mut() {
            if entry.snapshot.is_some() {
                entry.status.state = State::Stale;
            }
        }
        drop(registry);
        drop(retired);
        self.personal = rules;
        Ok(())
    }

    fn refresh_project(
        &mut self,
        directory: &Directory,
        cancel: &AtomicBool,
        token: &CancelToken,
        catalog: &mut Option<Catalog>,
    ) -> Result<Status, Error> {
        self.interrupted(cancel)?;
        let key = self.pipeline_key.clone().ok_or(Error::Unavailable)?;
        if self.collector.pipeline_key().as_ref() != Some(&key) {
            tracing::warn!("vocabulary collector changed its cache identity");
            return Err(Error::Unavailable);
        }
        let root = discover::resolve(directory.as_path()).map_err(|_| Error::Unavailable)?;
        self.interrupted(cancel)?;
        let cached = self.catalog(catalog, token)?.load(&root, &key)?;
        if let Some(cached) = &cached {
            let terms = select::terms(&cached.sources, token)?;
            let matcher = self.compile(&terms, token)?;
            self.publish(directory, &root, cached.revision, matcher, cancel)?;
            self.catalog(catalog, token)?
                .remember_alias(directory.as_path(), &root)?;
        }
        if !self.runtime.persistent {
            return Ok(self.runtime.status(Some(directory)));
        }
        self.reserve(DISCOVERY_RESERVE, Some(directory))?;
        let previous = cached.map(|cached| cached.sources).unwrap_or_default();
        let discovery = self.collector.collect(directory, &previous, token)?;
        if discovery.root != root {
            tracing::warn!("vocabulary selection changed during discovery");
            return Err(Error::Unavailable);
        }
        drop(previous);
        let terms = select::terms(&discovery.sources, token)?;
        let matcher = self.compile(&terms, token)?;
        self.interrupted(cancel)?;
        let revision = self
            .catalog(catalog, token)?
            .refresh_cancellable(&discovery, &key, token)?;
        tracing::debug!(
            entries = discovery.coverage.entries,
            hidden = discovery.coverage.hidden,
            symlinks = discovery.coverage.symlinks,
            unsupported = discovery.coverage.unsupported,
            oversized = discovery.coverage.oversized,
            bytes = discovery.coverage.bytes,
            "vocabulary collection completed"
        );
        if token.is_cancelled() {
            return Err(Error::Busy);
        }
        self.publish(directory, &root, revision, matcher, cancel)?;
        Ok(self.runtime.status(Some(directory)))
    }

    fn compile(&self, terms: &[Term], token: &CancelToken) -> Result<Arc<Vocabulary>, Error> {
        if token.is_cancelled() {
            return Err(Error::Busy);
        }
        let matcher = Arc::new(Vocabulary::compile_with(
            terms,
            self.personal.clone(),
            token,
        )?);
        if token.is_cancelled() {
            return Err(Error::Busy);
        }
        Ok(matcher)
    }

    fn publish(
        &self,
        directory: &Directory,
        root: &Path,
        revision: u64,
        matcher: Arc<Vocabulary>,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        self.interrupted(cancel)?;
        let mut registry = self
            .runtime
            .registry
            .lock()
            .map_err(|_| Error::Unavailable)?;
        self.interrupted(cancel)?;
        if !registry.removals.is_empty() {
            return Err(Error::Busy);
        }
        if !registry
            .pending
            .get(&Some(directory.clone()))
            .is_some_and(|pending| std::ptr::eq(pending.as_ref(), cancel))
        {
            return Err(Error::Unavailable);
        }
        let personal_revision = registry.personal_revision;
        registry.live.retain(|(entry, _)| entry.strong_count() != 0);
        registry
            .live
            .push((Arc::downgrade(&matcher), matcher.estimated_bytes()));
        let evicted = if registry.entries.len() >= QUEUE_CAPACITY
            && !registry.entries.contains_key(directory)
        {
            let oldest = registry
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            oldest.and_then(|oldest| registry.entries.remove(&oldest))
        } else {
            None
        };
        let retired = registry.entries.insert(
            directory.clone(),
            Entry {
                status: Status {
                    state: State::Ready,
                    catalog_revision: revision,
                    runtime_revision: revision,
                    terms: matcher.term_count(),
                    error: String::new(),
                },
                snapshot: Some(matcher),
                root: Some(root.to_path_buf()),
                personal_revision,
                used: Instant::now(),
            },
        );
        drop(registry);
        drop(retired);
        drop(evicted);
        Ok(())
    }

    fn finished(
        &self,
        directory: Option<&Directory>,
        cancel: &AtomicBool,
        result: &Result<Status, Error>,
    ) {
        let mut evicted = None;
        if let Ok(mut registry) = self.runtime.registry.lock() {
            let key = directory.cloned();
            if !registry
                .pending
                .get(&key)
                .is_some_and(|pending| std::ptr::eq(pending.as_ref(), cancel))
            {
                return;
            }
            registry.pending.remove(&key);
            if let Err(error) = result {
                if let Some(directory) = directory
                    && !registry.entries.contains_key(directory)
                {
                    if registry.entries.len() >= QUEUE_CAPACITY {
                        let oldest = registry
                            .entries
                            .iter()
                            .min_by_key(|(_, entry)| entry.used)
                            .map(|(key, _)| key.clone());
                        evicted = oldest.and_then(|oldest| registry.entries.remove(&oldest));
                    }
                    registry.entries.insert(
                        directory.clone(),
                        Entry {
                            snapshot: None,
                            root: None,
                            personal_revision: 0,
                            status: Status::default(),
                            used: Instant::now(),
                        },
                    );
                }
                if let Some(entry) =
                    directory.and_then(|directory| registry.entries.get_mut(directory))
                {
                    entry.status.state = if matches!(error, Error::TooLarge) {
                        State::TooLarge
                    } else if entry.snapshot.is_some() {
                        State::Stale
                    } else {
                        State::Unavailable
                    };
                    entry.status.error = if matches!(error, Error::TooLarge) {
                        "limit"
                    } else {
                        "refresh"
                    }
                    .to_owned();
                }
                tracing::warn!(%error, "vocabulary refresh failed; retaining available snapshot");
            }
        }
        drop(evicted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocabulary::discover::LocalCollector;
    use crate::vocabulary::extract::{self, Extractor, LocalExtractor, TextInput};
    use crate::vocabulary::{Candidate, Coverage, Discovery, Source, SourceKind, pipeline_key};
    use std::ops::Range;

    fn directory(path: &Path) -> Directory {
        Directory::new(path.to_path_buf()).unwrap()
    }

    fn project(base: &Path, name: &str) -> Directory {
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("notes.txt"), "URLSession vex16").unwrap();
        directory(&path)
    }

    struct StaticCollector {
        root: PathBuf,
        previous_counts: Vec<usize>,
    }

    impl Collector for StaticCollector {
        type Error = std::convert::Infallible;

        fn pipeline_key(&self) -> Option<String> {
            Some("test-collector-1".into())
        }

        fn collect(
            &mut self,
            selected: &Directory,
            previous: &[Source],
            _cancel: &CancelToken,
        ) -> Result<Discovery, collection::Error<Self::Error>> {
            self.previous_counts.push(previous.len());
            Ok(Discovery {
                selected: selected.as_path().to_path_buf(),
                root: self.root.clone(),
                sources: vec![Source {
                    relative_path: "terms.txt".into(),
                    kind: SourceKind::Text,
                    fingerprint: [1; 32],
                    input_bytes: 8,
                    candidates: vec![Candidate {
                        canonical: "aria-kit".into(),
                        span: 0..8,
                    }],
                }],
                coverage: Coverage {
                    entries: 1,
                    bytes: 8,
                    ..Coverage::default()
                },
            })
        }
    }

    #[test]
    fn injected_collection_reuses_evidence_and_publishes_corrections() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "project");
        let root = selected.as_path().canonicalize().unwrap();
        let collector = StaticCollector {
            root,
            previous_counts: Vec::new(),
        };
        let (runtime, mut worker) =
            Runtime::new(temporary.path().join("catalog"), None, true, collector);
        let mut catalog = None;
        for _ in 0..2 {
            runtime.prepare(Some(selected.clone()));
            let Command::Refresh { cancel, .. } = worker.requests.try_recv().unwrap() else {
                panic!("refresh expected")
            };
            let result =
                worker.refresh_project(&selected, &cancel, &CancelToken::never(), &mut catalog);
            worker.finished(Some(&selected), &cancel, &result);
            assert_eq!(result.unwrap().state, State::Ready);
        }
        assert_eq!(worker.collector.previous_counts, [0, 1]);

        let snapshot = runtime.snapshot(Some(&selected));
        assert_eq!(
            snapshot.matcher.correct("Use ARIA dash kit.").as_deref(),
            Some("Use aria-kit.")
        );
    }

    #[test]
    fn injected_collection_cannot_publish_evidence_from_another_root() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "project");
        let collector = StaticCollector {
            root: temporary.path().join("other-project"),
            previous_counts: Vec::new(),
        };
        let (runtime, mut worker) =
            Runtime::new(temporary.path().join("catalog"), None, true, collector);
        runtime.prepare(Some(selected.clone()));
        let Command::Refresh { cancel, .. } = worker.requests.try_recv().unwrap() else {
            panic!("refresh expected")
        };
        let mut catalog = None;
        let result =
            worker.refresh_project(&selected, &cancel, &CancelToken::never(), &mut catalog);
        worker.finished(Some(&selected), &cancel, &result);
        assert!(matches!(result, Err(Error::Unavailable)));
        assert_eq!(runtime.status(Some(&selected)).runtime_revision, 0);
        assert_eq!(
            runtime
                .snapshot(Some(&selected))
                .matcher
                .correct("aria dash kit"),
            None
        );
        assert!(
            catalog
                .unwrap()
                .load(
                    &selected.as_path().canonicalize().unwrap(),
                    &worker.pipeline_key.unwrap(),
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cold_admission_coalesces_and_bounds_work_without_filesystem_access() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, _worker) = Runtime::new(
            root.path().join("catalog"),
            None,
            true,
            LocalCollector::new(LocalExtractor),
        );
        for index in 0..100 {
            let selected = directory(&root.path().join(format!("missing-{index}")));
            let snapshot = runtime.snapshot(Some(&selected));
            assert_eq!(snapshot.matcher.term_count(), 0);
            assert_eq!(runtime.active.load(Ordering::Acquire), 1);
        }
        assert_eq!(runtime.active.load(Ordering::Acquire), 0);
        let registry = runtime.registry.lock().unwrap();
        assert_eq!(registry.pending.len(), QUEUE_CAPACITY);
        assert!(registry.entries.len() <= QUEUE_CAPACITY);
        assert!(registry.last_attempt.len() <= QUEUE_CAPACITY);
        assert!(!root.path().join("catalog").exists());
    }

    #[test]
    fn admitted_snapshots_survive_personal_deletion_and_refresh() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lexicon.txt"), "db2\n").unwrap();
        let (runtime, mut worker) = Runtime::new(
            root.path().join("catalog"),
            Some(Lexicon::new(root.path())),
            true,
            LocalCollector::new(LocalExtractor),
        );
        worker.initialize_personal().unwrap();
        let admitted = runtime.snapshot(None);
        assert_eq!(admitted.matcher.correct("d b two").as_deref(), Some("db2"));
        std::fs::remove_file(root.path().join("lexicon.txt")).unwrap();
        assert!(matches!(worker.refresh_personal(), Err(Error::Busy)));
        let pinned = Arc::clone(&admitted.matcher);
        drop(admitted);
        worker.await_idle(&AtomicBool::new(false)).unwrap();
        worker.refresh_personal().unwrap();
        assert_eq!(runtime.snapshot(None).matcher.correct("d b two"), None);
        assert_eq!(pinned.correct("d b two").as_deref(), Some("db2"));
    }

    #[tokio::test]
    async fn separate_scopes_and_failed_refresh_preserve_the_last_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let a = project(root.path(), "aria-stt");
        let b = project(root.path(), "other-stt");
        let (runtime, worker) = Runtime::new(
            root.path().join("catalog"),
            None,
            true,
            LocalCollector::new(LocalExtractor),
        );
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(worker.run(shutdown.clone()));
        assert_eq!(
            runtime.refresh(a.clone()).await.unwrap().state,
            State::Ready
        );
        runtime.refresh(b.clone()).await.unwrap();
        {
            let snapshot = runtime.snapshot(Some(&a));
            assert_eq!(
                snapshot.matcher.correct("aria dash stt").as_deref(),
                Some("aria-stt")
            );
            assert_eq!(snapshot.matcher.correct("other dash stt"), None);
            assert!(matches!(runtime.refresh(a.clone()).await, Err(Error::Busy)));
        }
        std::fs::rename(a.as_path(), root.path().join("moved")).unwrap();
        assert!(runtime.refresh(a.clone()).await.is_err());
        assert_eq!(runtime.status(Some(&a)).state, State::Stale);
        assert_eq!(
            runtime
                .snapshot(Some(&a))
                .matcher
                .correct("aria dash stt")
                .as_deref(),
            Some("aria-stt")
        );
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn forget_waits_for_active_recording_and_removes_overlapping_disk_scopes() {
        let root = tempfile::tempdir().unwrap();
        let parent = project(root.path(), "parent");
        let child = project(parent.as_path(), "aria-kit");
        let catalog_dir = root.path().join("catalog");
        let (runtime, worker) = Runtime::new(
            catalog_dir.clone(),
            None,
            true,
            LocalCollector::new(LocalExtractor),
        );
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(worker.run(shutdown.clone()));
        runtime.refresh(parent.clone()).await.unwrap();
        runtime.refresh(child.clone()).await.unwrap();
        let pinned = runtime.snapshot(Some(&parent));
        let forget_runtime = Arc::clone(&runtime);
        let forgotten = child.clone();
        let forget = tokio::spawn(async move { forget_runtime.forget(forgotten).await });
        while runtime.registry.lock().unwrap().removals.is_empty() {
            tokio::task::yield_now().await;
        }
        assert!(!forget.is_finished());
        assert_eq!(
            runtime
                .snapshot(Some(&parent))
                .matcher
                .correct("aria dash kit"),
            None
        );
        assert_eq!(
            pinned.matcher.correct("aria dash kit").as_deref(),
            Some("aria-kit")
        );
        drop(pinned);
        tokio::time::timeout(Duration::from_secs(3), forget)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        shutdown.cancel();
        task.await.unwrap();
        let catalog = Catalog::open(&catalog_dir, CancelToken::never()).unwrap();
        let key = pipeline_key(LocalExtractor.cache_key()).unwrap();
        assert!(
            catalog
                .load(&parent.as_path().canonicalize().unwrap(), &key)
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .load(&child.as_path().canonicalize().unwrap(), &key)
                .unwrap()
                .is_none()
        );
    }

    struct BlockingExtractor {
        entered: mpsc::Sender<()>,
        exited: mpsc::Sender<()>,
    }

    impl Extractor for BlockingExtractor {
        type Error = extract::Error;
        fn cache_key(&self) -> &str {
            "test-blocking-1"
        }
        fn extract(
            &mut self,
            _: TextInput<'_>,
            _: usize,
            cancel: &CancelToken,
        ) -> Result<Vec<Range<usize>>, Self::Error> {
            self.entered.send(()).unwrap();
            while !cancel.is_cancelled() {
                std::thread::yield_now();
            }
            self.exited.send(()).unwrap();
            Err(extract::Error::Cancelled)
        }
    }

    #[tokio::test]
    async fn admission_interrupts_injected_extraction_without_waiting_for_output() {
        let root = tempfile::tempdir().unwrap();
        let selected = project(root.path(), "aria-kit");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel();
        let (runtime, worker) = Runtime::new(
            root.path().join("catalog"),
            None,
            true,
            LocalCollector::new(BlockingExtractor {
                entered: entered_tx,
                exited: exited_tx,
            }),
        );
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(worker.run(shutdown.clone()));
        runtime.prepare(Some(selected.clone()));
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(3)))
            .await
            .unwrap()
            .unwrap();
        let snapshot = runtime.snapshot(Some(&selected));
        assert_eq!(snapshot.matcher.term_count(), 0);
        tokio::task::spawn_blocking(move || exited_rx.recv_timeout(Duration::from_secs(1)))
            .await
            .unwrap()
            .unwrap();
        drop(snapshot);
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn expired_deadline_cancels_backend_work() {
        let root = tempfile::tempdir().unwrap();
        let (_, worker) = Runtime::new(
            root.path().join("catalog"),
            None,
            true,
            LocalCollector::new(LocalExtractor),
        );
        let token = worker.control(
            Arc::new(AtomicBool::new(false)),
            Deadline::after(Duration::ZERO),
        );
        assert!(token.is_cancelled());
    }

    #[test]
    fn forgetting_an_unprepared_alias_removes_its_cached_ancestor() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "real-project");
        std::fs::create_dir(selected.as_path().join("child")).unwrap();
        let alias = temporary.path().join("alias");
        std::os::unix::fs::symlink(selected.as_path(), &alias).unwrap();
        let discovery = discover::scan_cached(
            selected.as_path(),
            &CancelToken::never(),
            &[],
            &mut LocalExtractor,
        )
        .unwrap();
        let key = pipeline_key(LocalExtractor.cache_key()).unwrap();
        let catalog_dir = temporary.path().join("catalog");
        let mut catalog = Catalog::open(&catalog_dir, CancelToken::never()).unwrap();
        catalog
            .refresh_cancellable(&discovery, &key, &CancelToken::never())
            .unwrap();
        let (_, worker) =
            Runtime::new(catalog_dir, None, true, LocalCollector::new(LocalExtractor));
        let mut catalog = Some(catalog);
        worker
            .remove(
                &directory(&alias.join("child")),
                None,
                &CancelToken::never(),
                &mut catalog,
            )
            .unwrap();
        assert!(
            catalog
                .unwrap()
                .load(&discovery.root, &key)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn obsolete_generation_cannot_publish_and_resource_failure_keeps_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let selected = directory(&root.path().join("project"));
        let (runtime, worker) = Runtime::new(
            root.path().join("catalog"),
            None,
            true,
            LocalCollector::new(LocalExtractor),
        );
        runtime.prepare(Some(selected.clone()));
        let Command::Refresh { cancel, .. } = worker.requests.try_recv().unwrap() else {
            panic!("refresh expected")
        };
        let matcher = Arc::new(
            Vocabulary::compile(
                &[Term {
                    canonical: "aria-stt".into(),
                    priority: 1,
                }],
                PersonalRules::default(),
            )
            .unwrap(),
        );
        worker
            .publish(
                &selected,
                selected.as_path(),
                1,
                Arc::clone(&matcher),
                &cancel,
            )
            .unwrap();
        assert!(matches!(
            worker.reserve(LIVE_BYTES, Some(&selected)),
            Err(Error::TooLarge)
        ));
        assert_eq!(
            runtime
                .snapshot(Some(&selected))
                .matcher
                .correct("aria dash stt")
                .as_deref(),
            Some("aria-stt")
        );
        runtime.registry.lock().unwrap().pending.clear();
        worker.await_idle(&AtomicBool::new(false)).unwrap();
        assert!(matches!(
            worker.publish(&selected, selected.as_path(), 2, matcher, &cancel),
            Err(Error::Unavailable)
        ));
    }
}
