//! Swaps dictation's speech model when the settings name another, or the
//! server's pin changes, without a restart. The swap waits for any
//! recording to finish. The old model drops before the new one loads, so
//! memory never holds both.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use aria_primitives::error_chain;
use aria_settings::SettingsCache;
use aria_voice::config::TranscriptionBackend;
use aria_voice::microphone::Gate;
use aria_voice::transcription::TranscriptionError;
use tokio_util::sync::CancellationToken;

use super::super::inference::identity::PinnedServer;
use super::super::inference::pinned_server;
use super::super::tasks::Supervised;
use super::assembly::{BuildEngine, DictationEngine, EngineParts, EngineSlot};

/// Pairing writes the pin without touching the settings, so nothing else
/// says it changed.
const PIN_POLL: Duration = Duration::from_secs(1);

pub(in crate::daemon) struct EngineFollower {
    slot: EngineSlot,
    build: BuildEngine,
    microphone: Arc<Gate>,
    cache: Arc<SettingsCache>,
    home: PathBuf,
    /// The pin the server engine was built for. The engine itself can't
    /// say: an unusable one builds later, from whatever pin stands then.
    settled_pin: Mutex<Option<PinnedServer>>,
}

impl Supervised for EngineFollower {
    async fn run(self, shutdown: CancellationToken) {
        let mut status = self.cache.status();
        let mut pin_poll = tokio::time::interval(PIN_POLL);
        pin_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let follower = Arc::new(self);
        // The first pass runs before any wait, since a change made before
        // this task started has no event of its own.
        loop {
            // A newer settings change stops a swap or preload in progress.
            let watching = status.clone();
            let shutting_down = shutdown.clone();
            let stop = CancelToken::from_fn(move || {
                shutting_down.is_cancelled() || watching.has_changed().unwrap_or(true)
            });
            let following = Arc::clone(&follower);
            if tokio::task::spawn_blocking(move || following.follow(&stop))
                .await
                .is_err()
            {
                tracing::error!("the speech model swap panicked; the current model stays");
            }
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    changed = status.changed() => {
                        if changed.is_err() { return }
                        break;
                    }
                    _ = pin_poll.tick() => {
                        let following = Arc::clone(&follower);
                        let moved = tokio::task::spawn_blocking(move || following.pin_moved());
                        if moved.await.unwrap_or(false) { break }
                    }
                }
            }
        }
    }
}

impl EngineFollower {
    pub(super) fn new(
        slot: EngineSlot,
        parts: EngineParts,
        microphone: Arc<Gate>,
        cache: Arc<SettingsCache>,
        home: PathBuf,
    ) -> Self {
        let build: BuildEngine = Arc::new(move |backend| parts.engine(backend));
        let settled_pin = Mutex::new(pinned_server(&home));
        Self {
            slot,
            build,
            microphone,
            cache,
            home,
            settled_pin,
        }
    }

    fn follow(&self, stop: &CancelToken) {
        let target = TranscriptionBackend::load(&self.cache.effective(), &self.home);
        let serves = target == TranscriptionBackend::Server;
        let pin = serves.then(|| pinned_server(&self.home)).flatten();
        let server_moved = serves && pin != *self.settled_pin();
        if settle(
            &self.slot,
            &self.microphone,
            target,
            server_moved,
            &self.build,
            stop,
        ) == Settled::Swapped
        {
            *self.settled_pin() = pin;
        }
    }

    /// Whether the server engine now runs on a pin that no longer stands.
    fn pin_moved(&self) -> bool {
        self.slot.current().backend == TranscriptionBackend::Server
            && pinned_server(&self.home) != *self.settled_pin()
    }

    fn settled_pin(&self) -> std::sync::MutexGuard<'_, Option<PinnedServer>> {
        self.settled_pin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Settled {
    Unchanged,
    Swapped,
    Stopped,
}

/// The preload runs after the swap, so a load that fails shows up on the
/// next press. `server_moved` rebuilds a server engine whose pin was
/// replaced or forgotten, which the backend alone can't show.
fn settle(
    slot: &EngineSlot,
    microphone: &Arc<Gate>,
    target: TranscriptionBackend,
    server_moved: bool,
    build: &BuildEngine,
    stop: &CancelToken,
) -> Settled {
    let current = slot.current();
    if current.backend == target && !server_moved {
        preload(&current, stop);
        return Settled::Unchanged;
    }
    let previous = current.name;
    // Holding it past the swap would keep the old model in memory.
    drop(current);
    let engine = match build(&target) {
        Ok(engine) => engine,
        Err(error) => {
            tracing::warn!(
                engine = target.backend_name(),
                error = %error_chain(&error),
                "the chosen speech model can't be set up; dictation reports why on each press"
            );
            DictationEngine::unusable(target, Arc::clone(build))
        }
    };
    let name = engine.name;
    let Some(claim) = microphone.admit_engine_swap(stop) else {
        return Settled::Stopped;
    };
    slot.replace(engine, &claim);
    drop(claim);
    tracing::info!(engine = name, previous, "speech model switched");
    preload(&slot.current(), stop);
    Settled::Swapped
}

/// A stopped preload runs again on the next pass, which every settings change
/// starts.
fn preload(engine: &DictationEngine, stop: &CancelToken) {
    match engine.preload(stop) {
        Ok(()) | Err(TranscriptionError::Cancelled) => {}
        Err(error) => tracing::warn!(
            engine = engine.name,
            error = %error_chain(&error),
            "the speech model didn't preload; the next dictation loads it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use aria_kit::audio::RecordingReader;
    use aria_settings::Settings;
    use aria_voice::config::TranscriptionConfig;
    use aria_voice::dictation::{
        DictationCoordinator, DictationError, DictationEventEmitter, DictationExecutor,
        DictationOutcome, DictationPolicy, DictationRequest, DictationResult, DictationSettings,
        ExecutionControl, Rewriting, StopMode, Transcribed,
    };
    use aria_voice::transcription::{Language, TranscriptionError};

    use super::super::assembly::RuntimeExecutor;
    use super::super::model_load::Preload;
    use super::*;

    struct Named {
        name: &'static str,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for Named {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl DictationExecutor for Named {
        fn execute(
            &self,
            _: DictationSettings,
            _: &Arc<ExecutionControl>,
            _: &DictationEventEmitter,
        ) -> Result<DictationResult, DictationError> {
            Ok(DictationResult {
                text: self.name.to_owned(),
                own_voice: false,
            })
        }

        fn transcribe(
            &self,
            _: &mut RecordingReader,
            _: &Language,
            _: bool,
            _: Option<&aria_settings::Directory>,
            _: Rewriting,
            _: &CancelToken,
        ) -> Result<Transcribed, DictationError> {
            panic!("these tests only press");
        }

        fn deliver_recording(
            &self,
            _: &mut RecordingReader,
            _: DictationSettings,
            _: &Arc<ExecutionControl>,
            _: &DictationEventEmitter,
        ) -> Result<String, DictationError> {
            panic!("these tests only press");
        }
    }

    fn named(name: &'static str) -> (RuntimeExecutor, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let executor = Box::new(Named {
            name,
            dropped: Arc::clone(&dropped),
        });
        (executor, dropped)
    }

    fn local(directory: &str) -> TranscriptionBackend {
        TranscriptionBackend::Local(TranscriptionConfig::Moonshine {
            model_directory: Path::new(directory).to_path_buf(),
        })
    }

    fn server() -> TranscriptionBackend {
        TranscriptionBackend::Server
    }

    fn settings() -> DictationSettings {
        DictationSettings::resolve(
            Settings::default(),
            DictationRequest::new(StopMode::OnSilence),
            None,
        )
    }

    fn press(slot: &EngineSlot) -> Result<String, DictationError> {
        slot.execute(
            settings(),
            &ExecutionControl::for_test(StopMode::OnSilence),
            &DictationEventEmitter::unsubscribed(),
        )
        .map(|result| result.text)
    }

    fn building(
        name: &'static str,
        old_dropped: Arc<AtomicBool>,
    ) -> (BuildEngine, Arc<AtomicBool>) {
        let preloaded_after_drop = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&preloaded_after_drop);
        let build: BuildEngine = Arc::new(move |backend: &TranscriptionBackend| {
            let (executor, _) = named(name);
            let old_dropped = Arc::clone(&old_dropped);
            let seen = Arc::clone(&seen);
            let preload: Preload = Arc::new(move |_| {
                seen.store(old_dropped.load(Ordering::SeqCst), Ordering::SeqCst);
                Ok(())
            });
            Ok(DictationEngine::built(name, backend.clone(), executor).with_preload(preload))
        });
        (build, preloaded_after_drop)
    }

    async fn until_pressed(slot: &EngineSlot, name: &str) {
        for _ in 0..500 {
            if press(slot).is_ok_and(|heard| heard == name) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("dictation never moved to {name}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_follower_swaps_before_any_change_and_again_on_each_one() {
        let home = tempfile::tempdir().unwrap();
        let path = aria_settings::default_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[stt]\nbackend = \"whisper-candle\"\n").unwrap();
        let cache = Arc::new(SettingsCache::open(path, home.path().to_path_buf()).unwrap());
        let (old, _) = named("old");
        let slot = EngineSlot::of("old", old);
        let build: BuildEngine = Arc::new(|backend: &TranscriptionBackend| {
            let (executor, _) = named(backend.backend_name());
            Ok(DictationEngine::built(
                backend.backend_name(),
                backend.clone(),
                executor,
            ))
        });
        let follower = EngineFollower {
            slot: slot.clone(),
            build,
            microphone: Gate::new(),
            cache: Arc::clone(&cache),
            home: home.path().to_path_buf(),
            settled_pin: Mutex::new(None),
        };
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(follower.run(shutdown.clone()));

        until_pressed(&slot, "whisper-candle").await;
        cache
            .modify(None, |_| {
                let mut patch = aria_settings::Patch::new();
                patch.set(aria_settings::Key::SttBackend, "parakeet");
                Ok::<_, aria_settings::SettingsError>(patch)
            })
            .unwrap();
        until_pressed(&slot, "parakeet").await;

        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_pairing_rebuilds_the_server_engine_without_a_settings_change() {
        let home = tempfile::tempdir().unwrap();
        let path = aria_settings::default_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[stt]\nruns_on = \"server\"\n").unwrap();
        crate::daemon::inference::pin_server_for_tests(home.path(), "https://a.test:9851");
        let cache = Arc::new(SettingsCache::open(path, home.path().to_path_buf()).unwrap());
        let (old, _) = named("local");
        let slot = EngineSlot::of("local", old);
        let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&builds);
        let build: BuildEngine = Arc::new(move |backend: &TranscriptionBackend| {
            let name = ["first pin", "second pin"][counted.fetch_add(1, Ordering::SeqCst).min(1)];
            let (executor, _) = named(name);
            Ok(DictationEngine::built(name, backend.clone(), executor))
        });
        let follower = EngineFollower {
            slot: slot.clone(),
            build,
            microphone: Gate::new(),
            cache,
            home: home.path().to_path_buf(),
            settled_pin: Mutex::new(None),
        };
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(follower.run(shutdown.clone()));

        until_pressed(&slot, "first pin").await;
        crate::daemon::inference::pin_server_for_tests(home.path(), "https://b.test:9851");
        until_pressed(&slot, "second pin").await;

        shutdown.cancel();
        task.await.unwrap();
    }

    #[test]
    fn a_swap_waits_for_the_recording_and_drops_the_old_model_before_loading() {
        let (old, old_dropped) = named("old");
        let slot = EngineSlot::of("old", old);
        let (build, preloaded_after_drop) = building("new", Arc::clone(&old_dropped));
        let microphone = Gate::new();
        let recording = microphone.admit_enrollment().unwrap();

        thread::scope(|scope| {
            let settling = scope.spawn(|| {
                settle(
                    &slot,
                    &microphone,
                    local("/new"),
                    false,
                    &build,
                    &CancelToken::never(),
                )
            });
            thread::sleep(Duration::from_millis(50));
            assert!(!settling.is_finished(), "the swap waits for the recording");
            assert_eq!(press(&slot).unwrap(), "old");
            drop(recording);
            assert_eq!(settling.join().unwrap(), Settled::Swapped);
        });

        assert!(preloaded_after_drop.load(Ordering::SeqCst));
        assert_eq!(press(&slot).unwrap(), "new");
    }

    #[test]
    fn a_stopped_swap_keeps_the_current_engine() {
        let (old, old_dropped) = named("old");
        let slot = EngineSlot::of("old", old);
        let (build, _) = building("new", Arc::clone(&old_dropped));
        let microphone = Gate::new();
        let _recording = microphone.admit_enrollment().unwrap();
        let stop = CancelToken::new();
        stop.cancel();

        let settled = settle(&slot, &microphone, local("/new"), false, &build, &stop);

        assert_eq!(settled, Settled::Stopped);
        assert!(!old_dropped.load(Ordering::SeqCst));
        assert_eq!(press(&slot).unwrap(), "old");
    }

    #[test]
    fn a_server_turned_off_stops_receiving_audio_even_when_the_new_model_fails() {
        let (remote, remote_dropped) = named("server");
        let slot = EngineSlot::new(DictationEngine::built("server", server(), remote));
        let failing: BuildEngine = Arc::new(|_: &TranscriptionBackend| {
            Err(DictationError::Transcription(
                TranscriptionError::NoNativeStream,
            ))
        });

        let settled = settle(
            &slot,
            &Gate::new(),
            local("/missing"),
            false,
            &failing,
            &CancelToken::never(),
        );

        assert_eq!(settled, Settled::Swapped);
        assert!(remote_dropped.load(Ordering::SeqCst));
        assert!(matches!(
            press(&slot),
            Err(DictationError::Transcription(_))
        ));
        assert_eq!(slot.current().backend, local("/missing"));
    }

    #[test]
    fn a_server_engine_is_rebuilt_when_its_pin_moves() {
        let (remote, remote_dropped) = named("old pin");
        let slot = EngineSlot::new(DictationEngine::built("old pin", server(), remote));
        let (build, _) = building("new pin", Arc::clone(&remote_dropped));

        let kept = settle(
            &slot,
            &Gate::new(),
            server(),
            false,
            &build,
            &CancelToken::never(),
        );
        assert_eq!(kept, Settled::Unchanged);

        let moved = settle(
            &slot,
            &Gate::new(),
            server(),
            true,
            &build,
            &CancelToken::never(),
        );

        assert_eq!(moved, Settled::Swapped);
        assert!(remote_dropped.load(Ordering::SeqCst));
        assert_eq!(press(&slot).unwrap(), "new pin");
    }

    #[test]
    fn a_pass_with_no_new_engine_runs_the_current_preload_again() {
        let (executor, _) = named("current");
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let preload: Preload = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let slot = EngineSlot::new(
            DictationEngine::built("current", local("/current"), executor).with_preload(preload),
        );
        let (build, _) = building("unused", Arc::new(AtomicBool::new(false)));

        let settled = settle(
            &slot,
            &Gate::new(),
            local("/current"),
            false,
            &build,
            &CancelToken::never(),
        );

        assert_eq!(settled, Settled::Unchanged);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_unusable_engine_keeps_the_first_build_that_succeeds() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let build: BuildEngine = Arc::new(move |backend: &TranscriptionBackend| {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(DictationError::Transcription(
                    TranscriptionError::NoNativeStream,
                ));
            }
            let (executor, _) = named("recovered");
            Ok(DictationEngine::built(
                "recovered",
                backend.clone(),
                executor,
            ))
        });
        let slot = EngineSlot::new(DictationEngine::unusable(local("/new"), build));

        assert!(press(&slot).is_err());
        assert_eq!(press(&slot).unwrap(), "recovered");
        assert_eq!(press(&slot).unwrap(), "recovered");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    /// The coordinator outlives the swap, so its recording IDs keep counting.
    #[test]
    fn the_same_coordinator_runs_the_new_engine_after_a_swap() {
        let (old, _) = named("old");
        let (new, _) = named("new");
        let slot = EngineSlot::of("old", old);
        let microphone = Gate::new();
        let execution: RuntimeExecutor = Box::new(slot.clone());
        let coordinator = DictationCoordinator::new(
            DictationPolicy::new(Duration::from_secs(5)).unwrap(),
            execution,
            Arc::clone(&microphone),
        );
        let dictate = || {
            let outcome = coordinator
                .execute(
                    settings(),
                    CancelToken::never(),
                    &DictationEventEmitter::unsubscribed(),
                )
                .unwrap();
            let DictationOutcome::Completed(result) = outcome else {
                panic!("an automatic press completes");
            };
            result.text
        };

        assert_eq!(dictate(), "old");
        let claim = microphone.admit_engine_swap(&CancelToken::never()).unwrap();
        slot.replace(DictationEngine::built("new", local("/new"), new), &claim);
        drop(claim);
        assert_eq!(dictate(), "new");
    }
}
