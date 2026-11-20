use super::tasks::Supervised;
use aria_assistant::memory::{PairsError, learned_pairs};
use aria_kit::cancel::{CancelToken, Deadline};
use aria_primitives::Cause;
use aria_primitives::error_chain;
use aria_settings::rpc::RpcErrorCode;
use aria_settings::{Directory, SettingsStore};
use aria_voice::vocabulary::{
    Activity, Completeness, Failure, List, MatchError, PersonalRules, Status, Term, Vocabulary,
    catalog::{self, Catalog},
    collection::{self, Collector},
    dictionary::{AddWordOutcome, Dictionary, DictionaryError, RemoveWordOutcome, Stamp},
    discover, select,
};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const QUEUE_CAPACITY: usize = 32;
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const WORKER_POLL: Duration = Duration::from_millis(20);
/// How often an idle worker checks the dictionary for a save, as the
/// settings file is checked.
const DICTIONARY_POLL: Duration = Duration::from_secs(1);
const LIVE_BYTES: usize = 128 * 1024 * 1024;
// Cached sources, discovery output and compilation can coexist with admitted snapshots.
const DISCOVERY_RESERVE: usize = 96 * 1024 * 1024;
const REFRESH_BUDGET: Duration = Duration::from_secs(2);
// The scan stops here, leaving the rest of the refresh budget for selection,
// compilation, and the catalog.
const SCAN_BUDGET: Duration = Duration::from_secs(1);

type Reply = oneshot::Sender<Result<Status, Error>>;

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("vocabulary worker is busy")]
    Busy,
    #[error("vocabulary worker is unavailable")]
    Unavailable(#[source] Option<Cause>),
    #[error("vocabulary exceeds its resource limit")]
    TooLarge,
    #[error("vocabulary refresh requires a persistent daemon")]
    Unsupported,
    #[error("select a vocabulary directory first")]
    NoDirectory,
    #[error(transparent)]
    InvalidWord(DictionaryError),
}

impl Error {
    fn unavailable(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Unavailable(Some(Box::new(error)))
    }
}

impl From<&Error> for RpcErrorCode {
    fn from(error: &Error) -> Self {
        match error {
            Error::Busy => Self::Busy,
            Error::Unavailable(_) => Self::Internal,
            Error::Unsupported => Self::Unsupported,
            Error::TooLarge => Self::PayloadTooLarge,
            Error::NoDirectory | Error::InvalidWord(_) => Self::InvalidRequest,
        }
    }
}

impl From<DictionaryError> for Error {
    fn from(error: DictionaryError) -> Self {
        match error {
            DictionaryError::InvalidWord => Self::InvalidWord(error),
            DictionaryError::TooLarge => Self::TooLarge,
            DictionaryError::Changed => Self::Busy,
            DictionaryError::Io(_) | DictionaryError::InvalidFile => Self::unavailable(error),
        }
    }
}

impl From<catalog::Error> for Error {
    fn from(error: catalog::Error) -> Self {
        match error {
            catalog::Error::TooLarge => Self::TooLarge,
            catalog::Error::Cancelled => Self::Busy,
            error => Self::unavailable(error),
        }
    }
}

impl<E: std::error::Error + Send + Sync + 'static> From<collection::Error<E>> for Error {
    fn from(error: collection::Error<E>) -> Self {
        match error {
            collection::Error::Cancelled => Self::Busy,
            error => Self::unavailable(error),
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
            MatchError::InvalidName => Self::Unavailable(None),
        }
    }
}

/// A compiled list and what status reports about it.
struct Prepared {
    matcher: Arc<Vocabulary>,
    catalog_revision: Option<u64>,
    found: usize,
    completeness: Option<Completeness>,
}

/// The list dictation uses for a folder.
struct Live {
    matcher: Arc<Vocabulary>,
    /// The personal rules revision it was compiled with.
    personal_revision: u64,
    catalog_revision: Option<u64>,
    found: usize,
    completeness: Option<Completeness>,
}

impl Live {
    fn list(&self, personal_revision: u64) -> List {
        List {
            catalog_revision: self.catalog_revision,
            terms: self.matcher.term_count(),
            found: self.found,
            completeness: self.completeness,
            is_stale: self.personal_revision != personal_revision,
        }
    }
}

struct Entry {
    live: Option<Live>,
    root: Option<PathBuf>,
    activity: Activity,
    used: Instant,
}

impl Entry {
    fn new(activity: Activity) -> Self {
        Self {
            live: None,
            root: None,
            activity,
            used: Instant::now(),
        }
    }
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
    /// Names the folder a request leaves unnamed, and whether vocabulary is on.
    settings: Arc<SettingsStore>,
    dictionary: Arc<Dictionary>,
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
    /// The settings' project folder, `None` while project vocabulary is off.
    project: Option<tokio::sync::watch::Receiver<Option<Directory>>>,
    collector: C,
    cache_key: Option<String>,
    runtime: Arc<Runtime>,
    requests: mpsc::Receiver<Command>,
    directory: PathBuf,
    sources: PersonalSources,
    personal: PersonalRules,
    /// The dictionary file `personal` was read from.
    stamp: Option<Stamp>,
    next_poll: Instant,
}

/// Where personal rules come from: your dictionary, and the pairs memory
/// learned in its directory.
pub(super) struct PersonalSources {
    dictionary: Arc<Dictionary>,
    memory: PathBuf,
}

impl PersonalSources {
    pub(super) fn new(dictionary: Arc<Dictionary>, memory: PathBuf) -> Self {
        Self { dictionary, memory }
    }

    /// No dictionary file, so the personal rules stay empty.
    #[cfg(test)]
    pub(super) fn unlisted(directory: &Path) -> Self {
        Self::new(
            Arc::new(Dictionary::new(directory.join("dictionary.txt"))),
            directory.to_owned(),
        )
    }

    /// No dictionary file means no rules, whatever memory learned.
    fn rules(&self, cancel: &CancelToken) -> Result<PersonalRules, PersonalError> {
        let Some(words) = self.dictionary.words()? else {
            return Ok(PersonalRules::default());
        };
        let learned = learned_pairs(&self.memory)?;
        Ok(PersonalRules::from_stored(words, learned, cancel)?)
    }
}

#[derive(Debug, thiserror::Error)]
enum PersonalError {
    #[error(transparent)]
    Dictionary(#[from] DictionaryError),
    #[error(transparent)]
    Pairs(#[from] PairsError),
    #[error(transparent)]
    Rules(#[from] MatchError),
}

impl Runtime {
    pub(super) fn new<C: Collector>(
        directory: PathBuf,
        sources: PersonalSources,
        persistent: bool,
        settings: Arc<SettingsStore>,
        collector: C,
    ) -> (Arc<Self>, Worker<C>) {
        let (requests, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let dictionary = Arc::clone(&sources.dictionary);
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
            settings,
            dictionary,
        });
        let worker = Worker {
            project: None,
            cache_key: collector.cache_key(),
            collector,
            runtime: Arc::clone(&runtime),
            requests: receiver,
            directory,
            sources,
            personal: PersonalRules::default(),
            stamp: None,
            next_poll: Instant::now(),
        };
        (runtime, worker)
    }

    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn snapshot(&self, directory: Option<&Directory>) -> Snapshot {
        self.active.fetch_add(1, Ordering::AcqRel);
        self.interrupt.store(true, Ordering::Release);
        let matcher = {
            let mut registry = self.registry();
            let revision = registry.personal_revision;
            let entry = if registry.removals.is_empty() {
                directory.and_then(|directory| registry.entries.get_mut(directory))
            } else {
                None
            };
            let matcher = entry
                .and_then(|entry| {
                    entry.used = Instant::now();
                    entry
                        .live
                        .as_ref()
                        .filter(|live| live.personal_revision == revision)
                        .map(|live| Arc::clone(&live.matcher))
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

    /// Adds `word` to your dictionary, then queues the personal rules'
    /// recompile.
    pub(super) fn add_word(&self, word: &str) -> Result<AddWordOutcome, Error> {
        let added = self.dictionary.add(word)?;
        if added == AddWordOutcome::Added {
            self.prepare(None);
        }
        Ok(added)
    }

    /// Removes `word` from your dictionary, then queues the personal rules'
    /// recompile.
    pub(super) fn remove_word(&self, word: &str) -> Result<RemoveWordOutcome, Error> {
        let removed = self.dictionary.remove(word)?;
        if removed == RemoveWordOutcome::Removed {
            self.prepare(None);
        }
        Ok(removed)
    }

    pub(super) fn list_words(&self) -> Result<Vec<String>, Error> {
        Ok(self.dictionary.words()?.unwrap_or_default())
    }

    pub(super) fn prepare(&self, directory: Option<Directory>) {
        if let Err(error) = self.enqueue(&mut self.registry(), directory, None, true) {
            tracing::debug!(%error, "vocabulary preparation not queued");
        }
    }

    /// What a client sees for the folder it names, else the configured one:
    /// disabled while the setting is off, whatever the catalog holds.
    pub(super) fn status(&self, requested: Option<Directory>) -> Result<Status, Error> {
        let settings = self
            .settings
            .snapshot()
            .map_err(Error::unavailable)?
            .settings;
        let directory = requested.or(settings.vocabulary_dir);
        if !settings.vocabulary_enabled {
            return Ok(Status::Disabled);
        }
        Ok(self.catalog_status(directory.as_ref()))
    }

    /// The folder a change applies to: the one named, else the configured one.
    fn selected(&self, requested: Option<Directory>) -> Result<Directory, Error> {
        match requested {
            Some(directory) => Ok(directory),
            None => self
                .settings
                .snapshot()
                .map_err(Error::unavailable)?
                .settings
                .vocabulary_dir
                .ok_or(Error::NoDirectory),
        }
    }

    fn catalog_status(&self, directory: Option<&Directory>) -> Status {
        let Some(directory) = directory else {
            return Status::Disabled;
        };
        let registry = self.registry();
        if !registry.removals.is_empty() {
            let activity = if registry
                .removals
                .values()
                .any(|state| *state == RemovalState::Pending)
            {
                Activity::Removing
            } else {
                Activity::Failed(Failure::Removal)
            };
            return Status::Folder {
                list: None,
                activity,
            };
        }
        match registry.entries.get(directory) {
            Some(entry) => Status::Folder {
                list: entry
                    .live
                    .as_ref()
                    .map(|live| live.list(registry.personal_revision)),
                activity: entry.activity,
            },
            None => Status::Folder {
                list: None,
                activity: if registry.pending.contains_key(&Some(directory.clone())) {
                    Activity::Refreshing
                } else {
                    Activity::Idle
                },
            },
        }
    }

    pub(super) async fn refresh(&self, requested: Option<Directory>) -> Result<Status, Error> {
        let directory = self.selected(requested)?;
        if !self.persistent {
            return Err(Error::Unsupported);
        }
        if self.active.load(Ordering::Acquire) != 0 {
            return Err(Error::Busy);
        }
        let (reply, response) = oneshot::channel();
        {
            let mut registry = self.registry();
            self.enqueue(&mut registry, Some(directory), Some(reply), true)?;
        }
        response.await.map_err(|_| Error::Unavailable(None))?
    }

    pub(super) async fn forget(&self, requested: Option<Directory>) -> Result<Status, Error> {
        let directory = self.selected(requested)?;
        let (reply, response) = oneshot::channel();
        {
            let mut registry = self.registry();
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
                if entry.activity == Activity::Refreshing {
                    entry.activity = Activity::Idle;
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
            registry.retired.extend(
                retired
                    .into_iter()
                    .filter_map(|entry| entry.live.map(|live| live.matcher)),
            );
            self.interrupt.store(true, Ordering::Release);
        };
        response.await.map_err(|_| Error::Unavailable(None))?
    }

    fn publish(
        &self,
        directory: &Directory,
        root: &Path,
        prepared: Prepared,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let Prepared {
            matcher,
            catalog_revision,
            found,
            completeness,
        } = prepared;
        self.interrupted(cancel)?;
        let mut registry = self.registry();
        self.interrupted(cancel)?;
        if !registry.removals.is_empty() {
            return Err(Error::Busy);
        }
        if !registry
            .pending
            .get(&Some(directory.clone()))
            .is_some_and(|pending| std::ptr::eq(pending.as_ref(), cancel))
        {
            return Err(Error::Unavailable(None));
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
                live: Some(Live {
                    matcher,
                    personal_revision,
                    catalog_revision,
                    found,
                    completeness,
                }),
                root: Some(root.to_path_buf()),
                activity: Activity::Idle,
                used: Instant::now(),
            },
        );
        drop(registry);
        drop(retired);
        drop(evicted);
        Ok(())
    }

    fn interrupted(&self, cancel: &AtomicBool) -> Result<(), Error> {
        if cancel.load(Ordering::Acquire) || self.stopping.load(Ordering::Acquire) {
            Err(Error::Unavailable(None))
        } else if self.interrupt.load(Ordering::Acquire) || self.active.load(Ordering::Acquire) != 0
        {
            Err(Error::Busy)
        } else {
            Ok(())
        }
    }

    fn enqueue(
        &self,
        registry: &mut Registry,
        directory: Option<Directory>,
        reply: Option<Reply>,
        force: bool,
    ) -> Result<(), Error> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(Error::Unavailable(None));
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
                .or_insert_with(|| Entry::new(Activity::Refreshing))
                .activity = Activity::Refreshing;
        }
        Ok(())
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.interrupt.store(true, Ordering::Release);
        for cancel in self.registry().pending.values() {
            cancel.store(true, Ordering::Release);
        }
    }
}

fn send_error<T>(error: mpsc::TrySendError<T>) -> Error {
    match error {
        mpsc::TrySendError::Full(_) => Error::Busy,
        mpsc::TrySendError::Disconnected(_) => Error::Unavailable(None),
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl<C: Collector> Supervised for Worker<C> {
    async fn run(mut self, shutdown: CancellationToken) {
        let runtime = Arc::clone(&self.runtime);
        let project = self.project.take();
        let mut job = tokio::task::spawn_blocking(move || self.work());
        let result = tokio::select! {
            result = &mut job => result,
            () = shutdown.cancelled() => { runtime.stop(); job.await },
            () = follow(project, Arc::clone(&runtime)) => { runtime.stop(); job.await },
        };
        runtime.stop();
        if result.is_err() {
            tracing::error!("vocabulary worker terminated unexpectedly");
        }
    }
}

/// Prepares each folder the settings name, whether a client's write or a
/// hand edit named it. Waits forever once the watch is gone.
async fn follow(
    project: Option<tokio::sync::watch::Receiver<Option<Directory>>>,
    runtime: Arc<Runtime>,
) {
    if let Some(mut project) = project {
        while project.changed().await.is_ok() {
            let directory = project.borrow_and_update().clone();
            runtime.prepare(directory);
        }
    }
    std::future::pending::<()>().await;
}

impl<C: Collector> Worker<C> {
    pub(super) fn initialize_personal(&mut self) -> Result<(), Error> {
        self.refresh_personal()
    }

    pub(super) fn follow(&mut self, project: tokio::sync::watch::Receiver<Option<Directory>>) {
        self.project = Some(project);
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
            self.follow_dictionary();
            let retired = std::mem::take(&mut self.runtime.registry().retired);
            drop(retired);
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
                            let _ = reply.send(Err(Error::Unavailable(None)));
                        }
                        continue;
                    }
                    if self.await_idle(&cancel).is_err() {
                        if self.runtime.stopping.load(Ordering::Acquire) {
                            break;
                        }
                        self.conclude(
                            directory.as_ref(),
                            &cancel,
                            Err(Error::Unavailable(None)),
                            reply,
                        );
                        continue;
                    }
                    let deadline = Deadline::after(REFRESH_BUDGET);
                    let scan = Deadline::after(SCAN_BUDGET);
                    let token = self.control(Arc::clone(&cancel), deadline);
                    let mut result = self
                        .reserve(DISCOVERY_RESERVE, directory.as_ref())
                        .and_then(|()| self.refresh_personal())
                        .and_then(|()| match directory.as_ref() {
                            Some(directory) => {
                                self.refresh_project(directory, scan, &cancel, &token, &mut catalog)
                            }
                            None => Ok(Status::Folder {
                                list: None,
                                activity: Activity::Idle,
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
                    self.conclude(directory.as_ref(), &cancel, result, reply);
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
                    let mut registry = self.runtime.registry();
                    if result.is_ok() {
                        registry.removals.remove(&directory);
                    } else {
                        registry.removals.insert(directory, RemovalState::Failed);
                    }
                    drop(registry);
                    if let Err(Err(error)) = reply.send(result) {
                        tracing::warn!(
                            error = %error_chain(&error),
                            "vocabulary removal failed; requester gone"
                        );
                    }
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
                Err(error) => return Err(Error::unavailable(error)),
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
            .registry()
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
        Ok(Status::Folder {
            list: None,
            activity: Activity::Idle,
        })
    }

    fn await_idle(&self, cancel: &AtomicBool) -> Result<(), Error> {
        loop {
            if cancel.load(Ordering::Acquire) || self.runtime.stopping.load(Ordering::Acquire) {
                return Err(Error::Unavailable(None));
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

    fn reserve(&self, reservation: usize, protected: Option<&Directory>) -> Result<(), Error> {
        loop {
            let mut registry = self.runtime.registry();
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
                            .live
                            .as_ref()
                            .is_some_and(|live| Arc::strong_count(&live.matcher) == 1)
                })
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                return Err(Error::TooLarge);
            };
            let entry = registry.entries.get_mut(&oldest).expect("selected entry");
            let retired = entry.live.take();
            entry.activity = Activity::Failed(Failure::Evicted);
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
            let opened = Catalog::open(&self.directory, token.clone())?;
            if let Some(key) = &self.cache_key {
                opened.remove_other_pipelines(key)?;
            }
            *catalog = Some(opened);
        }
        let catalog = catalog.as_mut().ok_or(Error::Unavailable(None))?;
        catalog.cancel_on(token.clone())?;
        Ok(catalog)
    }

    /// Picks up a save to the dictionary, from an editor or anything else,
    /// through a refresh, which waits for the presses in flight.
    fn follow_dictionary(&mut self) {
        if Instant::now() < self.next_poll {
            return;
        }
        self.next_poll = Instant::now() + DICTIONARY_POLL;
        if self.sources.dictionary.stamp() != self.stamp {
            self.runtime.prepare(None);
        }
    }

    /// A failed refresh keeps the stamp, so the poll waits for the next save
    /// rather than failing every second. An interrupted one retries.
    fn refresh_personal(&mut self) -> Result<(), Error> {
        let stamp = self.sources.dictionary.stamp();
        let result = self.compile_personal();
        if !matches!(result, Err(Error::Busy)) {
            self.stamp = stamp;
        }
        result
    }

    fn compile_personal(&mut self) -> Result<(), Error> {
        let interrupt = Arc::clone(&self.runtime.interrupt);
        let cancel = CancelToken::from_fn(move || interrupt.load(Ordering::Acquire));
        let rules = self.sources.rules(&cancel).map_err(|error| {
            if self.runtime.interrupt.load(Ordering::Acquire) {
                Error::Busy
            } else {
                Error::unavailable(error)
            }
        })?;
        if rules == self.personal {
            return Ok(());
        }
        let matcher = Arc::new(Vocabulary::compile_with(&[], rules.clone(), &cancel)?);
        let mut registry = self.runtime.registry();
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
        drop(registry);
        drop(retired);
        self.personal = rules;
        Ok(())
    }

    fn refresh_project(
        &mut self,
        directory: &Directory,
        scan: Deadline,
        cancel: &AtomicBool,
        token: &CancelToken,
        catalog: &mut Option<Catalog>,
    ) -> Result<Status, Error> {
        self.runtime.interrupted(cancel)?;
        let key = self.cache_key.clone().ok_or(Error::Unavailable(None))?;
        if self.collector.cache_key().as_ref() != Some(&key) {
            tracing::warn!("vocabulary collector changed its cache identity");
            return Err(Error::Unavailable(None));
        }
        let root = discover::resolve(directory.as_path()).map_err(Error::unavailable)?;
        self.runtime.interrupted(cancel)?;
        let cached = self.catalog(catalog, token)?.load(&root, &key)?;
        if let Some(cached) = &cached {
            let selection = select::terms(&cached.sources, token)?;
            let matcher = self.compile(&selection.terms, token)?;
            self.runtime.publish(
                directory,
                &root,
                Prepared {
                    matcher,
                    catalog_revision: Some(cached.revision),
                    found: selection.found,
                    // The catalog doesn't keep it.
                    completeness: None,
                },
                cancel,
            )?;
            self.catalog(catalog, token)?
                .remember_alias(directory.as_path(), &root)?;
        }
        if !self.runtime.persistent {
            return Ok(self.runtime.catalog_status(Some(directory)));
        }
        self.reserve(DISCOVERY_RESERVE, Some(directory))?;
        let previous = cached.map(|cached| cached.sources).unwrap_or_default();
        let discovery = self.collector.collect(directory, &previous, scan, token)?;
        if discovery.root != root {
            tracing::warn!("vocabulary selection changed during discovery");
            return Err(Error::Unavailable(None));
        }
        drop(previous);
        let selection = select::terms(&discovery.sources, token)?;
        let matcher = self.compile(&selection.terms, token)?;
        self.runtime.interrupted(cancel)?;
        // How far a scan gets in time depends on the machine's load, so a
        // short one goes live without replacing a longer stored scan.
        let catalog_revision = if discovery.coverage.completeness == Completeness::OutOfTime {
            None
        } else {
            Some(
                self.catalog(catalog, token)?
                    .refresh_cancellable(&discovery, &key, token)?,
            )
        };
        tracing::debug!(
            entries = discovery.coverage.entries,
            hidden = discovery.coverage.hidden,
            symlinks = discovery.coverage.symlinks,
            unsupported = discovery.coverage.unsupported,
            linked = discovery.coverage.linked,
            unreadable = discovery.coverage.unreadable,
            completeness = ?discovery.coverage.completeness,
            ignore_bytes = discovery.coverage.ignore_bytes,
            found = selection.found,
            "vocabulary collection completed"
        );
        if token.is_cancelled() {
            return Err(Error::Busy);
        }
        self.runtime.publish(
            directory,
            &root,
            Prepared {
                matcher,
                catalog_revision,
                found: selection.found,
                completeness: Some(discovery.coverage.completeness),
            },
            cancel,
        )?;
        Ok(self.runtime.catalog_status(Some(directory)))
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

    /// A handler that asked logs its own failure, so only a background
    /// refresh's, or one whose requester left, is logged here.
    fn conclude(
        &self,
        directory: Option<&Directory>,
        cancel: &AtomicBool,
        result: Result<Status, Error>,
        reply: Option<Reply>,
    ) {
        let current = self.finished(directory, cancel, &result);
        match (reply, result) {
            (Some(reply), result) => {
                if let Err(Err(error)) = reply.send(result) {
                    tracing::warn!(
                        error = %error_chain(&error),
                        "vocabulary refresh failed; requester gone"
                    );
                }
            }
            (None, Err(error)) if current => tracing::warn!(
                error = %error_chain(&error),
                "vocabulary refresh failed; retaining available snapshot"
            ),
            (None, _) => {}
        }
    }

    /// Whether `result` is still the folder's latest.
    fn finished(
        &self,
        directory: Option<&Directory>,
        cancel: &AtomicBool,
        result: &Result<Status, Error>,
    ) -> bool {
        let mut evicted = None;
        {
            let mut registry = self.runtime.registry();
            let key = directory.cloned();
            if !registry
                .pending
                .get(&key)
                .is_some_and(|pending| std::ptr::eq(pending.as_ref(), cancel))
            {
                return false;
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
                    registry
                        .entries
                        .insert(directory.clone(), Entry::new(Activity::Idle));
                }
                if let Some(entry) =
                    directory.and_then(|directory| registry.entries.get_mut(directory))
                {
                    entry.activity = Activity::Failed(if matches!(error, Error::TooLarge) {
                        Failure::Limit
                    } else {
                        Failure::Refresh
                    });
                }
            }
        }
        drop(evicted);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::log::testing::CapturedLogs;
    use super::*;
    use aria_voice::vocabulary::extract::{self, Extractor, LocalExtractor, TextInput};
    use aria_voice::vocabulary::{
        Candidate, Coverage, Discovery, Source, SourceKind, cache_key, discover::LocalCollector,
    };
    use std::ops::Range;

    fn directory(path: &Path) -> Directory {
        Directory::new(path.to_path_buf()).unwrap()
    }

    /// A settings file under `home` with the given vocabulary section.
    fn settings_with(home: &Path, vocabulary: &str) -> Arc<SettingsStore> {
        let path = home.join("settings.toml");
        std::fs::write(&path, format!("[dictation.vocabulary]\n{vocabulary}\n")).unwrap();
        let cache = aria_settings::SettingsCache::open(path, home.to_path_buf()).unwrap();
        Arc::new(SettingsStore::new(Arc::new(cache)))
    }

    fn settings(home: &Path) -> Arc<SettingsStore> {
        settings_with(home, "enabled = true")
    }

    fn local_runtime(root: &Path) -> (Arc<Runtime>, Worker<LocalCollector<LocalExtractor>>) {
        Runtime::new(
            root.join("catalog"),
            PersonalSources::unlisted(root),
            true,
            settings(root),
            LocalCollector::new(LocalExtractor),
        )
    }

    fn prepared(matcher: &Arc<Vocabulary>, revision: u64) -> Prepared {
        Prepared {
            matcher: Arc::clone(matcher),
            catalog_revision: Some(revision),
            found: 1,
            completeness: Some(Completeness::Whole),
        }
    }

    /// A folder whose current list dictation uses.
    fn is_ready(status: &Status) -> bool {
        matches!(
            status,
            Status::Folder { list: Some(list), activity: Activity::Idle } if !list.is_stale
        )
    }

    fn list(status: &Status) -> &List {
        match status {
            Status::Folder {
                list: Some(list), ..
            } => list,
            other => panic!("no list in {other:?}"),
        }
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
        out_of_time: bool,
    }

    impl Collector for StaticCollector {
        type Error = std::convert::Infallible;

        fn cache_key(&self) -> Option<String> {
            Some("test-collector-1".into())
        }

        fn collect(
            &mut self,
            selected: &Directory,
            previous: &[Source],
            _deadline: Deadline,
            _cancel: &CancelToken,
        ) -> Result<Discovery, collection::Error<Self::Error>> {
            self.previous_counts.push(previous.len());
            Ok(Discovery {
                selected: selected.as_path().to_path_buf(),
                root: self.root.clone(),
                sources: vec![Source {
                    relative_path: "aria-kit".into(),
                    kind: SourceKind::DirectoryName,
                    fingerprint: [1; 32],
                    input_bytes: 8,
                    candidates: vec![Candidate {
                        canonical: "aria-kit".into(),
                        span: 0..8,
                    }],
                }],
                coverage: Coverage {
                    entries: 1,
                    ignore_bytes: 8,
                    completeness: if self.out_of_time {
                        Completeness::OutOfTime
                    } else {
                        Completeness::Whole
                    },
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
            out_of_time: false,
        };
        let (runtime, mut worker) = Runtime::new(
            temporary.path().join("catalog"),
            PersonalSources::unlisted(temporary.path()),
            true,
            settings(temporary.path()),
            collector,
        );
        let mut catalog = None;
        for _ in 0..2 {
            runtime.prepare(Some(selected.clone()));
            let Command::Refresh { cancel, .. } = worker.requests.try_recv().unwrap() else {
                panic!("refresh expected")
            };
            let result = worker.refresh_project(
                &selected,
                Deadline::never(),
                &cancel,
                &CancelToken::never(),
                &mut catalog,
            );
            worker.finished(Some(&selected), &cancel, &result);
            assert!(is_ready(&result.unwrap()));
        }
        assert_eq!(worker.collector.previous_counts, [0, 1]);

        let snapshot = runtime.snapshot(Some(&selected));
        assert_eq!(
            snapshot.matcher.correct("Use ARIA dash kit.").as_deref(),
            Some("Use aria-kit.")
        );
    }

    fn static_runtime(
        base: &Path,
        selected: &Directory,
    ) -> (Arc<Runtime>, Worker<StaticCollector>) {
        let collector = StaticCollector {
            root: selected.as_path().canonicalize().unwrap(),
            previous_counts: Vec::new(),
            out_of_time: false,
        };
        Runtime::new(
            base.join("catalog"),
            PersonalSources::unlisted(base),
            true,
            settings(base),
            collector,
        )
    }

    fn refresh_once(
        runtime: &Runtime,
        worker: &mut Worker<StaticCollector>,
        selected: &Directory,
    ) -> Result<Status, Error> {
        runtime.prepare(Some(selected.clone()));
        let Command::Refresh { cancel, .. } = worker.requests.try_recv().unwrap() else {
            panic!("refresh expected")
        };
        let result = worker.refresh_project(
            selected,
            Deadline::never(),
            &cancel,
            &CancelToken::never(),
            &mut None,
        );
        worker.finished(Some(selected), &cancel, &result);
        result
    }

    #[test]
    fn a_scan_out_of_time_goes_live_without_replacing_the_stored_one() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "project");
        let (runtime, mut worker) = static_runtime(temporary.path(), &selected);
        let stored = refresh_once(&runtime, &mut worker, &selected).unwrap();
        worker.collector.out_of_time = true;

        let status = refresh_once(&runtime, &mut worker, &selected).unwrap();

        assert!(is_ready(&status));
        assert_eq!(list(&status).completeness, Some(Completeness::OutOfTime));
        assert_eq!(list(&status).catalog_revision, None);
        let catalog =
            Catalog::open(&temporary.path().join("catalog"), CancelToken::never()).unwrap();
        let root = selected.as_path().canonicalize().unwrap();
        assert_eq!(
            Some(
                catalog
                    .load(&root, "test-collector-1")
                    .unwrap()
                    .unwrap()
                    .revision
            ),
            list(&stored).catalog_revision
        );
    }

    #[test]
    fn a_refresh_removes_folders_an_earlier_pipeline_stored() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "project");
        let older = temporary.path().join("older-project");
        let mut catalog =
            Catalog::open(&temporary.path().join("catalog"), CancelToken::never()).unwrap();
        let stored = Discovery {
            selected: older.clone(),
            root: older.clone(),
            sources: Vec::new(),
            coverage: Coverage::default(),
        };
        catalog
            .refresh_cancellable(&stored, "scan3:older", &CancelToken::never())
            .unwrap();
        drop(catalog);
        let (runtime, mut worker) = static_runtime(temporary.path(), &selected);

        refresh_once(&runtime, &mut worker, &selected).unwrap();

        let catalog =
            Catalog::open(&temporary.path().join("catalog"), CancelToken::never()).unwrap();
        assert!(catalog.load(&older, "scan3:older").unwrap().is_none());
    }

    #[test]
    fn a_poisoned_registry_still_serves_the_next_refresh() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "project");
        let (runtime, mut worker) = static_runtime(temporary.path(), &selected);
        let poisoner = Arc::clone(&runtime);
        std::thread::spawn(move || {
            let _registry = poisoner.registry.lock().unwrap();
            panic!("poison the registry");
        })
        .join()
        .unwrap_err();
        assert!(runtime.registry.is_poisoned());

        let result = refresh_once(&runtime, &mut worker, &selected);

        assert!(is_ready(&result.unwrap()));
        assert!(is_ready(&runtime.status(Some(selected)).unwrap()));
    }

    #[test]
    fn a_catalog_failure_keeps_its_cause_under_the_same_code() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "project");
        std::fs::write(temporary.path().join("catalog"), "").unwrap();
        let (runtime, mut worker) = static_runtime(temporary.path(), &selected);

        let error = refresh_once(&runtime, &mut worker, &selected).unwrap_err();

        assert!(
            std::error::Error::source(&error)
                .and_then(|cause| cause.downcast_ref::<catalog::Error>())
                .is_some(),
            "{error:?}"
        );
        assert!(
            error_chain(&error).starts_with("vocabulary worker is unavailable: vocabulary catalog")
        );
        assert_eq!(RpcErrorCode::from(&error), RpcErrorCode::Internal);
    }

    #[test]
    fn a_refresh_whose_requester_left_logs_its_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "project");
        let (_runtime, worker) = static_runtime(temporary.path(), &selected);
        let (reply, receiver) = oneshot::channel();
        drop(receiver);
        let logs = CapturedLogs::start();

        worker.conclude(
            Some(&selected),
            &AtomicBool::new(false),
            Err(Error::Unavailable(None)),
            Some(reply),
        );

        let log = logs.text();
        assert!(log.contains("requester gone"), "{log}");
    }

    #[test]
    fn injected_collection_cannot_publish_evidence_from_another_root() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "project");
        let collector = StaticCollector {
            root: temporary.path().join("other-project"),
            previous_counts: Vec::new(),
            out_of_time: false,
        };
        let (runtime, mut worker) = Runtime::new(
            temporary.path().join("catalog"),
            PersonalSources::unlisted(temporary.path()),
            true,
            settings(temporary.path()),
            collector,
        );
        runtime.prepare(Some(selected.clone()));
        let Command::Refresh { cancel, .. } = worker.requests.try_recv().unwrap() else {
            panic!("refresh expected")
        };
        let mut catalog = None;
        let result = worker.refresh_project(
            &selected,
            Deadline::never(),
            &cancel,
            &CancelToken::never(),
            &mut catalog,
        );
        worker.finished(Some(&selected), &cancel, &result);
        assert!(matches!(result, Err(Error::Unavailable(None))));
        assert!(matches!(
            runtime.catalog_status(Some(&selected)),
            Status::Folder { list: None, .. }
        ));
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
                    &worker.cache_key.unwrap(),
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_request_without_a_directory_uses_the_configured_one() {
        let root = tempfile::tempdir().unwrap();
        let configured = project(root.path(), "configured");
        let (runtime, mut worker) = Runtime::new(
            root.path().join("catalog"),
            PersonalSources::unlisted(root.path()),
            true,
            settings_with(
                root.path(),
                &format!("enabled = true\ndir = {:?}", configured.as_path()),
            ),
            LocalCollector::new(LocalExtractor),
        );
        runtime.prepare(Some(configured.clone()));
        let Command::Refresh { cancel, .. } = worker.requests.try_recv().unwrap() else {
            panic!("refresh expected")
        };
        let mut catalog = None;
        let result = worker.refresh_project(
            &configured,
            Deadline::never(),
            &cancel,
            &CancelToken::never(),
            &mut catalog,
        );
        worker.finished(Some(&configured), &cancel, &result);

        let status = runtime.status(None).unwrap();

        assert!(is_ready(&status));
        assert_eq!(status, runtime.catalog_status(Some(&configured)));
    }

    #[test]
    fn a_scan_cut_short_publishes_a_ready_partial_list() {
        let root = tempfile::tempdir().unwrap();
        let selected = project(root.path(), "aria-kit");
        std::fs::write(selected.as_path().join("Cargo.toml"), "").unwrap();
        let mut deep = selected.as_path().to_path_buf();
        for _ in 0..17 {
            deep.push("nested");
        }
        std::fs::create_dir_all(&deep).unwrap();
        let (runtime, mut worker) = local_runtime(root.path());
        runtime.prepare(Some(selected.clone()));
        let Command::Refresh { cancel, .. } = worker.requests.try_recv().unwrap() else {
            panic!("refresh expected")
        };

        let status = worker
            .refresh_project(
                &selected,
                Deadline::never(),
                &cancel,
                &CancelToken::never(),
                &mut None,
            )
            .unwrap();

        assert!(is_ready(&status));
        assert_eq!(list(&status).completeness, Some(Completeness::Partial));
        assert_eq!((list(&status).terms, list(&status).found), (3, 3));
    }

    #[test]
    fn a_disabled_vocabulary_reports_disabled() {
        let root = tempfile::tempdir().unwrap();
        let selected = project(root.path(), "project");
        let (runtime, _worker) = Runtime::new(
            root.path().join("catalog"),
            PersonalSources::unlisted(root.path()),
            true,
            settings_with(root.path(), "enabled = false"),
            LocalCollector::new(LocalExtractor),
        );

        let status = runtime.status(Some(selected)).unwrap();

        assert_eq!(status, Status::Disabled);
    }

    #[tokio::test]
    async fn a_change_without_a_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, _worker) = local_runtime(root.path());

        assert!(matches!(
            runtime.refresh(None).await,
            Err(Error::NoDirectory)
        ));
        assert!(matches!(
            runtime.forget(None).await,
            Err(Error::NoDirectory)
        ));
    }

    #[test]
    fn cold_admission_coalesces_and_bounds_work_without_filesystem_access() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, _worker) = local_runtime(root.path());
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
        std::fs::write(root.path().join("dictionary.txt"), "db2\n").unwrap();
        let (runtime, mut worker) = Runtime::new(
            root.path().join("catalog"),
            PersonalSources::new(
                Arc::new(Dictionary::new(root.path().join("dictionary.txt"))),
                root.path().to_owned(),
            ),
            true,
            settings(root.path()),
            LocalCollector::new(LocalExtractor),
        );
        worker.initialize_personal().unwrap();
        let admitted = runtime.snapshot(None);
        assert_eq!(admitted.matcher.correct("d b two").as_deref(), Some("db2"));
        std::fs::remove_file(root.path().join("dictionary.txt")).unwrap();
        assert!(matches!(worker.refresh_personal(), Err(Error::Busy)));
        let pinned = Arc::clone(&admitted.matcher);
        drop(admitted);
        worker.await_idle(&AtomicBool::new(false)).unwrap();
        worker.refresh_personal().unwrap();
        assert_eq!(runtime.snapshot(None).matcher.correct("d b two"), None);
        assert_eq!(pinned.correct("d b two").as_deref(), Some("db2"));
    }

    #[test]
    fn an_added_word_corrects_after_the_next_refresh() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, mut worker) = Runtime::new(
            root.path().join("catalog"),
            PersonalSources::new(
                Arc::new(Dictionary::new(root.path().join("dictionary.txt"))),
                root.path().to_owned(),
            ),
            true,
            settings(root.path()),
            LocalCollector::new(LocalExtractor),
        );
        assert_eq!(runtime.add_word("db2").unwrap(), AddWordOutcome::Added);
        assert_eq!(
            runtime.add_word("db2").unwrap(),
            AddWordOutcome::AlreadyListed
        );
        assert_eq!(runtime.list_words().unwrap(), ["db2"]);
        let invalid = runtime.add_word("# note").unwrap_err();
        assert_eq!(RpcErrorCode::from(&invalid), RpcErrorCode::InvalidRequest);
        worker.refresh_personal().unwrap();
        assert_eq!(
            runtime.snapshot(None).matcher.correct("d b two").as_deref(),
            Some("db2")
        );
    }

    #[test]
    fn a_removed_word_stops_correcting() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("dictionary.txt"), "db2\n").unwrap();
        let (runtime, mut worker) = Runtime::new(
            root.path().join("catalog"),
            PersonalSources::new(
                Arc::new(Dictionary::new(root.path().join("dictionary.txt"))),
                root.path().to_owned(),
            ),
            true,
            settings(root.path()),
            LocalCollector::new(LocalExtractor),
        );
        worker.initialize_personal().unwrap();
        assert_eq!(
            runtime.remove_word("db2").unwrap(),
            RemoveWordOutcome::Removed
        );
        assert_eq!(
            runtime.remove_word("db2").unwrap(),
            RemoveWordOutcome::NotListed
        );
        assert!(runtime.list_words().unwrap().is_empty());
        worker.refresh_personal().unwrap();
        assert_eq!(runtime.snapshot(None).matcher.correct("d b two"), None);
    }

    #[tokio::test]
    async fn a_saved_dictionary_applies_without_a_press() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("dictionary.txt");
        std::fs::write(&path, "db2\n").unwrap();
        let (runtime, mut worker) = Runtime::new(
            root.path().join("catalog"),
            PersonalSources::new(
                Arc::new(Dictionary::new(path.clone())),
                root.path().to_owned(),
            ),
            true,
            settings(root.path()),
            LocalCollector::new(LocalExtractor),
        );
        worker.initialize_personal().unwrap();
        let revision = runtime.registry().personal_revision;
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(worker.run(shutdown.clone()));
        // A press queues a refresh. One soon after queues none, and leaves the
        // interrupt raised for whatever the worker runs next.
        drop(runtime.snapshot(None));
        while !runtime.registry().pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(runtime.snapshot(None));
        std::fs::write(&path, "db2\nvex16\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let applied = loop {
            if runtime.registry().personal_revision != revision {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        shutdown.cancel();
        task.await.unwrap();
        assert!(applied, "the save was never applied");
        assert_eq!(
            runtime
                .snapshot(None)
                .matcher
                .correct("vex sixteen")
                .as_deref(),
            Some("vex16")
        );
    }

    #[tokio::test]
    async fn one_folders_terms_stay_out_of_anothers_list() {
        let root = tempfile::tempdir().unwrap();
        let a = project(root.path(), "aria-stt");
        let b = project(root.path(), "other-stt");
        let (runtime, worker) = local_runtime(root.path());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(worker.run(shutdown.clone()));
        assert!(is_ready(&runtime.refresh(Some(a.clone())).await.unwrap()));
        runtime.refresh(Some(b.clone())).await.unwrap();

        let snapshot = runtime.snapshot(Some(&a));

        assert_eq!(
            snapshot.matcher.correct("aria dash stt").as_deref(),
            Some("aria-stt")
        );
        assert_eq!(snapshot.matcher.correct("other dash stt"), None);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_refresh_is_busy_while_a_snapshot_is_held() {
        let root = tempfile::tempdir().unwrap();
        let a = project(root.path(), "aria-stt");
        let (runtime, worker) = local_runtime(root.path());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(worker.run(shutdown.clone()));
        assert!(is_ready(&runtime.refresh(Some(a.clone())).await.unwrap()));
        let _held = runtime.snapshot(Some(&a));

        assert!(matches!(
            runtime.refresh(Some(a.clone())).await,
            Err(Error::Busy)
        ));
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_last_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let a = project(root.path(), "aria-stt");
        let (runtime, worker) = local_runtime(root.path());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(worker.run(shutdown.clone()));
        assert!(is_ready(&runtime.refresh(Some(a.clone())).await.unwrap()));
        std::fs::rename(a.as_path(), root.path().join("moved")).unwrap();

        assert!(runtime.refresh(Some(a.clone())).await.is_err());

        let status = runtime.catalog_status(Some(&a));
        assert!(matches!(
            status,
            Status::Folder {
                list: Some(_),
                activity: Activity::Failed(Failure::Refresh)
            }
        ));
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
        let (runtime, worker) = local_runtime(root.path());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(worker.run(shutdown.clone()));
        runtime.refresh(Some(parent.clone())).await.unwrap();
        runtime.refresh(Some(child.clone())).await.unwrap();
        let pinned = runtime.snapshot(Some(&parent));
        let forget_runtime = Arc::clone(&runtime);
        let forgotten = child.clone();
        let forget = tokio::spawn(async move { forget_runtime.forget(Some(forgotten)).await });
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
        let key = cache_key(LocalExtractor.cache_key()).unwrap();
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
            PersonalSources::unlisted(root.path()),
            true,
            settings(root.path()),
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
        let (_, worker) = local_runtime(root.path());
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
            &[],
            &mut LocalExtractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        let key = cache_key(LocalExtractor.cache_key()).unwrap();
        let catalog_dir = temporary.path().join("catalog");
        let mut catalog = Catalog::open(&catalog_dir, CancelToken::never()).unwrap();
        catalog
            .refresh_cancellable(&discovery, &key, &CancelToken::never())
            .unwrap();
        let (_, worker) = local_runtime(temporary.path());
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
    fn a_list_built_before_a_rules_change_reads_as_stale() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = project(temporary.path(), "project");
        let (runtime, mut worker) = static_runtime(temporary.path(), &selected);
        refresh_once(&runtime, &mut worker, &selected).unwrap();

        runtime.registry().personal_revision += 1;

        let status = runtime.catalog_status(Some(&selected));
        assert!(list(&status).is_stale);
        assert!(!is_ready(&status));
    }

    #[test]
    fn an_evicted_list_reports_no_names() {
        let root = tempfile::tempdir().unwrap();
        let selected = directory(&root.path().join("project"));
        let (runtime, worker) = local_runtime(root.path());
        runtime.prepare(Some(selected.clone()));
        let Command::Refresh { cancel, .. } = worker.requests.try_recv().unwrap() else {
            panic!("refresh expected")
        };
        let matcher = Arc::new(Vocabulary::compile(&[], PersonalRules::default()).unwrap());
        let prepared = prepared(&matcher, 1);
        drop(matcher);
        runtime
            .publish(&selected, selected.as_path(), prepared, &cancel)
            .unwrap();

        let _ = worker.reserve(LIVE_BYTES, None);

        let status = runtime.status(Some(selected)).unwrap();
        assert_eq!(
            status,
            Status::Folder {
                list: None,
                activity: Activity::Failed(Failure::Evicted)
            }
        );
    }

    #[test]
    fn a_reserve_that_cannot_evict_keeps_the_protected_list() {
        let root = tempfile::tempdir().unwrap();
        let selected = directory(&root.path().join("project"));
        let (runtime, worker) = local_runtime(root.path());
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
        runtime
            .publish(
                &selected,
                selected.as_path(),
                prepared(&matcher, 1),
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
    }

    #[test]
    fn a_superseded_refresh_cannot_publish() {
        let root = tempfile::tempdir().unwrap();
        let selected = directory(&root.path().join("project"));
        let (runtime, worker) = local_runtime(root.path());
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
        runtime
            .publish(
                &selected,
                selected.as_path(),
                prepared(&matcher, 1),
                &cancel,
            )
            .unwrap();
        runtime.registry.lock().unwrap().pending.clear();
        worker.await_idle(&AtomicBool::new(false)).unwrap();

        assert!(matches!(
            runtime.publish(
                &selected,
                selected.as_path(),
                prepared(&matcher, 2),
                &cancel
            ),
            Err(Error::Unavailable(None))
        ));
    }
}
