//! The daemon's cache of the file. A 1 s stat poll reloads it on change, and
//! every reload or write re-arms the live watches.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::Directory;
use super::document::{self, Loaded, SettingsFile, Stamp, Text};
use super::write::{self, Refusal, Settled};
use super::{Apply, Key, Patch, Revision, SettingsError, SettingsFault, SettingsStatus, Written};
use crate::hotkey::AriaKey;
use crate::primitives::error_chain;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const START_READ_ATTEMPTS: usize = 3;
/// A reload that undoes a key written this recently is an editor saving a
/// stale buffer over the daemon's write.
const REVERT_WINDOW: Duration = Duration::from_secs(10);

/// The restart-marked values the daemon started with. Engines keep these
/// until a restart; a reload only compares against them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupSettings(BTreeMap<Key, serde_json::Value>);

impl StartupSettings {
    fn freeze(settings: &SettingsFile) -> Self {
        let values = settings.values();
        Self(
            Key::ALL
                .iter()
                .copied()
                .filter(|key| key.apply() == Apply::Restart)
                .map(|key| (key, document::value_at(&values, key)))
                .collect(),
        )
    }

    fn restart_required(&self, settings: &SettingsFile) -> Vec<Key> {
        let values = settings.values();
        self.0
            .iter()
            .filter(|(key, frozen)| document::value_at(&values, **key) != **frozen)
            .map(|(key, _frozen)| *key)
            .collect()
    }

    /// `current` with every restart key put back to its startup value.
    ///
    /// Each key's value was valid on its own, so the mix always parses.
    fn apply(&self, current: &SettingsFile) -> Result<SettingsFile, SettingsError> {
        let mut values = current.values();
        for (key, frozen) in &self.0 {
            document::replace_at(&mut values, *key, frozen.clone());
        }
        serde_json::from_value(values).map_err(SettingsError::Freeze)
    }
}

struct RecentWrite {
    key: Key,
    value: serde_json::Value,
    at: Instant,
}

struct State {
    /// The last valid file.
    loaded: Arc<Loaded>,
    /// What operations read: live keys from `loaded`, restart keys from the
    /// startup snapshot.
    effective: Arc<SettingsFile>,
    stamp: Option<Stamp>,
    recent: Vec<RecentWrite>,
}

pub(crate) struct SettingsCache {
    path: PathBuf,
    home: PathBuf,
    startup: StartupSettings,
    state: Mutex<State>,
    aria_key: watch::Sender<Option<AriaKey>>,
    project_vocabulary: watch::Sender<Option<Directory>>,
    status: watch::Sender<SettingsStatus>,
}

#[derive(Debug)]
pub(crate) enum Reload {
    Unchanged,
    Reloaded {
        restart_required: Vec<Key>,
        /// Keys the daemon wrote in the last 10 s that the file no longer
        /// holds that way.
        reverted: Vec<Key>,
    },
    /// The file is invalid; the last good values stand.
    Invalid(SettingsError),
}

impl SettingsCache {
    /// Reads the file once. An invalid file is fatal here, with its position.
    pub(crate) fn open(path: PathBuf, home: PathBuf) -> Result<Self, SettingsError> {
        let text = text_at_start(&path)?;
        let stamp = text.as_ref().map(|text| text.stamp);
        let loaded = Loaded::parse(text.as_ref().map_or("", |text| &text.raw), &home)?;
        let (aria_key, _) = watch::channel(loaded.settings.aria_key());
        let (project_vocabulary, _) = watch::channel(loaded.settings.project_vocabulary());
        let (status, _) = watch::channel(SettingsStatus {
            revision: loaded.revision.get(),
            ..Default::default()
        });
        let startup = StartupSettings::freeze(&loaded.settings);
        let effective = Arc::new(loaded.settings.clone());
        Ok(Self {
            path,
            home,
            startup,
            state: Mutex::new(State {
                loaded: Arc::new(loaded),
                effective,
                stamp,
                recent: Vec::new(),
            }),
            aria_key,
            project_vocabulary,
            status,
        })
    }

    pub(crate) fn snapshot(&self) -> Arc<Loaded> {
        Arc::clone(&self.lock_state().loaded)
    }

    /// What an operation admitted now reads: restart keys stay frozen.
    pub(crate) fn effective(&self) -> Arc<SettingsFile> {
        Arc::clone(&self.lock_state().effective)
    }

    /// The Aria key, `None` while it is off. Fires only when the key
    /// changes, so the listener is not re-armed by unrelated writes.
    pub(crate) fn aria_key(&self) -> watch::Receiver<Option<AriaKey>> {
        self.aria_key.subscribe()
    }

    /// The project folder to read terms from, `None` while project
    /// vocabulary is off. Fires only when it changes.
    pub(crate) fn project_vocabulary(&self) -> watch::Receiver<Option<Directory>> {
        self.project_vocabulary.subscribe()
    }

    /// Fires on every change of the status: a fault, a fix, a restart key,
    /// a revert.
    pub(crate) fn status(&self) -> watch::Receiver<SettingsStatus> {
        self.status.subscribe()
    }

    /// Restart-only keys whose file value differs from the running one.
    pub(crate) fn restart_required(&self) -> Vec<String> {
        self.status.borrow().restart_required.clone()
    }

    pub(crate) fn check_patch(&self, patch: &Patch) -> Result<Written, SettingsError> {
        write::check_patch(&self.path, &self.home, patch)
    }

    /// The one write path, with the patch made from the file as it stands
    /// under the lock. The daemon's own writes pass no expected revision.
    pub(crate) fn modify<E: From<SettingsError>>(
        &self,
        expected: Option<Revision>,
        edit: impl FnOnce(&SettingsFile) -> Result<Patch, E>,
    ) -> Result<Written, E> {
        let mut keys = Vec::new();
        let written = write::modify(
            &self.path,
            &self.home,
            expected,
            |current| -> Result<Patch, E> {
                let patch = edit(current)?;
                keys = patch.keys().collect();
                Ok(patch)
            },
        )?;
        match &written {
            Written::Applied(loaded) | Written::AppliedRestartKey(loaded) => {
                self.remember(loaded, &keys)?;
            }
            // The file is valid and newer than the cache, so it is the cache.
            Written::Refused(Refusal::Conflict { current }) => {
                self.remember(current, &[])?;
            }
            Written::Refused(
                Refusal::FileInvalid(_) | Refusal::Invalid(_) | Refusal::ChangedRecently,
            ) => {}
        }
        Ok(written)
    }

    fn remember(&self, loaded: &Loaded, written: &[Key]) -> Result<(), SettingsError> {
        let mut state = self.lock_state();
        let values = loaded.settings.values();
        let now = Instant::now();
        state
            .recent
            .retain(|write| now.duration_since(write.at) < REVERT_WINDOW);
        // The last write of a key is the only one a reload can undo.
        state.recent.retain(|write| !written.contains(&write.key));
        for key in written {
            state.recent.push(RecentWrite {
                key: *key,
                value: document::value_at(&values, *key),
                at: now,
            });
        }
        if is_older(loaded.stamp, state.stamp) {
            return Ok(());
        }
        // The poll sees this stamp as already known, so a write of ours is
        // not reloaded. An edit after the write has another stamp, and is.
        state.stamp = loaded.stamp;
        self.install(&mut state, loaded.clone())?;
        self.publish(
            None,
            &self.startup.restart_required(&loaded.settings),
            &[],
            loaded.revision.get(),
        );
        Ok(())
    }

    /// One check of the file. The poll thread runs this every second; tests
    /// call it directly.
    pub(crate) fn poll(&self) -> Result<Reload, SettingsError> {
        let seen = Stamp::of(&self.path)?;
        let known = self.lock_state().stamp;
        if seen == known {
            return Ok(Reload::Unchanged);
        }
        // Outside the state lock: operations keep reading while this waits.
        // A save still landing stays unseen, so the next poll takes it whole.
        let parsed = match write::settled_text(&self.path) {
            Ok(Settled::Changing) => return Ok(Reload::Unchanged),
            Ok(Settled::Text(text)) => {
                let stamp = text.as_ref().map(|text| text.stamp);
                Loaded::parse(text.as_ref().map_or("", |text| &text.raw), &self.home)
                    .map(|loaded| (loaded, stamp))
            }
            Err(error) if error.is_file_fault() => Err(error),
            Err(error) => return Err(error),
        };
        let mut state = self.lock_state();
        // A write landed during the read, so what was read may be older. The
        // next poll compares against what that write left.
        if state.stamp != known {
            return Ok(Reload::Unchanged);
        }
        let loaded = match parsed {
            Ok((loaded, stamp)) => {
                state.stamp = stamp;
                loaded
            }
            Err(error) => {
                state.stamp = seen;
                let restart_required = self.startup.restart_required(&state.loaded.settings);
                self.publish(
                    Some(error.fault()),
                    &restart_required,
                    &[],
                    state.loaded.revision.get(),
                );
                return Ok(Reload::Invalid(error));
            }
        };
        let restart_required = self.startup.restart_required(&loaded.settings);
        let values = loaded.settings.values();
        let now = Instant::now();
        state
            .recent
            .retain(|write| now.duration_since(write.at) < REVERT_WINDOW);
        let reverted: Vec<Key> = state
            .recent
            .iter()
            .filter(|write| document::value_at(&values, write.key) != write.value)
            .map(|write| write.key)
            .collect();
        state.recent.retain(|write| !reverted.contains(&write.key));
        let revision = loaded.revision.get();
        self.install(&mut state, loaded)?;
        self.publish(None, &restart_required, &reverted, revision);
        Ok(Reload::Reloaded {
            restart_required,
            reverted,
        })
    }

    /// Takes a valid file as the cache. Owns every live watch sender, so a
    /// hand edit re-arms them the same way a write does.
    fn install(&self, state: &mut State, loaded: Loaded) -> Result<(), SettingsError> {
        state.effective = Arc::new(self.startup.apply(&loaded.settings)?);
        send_if_changed(&self.aria_key, loaded.settings.aria_key());
        send_if_changed(
            &self.project_vocabulary,
            loaded.settings.project_vocabulary(),
        );
        state.loaded = Arc::new(loaded);
        Ok(())
    }

    fn publish(
        &self,
        fault: Option<SettingsFault>,
        restart_required: &[Key],
        reverted: &[Key],
        revision: u64,
    ) {
        let paths = |keys: &[Key]| keys.iter().map(|key| key.path().to_owned()).collect();
        send_if_changed(
            &self.status,
            SettingsStatus {
                fault,
                restart_required: paths(restart_required),
                reverted: paths(reverted),
                revision,
            },
        );
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Restart keys freeze from this read, so it must not take a prefix of a save.
fn text_at_start(path: &Path) -> Result<Option<Text>, SettingsError> {
    for _attempt in 0..START_READ_ATTEMPTS {
        if let Settled::Text(text) = write::settled_text(path)? {
            return Ok(text);
        }
    }
    Err(SettingsError::TornRead {
        path: path.to_path_buf(),
    })
}

/// A write that finished after a newer one must not replace it. Only writes
/// compare this way: a hand-restored backup can carry any older time.
fn is_older(stamp: Option<Stamp>, seen: Option<Stamp>) -> bool {
    match (stamp, seen) {
        (Some(stamp), Some(seen)) => stamp.modified < seen.modified,
        (None, Some(_)) | (Some(_), None) | (None, None) => false,
    }
}

fn send_if_changed<T: PartialEq>(sender: &watch::Sender<T>, value: T) {
    sender.send_if_modified(|seen| {
        if *seen == value {
            return false;
        }
        *seen = value;
        true
    });
}

/// Checks the file every second until dropped.
pub(crate) struct SettingsPoll {
    stop: Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SettingsPoll {
    pub(crate) fn spawn(cache: Arc<SettingsCache>) -> Result<Self, SettingsError> {
        let (stop, stopped) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("aria-settings-poll".to_owned())
            .spawn(move || poll_until_stopped(&cache, &stopped))
            .map_err(SettingsError::Thread)?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for SettingsPoll {
    fn drop(&mut self) {
        // A failed send means the thread has already gone, which join reports.
        self.stop.send(()).ok();
        let Some(thread) = self.thread.take() else {
            return;
        };
        if thread.join().is_err() {
            tracing::error!("the settings poll thread panicked");
        }
    }
}

fn poll_until_stopped(cache: &SettingsCache, stopped: &Receiver<()>) {
    while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(POLL_INTERVAL) {
        match cache.poll() {
            Ok(reload) => report(&reload),
            Err(error) => {
                tracing::warn!(error = %error_chain(&error), "settings poll failed");
            }
        }
    }
}

/// Names the keys, never their values.
fn report(reload: &Reload) {
    match reload {
        Reload::Unchanged => {}
        Reload::Reloaded {
            restart_required,
            reverted,
        } => {
            tracing::info!("settings reloaded");
            for key in reverted {
                tracing::warn!(%key, "an editor save reverted a key the daemon wrote");
            }
            for key in restart_required {
                tracing::info!(%key, "a restart-only key changed; restart to apply");
            }
        }
        // The message can quote a value, so only the place is logged.
        Reload::Invalid(error) => match error.position() {
            Some(position) => tracing::warn!(
                line = position.line,
                column = position.column,
                "settings.toml is invalid; keeping the last good settings"
            ),
            None => tracing::warn!("settings.toml is invalid; keeping the last good settings"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::Position;
    use super::super::document::tests::HOME;
    use super::*;

    fn store(dir: &std::path::Path, text: &str) -> SettingsCache {
        let path = dir.join("settings.toml");
        fs::write(&path, text).expect("written");
        SettingsCache::open(path, PathBuf::from(HOME)).expect("opens")
    }

    /// A hand edit that lands in the same mtime tick as the open would hide
    /// from a stat poll, so tests push the clock past it.
    fn hand_edit(store: &SettingsCache, text: &str) {
        thread::sleep(Duration::from_millis(20));
        fs::write(&store.path, text).expect("hand edit");
    }

    fn write(store: &SettingsCache, patch: &Patch) -> Written {
        store
            .modify(None, |_current| Ok::<_, SettingsError>(patch.clone()))
            .expect("writes")
    }

    fn reloaded(reload: Reload) -> (Vec<Key>, Vec<Key>) {
        match reload {
            Reload::Reloaded {
                restart_required,
                reverted,
            } => (restart_required, reverted),
            other => panic!("expected Reloaded, got {other:?}"),
        }
    }

    #[test]
    fn a_hand_edit_is_reloaded_and_re_arms_the_live_watch() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = store(dir.path(), "[memory]\nenabled = false\n");
        let mut aria_key = store.aria_key();
        let revision = store.snapshot().revision;
        assert!(matches!(store.poll().expect("polls"), Reload::Unchanged));

        hand_edit(&store, "[memory]\nenabled = true\n");
        let (restart_required, reverted) = reloaded(store.poll().expect("polls"));

        assert!(restart_required.is_empty() && reverted.is_empty());
        assert_eq!(store.snapshot().settings.memory.enabled, Some(true));
        assert_ne!(store.snapshot().revision, revision);
        assert!(
            !aria_key.has_changed().expect("alive"),
            "an unrelated edit leaves the key alone"
        );

        hand_edit(&store, "[shortcuts]\naria_key = \"fn\"\n");
        reloaded(store.poll().expect("polls"));
        assert!(aria_key.has_changed().expect("alive"));
        assert_eq!(*aria_key.borrow_and_update(), Some(AriaKey::Fn));

        let mut vocabulary = store.project_vocabulary();
        hand_edit(
            &store,
            "[shortcuts]\naria_key = \"off\"\n[dictation.vocabulary]\nenabled = true\ndir = \"/src/writ\"\n",
        );
        reloaded(store.poll().expect("polls"));
        assert_eq!(*aria_key.borrow_and_update(), None);
        assert!(vocabulary.has_changed().expect("alive"));
        assert_eq!(
            vocabulary
                .borrow_and_update()
                .as_ref()
                .map(Directory::as_str),
            Some("/src/writ")
        );
    }

    fn memory_on() -> Patch {
        let mut patch = Patch::new();
        patch.set(Key::MemoryEnabled, true);
        patch
    }

    /// An edit landing between the write's lock and its
    /// `remember` must still count as unseen.
    #[test]
    fn an_edit_right_after_a_write_is_still_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "");
        let written = write::modify::<SettingsError>(&store.path, &store.home, None, |_current| {
            Ok(memory_on())
        })
        .expect("writes");
        hand_edit(&store, "[history]\nenabled = false\n");
        let Written::Applied(loaded) = written else {
            panic!("applied");
        };

        store
            .remember(&loaded, &[Key::MemoryEnabled])
            .expect("remembers");
        reloaded(store.poll().expect("polls"));

        assert_eq!(store.effective().history.enabled, Some(false));
    }

    #[test]
    fn an_older_write_never_replaces_a_newer_cache() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "");
        let older = write::modify::<SettingsError>(&store.path, &store.home, None, |_current| {
            Ok(memory_on())
        })
        .expect("writes");
        thread::sleep(Duration::from_millis(20));
        let newer = write::modify::<SettingsError>(&store.path, &store.home, None, |_current| {
            let mut patch = Patch::new();
            patch.set(Key::HistoryEnabled, false);
            Ok(patch)
        })
        .expect("writes");
        let (Written::Applied(older), Written::Applied(newer)) = (older, newer) else {
            panic!("applied");
        };

        store.remember(&newer, &[]).expect("remembers");
        store.remember(&older, &[]).expect("remembers");

        assert_eq!(store.effective().history.enabled, Some(false));
    }

    #[test]
    fn a_restored_backup_with_an_older_time_is_still_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "[memory]\nenabled = true\n");
        hand_edit(&store, "[memory]\nenabled = false\n");
        let backup = std::time::SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&store.path)
            .unwrap()
            .set_modified(backup)
            .unwrap();

        reloaded(store.poll().expect("polls"));

        assert_eq!(store.effective().memory.enabled, Some(false));
    }

    /// Bytes that aren't text are a fault in the file, reported like any
    /// other, and later polls don't lose it.
    #[test]
    fn a_file_that_is_not_text_is_a_status_fault() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "[memory]\nenabled = true\n");
        let status = store.status();
        thread::sleep(Duration::from_millis(20));
        fs::write(&store.path, [0xff, 0xfe]).unwrap();

        assert!(matches!(store.poll().expect("polls"), Reload::Invalid(_)));

        assert!(status.borrow().fault.is_some());
        assert_eq!(store.effective().memory.enabled, Some(true));
        assert!(matches!(store.poll().expect("polls"), Reload::Unchanged));
        let refused = store
            .modify(None, |_current| Ok::<_, SettingsError>(memory_on()))
            .expect("a refusal, not an error");
        assert!(matches!(
            refused,
            Written::Refused(Refusal::FileInvalid(SettingsError::InvalidUtf8 { .. }))
        ));
    }

    /// Writing a key twice leaves one entry, so the first value
    /// doesn't read as a revert of the second.
    #[test]
    fn a_key_written_twice_is_not_reported_as_reverted() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "");
        write(&store, &memory_on());
        let mut off = Patch::new();
        off.set(Key::MemoryEnabled, false);
        write(&store, &off);

        hand_edit(&store, "# a note\n[memory]\nenabled = false\n");
        let (_restart, reverted) = reloaded(store.poll().expect("polls"));

        assert!(reverted.is_empty(), "{reverted:?}");
    }

    #[test]
    fn a_key_written_twice_then_reverted_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "");
        write(&store, &memory_on());
        let mut off = Patch::new();
        off.set(Key::MemoryEnabled, false);
        write(&store, &off);

        hand_edit(&store, "# gone\n");
        let (_restart, reverted) = reloaded(store.poll().expect("polls"));

        assert_eq!(reverted, [Key::MemoryEnabled]);
    }

    /// An editor writing in place shows a prefix first. Applied,
    /// its missing `history.enabled = false` would save the next dictation.
    #[test]
    fn a_save_still_landing_is_not_applied() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "[history]\nenabled = false\n");
        thread::sleep(Duration::from_millis(20));
        fs::write(&store.path, "# a note\n").unwrap();
        let path = store.path.clone();
        let editor = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            fs::write(&path, "# a note\n[history]\nenabled = false\n").unwrap();
        });

        store.poll().expect("polls");
        assert_eq!(store.effective().history.enabled, Some(false));

        editor.join().unwrap();
        store.poll().expect("polls");
        assert_eq!(store.effective().history.enabled, Some(false));
    }

    /// The live switch decides, and the frozen table goes unused.
    #[test]
    fn turning_cleanup_off_leaves_the_frozen_llm_unused_and_loadable() {
        let llm = "[dictation.cleanup_llm]\nendpoint = \"http://127.0.0.1:1/v1/chat/completions\"\nmodel = \"m\"\n";
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), &format!("[dictation]\ncleanup = true\n{llm}"));

        hand_edit(&store, "[dictation]\ncleanup = false\n");
        reloaded(store.poll().expect("polls"));

        let home_dir = PathBuf::from(HOME);
        crate::config::DictationConfig::load(&store.effective(), &home_dir)
            .expect("the live switch and the frozen table load together");
    }

    /// The app learns of a hand edit from this: it reads the values again
    /// when the status moves.
    #[test]
    fn a_hand_edit_of_a_live_value_moves_the_status_and_a_comment_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "[memory]\nenabled = false\n");
        let mut status = store.status();
        let before = status.borrow_and_update().revision;

        hand_edit(&store, "[memory]\nenabled = true\n");
        store.poll().unwrap();

        assert!(status.has_changed().expect("alive"));
        let after = status.borrow_and_update().revision;
        assert_ne!(after, before);

        hand_edit(&store, "# a note\n[memory]\nenabled = true\n");
        store.poll().unwrap();

        assert!(!status.has_changed().expect("alive"));
    }

    #[test]
    fn an_invalid_edit_keeps_the_last_good_values_until_it_is_fixed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = store(dir.path(), "[memory]\nenabled = true\n");

        hand_edit(&store, "[memory]\nenabled = true\nenabld = false\n");
        let reload = store.poll().expect("polls");

        assert!(matches!(reload, Reload::Invalid(_)), "{reload:?}");
        assert_eq!(store.snapshot().settings.memory.enabled, Some(true));
        let mut status = store.status();
        let fault = status.borrow_and_update().fault.clone().expect("a fault");
        assert!(fault.message.contains("enabld"), "{}", fault.message);
        assert_eq!(fault.position, Some(Position { line: 3, column: 1 }));

        hand_edit(&store, "[memory]\nenabled = false\n");
        reloaded(store.poll().expect("polls"));
        assert!(status.has_changed().expect("alive"));
        assert_eq!(status.borrow_and_update().fault, None);
        assert_eq!(store.snapshot().settings.memory.enabled, Some(false));
    }

    #[test]
    fn a_restart_key_is_diffed_against_the_startup_snapshot() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = store(dir.path(), "[phone]\nport = 9850\n");

        hand_edit(&store, "[phone]\nport = 9999\n[tts]\nrate = 1.5\n");
        let (restart_required, _reverted) = reloaded(store.poll().expect("polls"));
        assert_eq!(restart_required, [Key::PhonePort]);
        assert_eq!(store.restart_required(), ["phone.port"]);

        hand_edit(&store, "[phone]\nport = 9850\n");
        let (restart_required, _reverted) = reloaded(store.poll().expect("polls"));
        assert!(
            restart_required.is_empty(),
            "back to the startup value needs no restart"
        );
    }

    #[test]
    fn a_stores_own_write_is_not_reloaded_and_a_revert_of_it_is_named() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = store(dir.path(), "[memory]\nenabled = false\n");
        let mut patch = Patch::new();
        patch.set(Key::MemoryEnabled, true);

        assert!(matches!(write(&store, &patch), Written::Applied(_)));
        assert!(matches!(store.poll().expect("polls"), Reload::Unchanged));
        assert_eq!(store.snapshot().settings.memory.enabled, Some(true));

        hand_edit(&store, "[memory]\nenabled = false\n");
        let (_restart_required, reverted) = reloaded(store.poll().expect("polls"));
        assert_eq!(reverted, [Key::MemoryEnabled]);
        assert_eq!(store.status().borrow().reverted, ["memory.enabled"]);

        hand_edit(&store, "[memory]\nenabled = true\n");
        let (_restart_required, reverted) = reloaded(store.poll().expect("polls"));
        assert!(reverted.is_empty(), "a revert is reported once");
        assert!(store.status().borrow().reverted.is_empty());
    }

    #[test]
    fn dropping_the_poll_joins_its_thread_at_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Arc::new(store(dir.path(), ""));
        let poll = SettingsPoll::spawn(Arc::clone(&store)).expect("spawns");

        let dropped_at = Instant::now();
        drop(poll);

        assert!(dropped_at.elapsed() < POLL_INTERVAL);
        assert_eq!(
            Arc::strong_count(&store),
            1,
            "the thread let go of the store"
        );
    }

    #[test]
    fn a_panic_under_the_state_lock_does_not_stop_later_reads() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = store(dir.path(), "[memory]\nenabled = true\n");

        thread::scope(|scope| {
            let holder = scope.spawn(|| {
                let _state = store.lock_state();
                panic!("poisons the lock");
            });
            assert!(holder.join().is_err());
        });

        assert_eq!(store.effective().memory.enabled, Some(true));
    }
}
