//! The Aria key as a trigger, owned by a persistent daemon. The listener
//! turns keys into presses and bare keys; the key router decides what
//! each one means.

mod router;

use super::chat::ChatHandle;
use super::dictation::{DictationHandle, DictationRuntime};
use super::subscriptions::Subscriptions;
use super::tasks::Supervised;
use crate::ipc::HOTKEY_HEALTH_SERVICE;
use aria_platform::hotkey::{
    AriaKey, BareKey, HotkeyState, ListenerEvent, PlatformHotkeyListener, Taken,
};
use router::{Hold, KeyRouter};
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

/// The blocking platform listener; tests swap in a fake.
type Listener = fn(AriaKey, &dyn Fn(BareKey) -> Taken, &dyn Fn(ListenerEvent), &dyn Fn() -> bool);

pub(super) struct HotkeyTrigger {
    key: watch::Receiver<AriaKey>,
    /// Shared with the listener thread, which asks it about each bare key.
    router: Arc<KeyRouter>,
    health: HealthReporter,
    /// None until a listener reports, and after one fails.
    state: watch::Sender<Option<HotkeyState>>,
    listener: Listener,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HotkeyExit {
    Shutdown,
    Failed,
    /// The Aria key setting changed; a fresh listener takes over.
    Rearm,
}

impl Supervised for HotkeyTrigger {
    async fn run(self, shutdown: CancellationToken) {
        let exit = self.listen(shutdown).await;
        if exit == HotkeyExit::Failed {
            self.publish(None).await;
        }
    }
}

impl HotkeyTrigger {
    pub(super) fn new(
        key: watch::Receiver<AriaKey>,
        dictation: DictationHandle,
        chat: ChatHandle,
        runtime: Arc<DictationRuntime>,
        subscriptions: Arc<Subscriptions>,
        health: HealthReporter,
        state: watch::Sender<Option<HotkeyState>>,
    ) -> Self {
        Self {
            key,
            router: Arc::new(KeyRouter::new(dictation, chat, runtime, subscriptions)),
            health,
            state,
            listener: |key, take, events, should_stop| {
                PlatformHotkeyListener::listen(key, take, events, should_stop);
            },
        }
    }

    /// Register before the daemon serves so health checks always find the hotkey.
    /// Permission is unknown until the listener reports Armed.
    pub(super) async fn announce(&self) {
        self.health
            .set_service_status(HOTKEY_HEALTH_SERVICE, ServingStatus::NotServing)
            .await;
    }

    /// A change to the Aria key setting re-arms the listener: the running one
    /// is told to stop and a fresh one starts on the new key.
    async fn listen(&self, shutdown: CancellationToken) -> HotkeyExit {
        let mut key = self.key.clone();
        loop {
            match self.listen_once(&mut key, &shutdown).await {
                HotkeyExit::Rearm => {
                    tracing::info!(trigger = "hotkey", "hotkey key changed; re-arming");
                }
                exit => return exit,
            }
        }
    }

    async fn listen_once(
        &self,
        key: &mut watch::Receiver<AriaKey>,
        shutdown: &CancellationToken,
    ) -> HotkeyExit {
        let current = *key.borrow_and_update();
        // Bounded, and fed with try_send: the listener thread must never
        // block, and a full channel just means a press was already queued.
        let (events, mut incoming) = tokio::sync::mpsc::channel::<ListenerEvent>(8);
        let stop = shutdown.clone();
        // A clone shares the seen version, so has_changed flips only on a
        // change made after this listener started.
        let rearm = key.clone();
        let listener = self.listener;
        let router = Arc::clone(&self.router);
        let mut listener = tokio::task::spawn_blocking(move || {
            let take = |key| router.take(key);
            let events = |event| {
                if events.try_send(event).is_err() {
                    tracing::warn!(trigger = "hotkey", ?event, "hotkey event dropped");
                }
            };
            let should_stop = || stop.is_cancelled() || rearm.has_changed().unwrap_or(true);
            listener(current, &take, &events, &should_stop);
        });

        // Dropped with this listener, which cancels a hold it can no longer
        // end.
        let mut hold: Option<Hold> = None;
        loop {
            tokio::select! {
                // A hold's end queued in the listener's last slice must not
                // lose to the listener's exit, or the hold is discarded.
                biased;
                Some(event) = incoming.recv() => match event {
                    ListenerEvent::Press(press) => self.router.press(press, &mut hold),
                    event => self.report(event).await,
                },
                result = &mut listener => return self.classify(listener_exit(result), key, shutdown),
                () = shutdown.cancelled() => return listener_exit(listener.await),
            }
        }
    }

    /// The listener retries every few seconds, so only a change of state is
    /// logged.
    async fn report(&self, event: ListenerEvent) {
        if !self.publish(event.state()).await {
            return;
        }
        match event {
            ListenerEvent::AwaitingPermission => tracing::info!(
                trigger = "hotkey",
                "the hotkey is waiting for permission; run `aria doctor` for steps"
            ),
            ListenerEvent::TapRefused(refusal) => tracing::warn!(
                trigger = "hotkey",
                step = refusal.step(),
                listen_access = refusal.listen_access(),
                post_access = refusal.post_access(),
                "macOS refused the hotkey's event taps though Accessibility is on"
            ),
            ListenerEvent::KeyUnavailable => tracing::warn!(
                trigger = "hotkey",
                "the hotkey's key is unavailable; run `aria doctor` for steps"
            ),
            ListenerEvent::Armed | ListenerEvent::Press(_) => {}
        }
    }

    /// Returns false when the state did not change.
    async fn publish(&self, state: Option<HotkeyState>) -> bool {
        if !self
            .state
            .send_if_modified(|current| std::mem::replace(current, state) != state)
        {
            return false;
        }
        let health = if state == Some(HotkeyState::Armed) {
            ServingStatus::Serving
        } else {
            ServingStatus::NotServing
        };
        self.health
            .set_service_status(HOTKEY_HEALTH_SERVICE, health)
            .await;
        true
    }

    /// A listener that stopped on its own did so for the key watch: a new
    /// key means re-arm, a closed watch means the store is gone.
    fn classify(
        &self,
        exit: HotkeyExit,
        key: &watch::Receiver<AriaKey>,
        shutdown: &CancellationToken,
    ) -> HotkeyExit {
        if exit != HotkeyExit::Shutdown || shutdown.is_cancelled() {
            return exit;
        }
        match key.has_changed() {
            Ok(true) => HotkeyExit::Rearm,
            Ok(false) => exit,
            Err(_) => {
                tracing::error!("hotkey listener lost the settings store");
                HotkeyExit::Failed
            }
        }
    }
}

fn listener_exit(result: Result<(), tokio::task::JoinError>) -> HotkeyExit {
    match result {
        Ok(()) => HotkeyExit::Shutdown,
        Err(error) => {
            tracing::error!(%error, "hotkey listener ended unexpectedly");
            HotkeyExit::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio_stream::StreamExt;
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_server::Health;
    use tonic_health::server::HealthService;

    use crate::ipc::wire;
    use aria_platform::hotkey::{Combo, Press, TapRefusal};
    use aria_platform::runtime::RuntimePaths;
    use aria_settings::SettingsStore;
    use aria_voice::dictation::testing::{RecordsUntilStopped, recording_started};

    use super::super::chat::{ChatRuntime, channel as chat_channel};
    use super::super::context::Context;
    use super::super::dictation::channel as dictation_channel;
    use super::super::host::DaemonLifetime;
    use super::super::log::testing::CapturedLogs;
    use super::*;

    /// Keys each fake listener was armed on; a fn pointer cannot capture.
    static ARMED: Mutex<Vec<AriaKey>> = Mutex::new(Vec::new());

    /// Holds the listener thread open, as the platform listener does, until
    /// the trigger asks it to stop.
    fn park_until(should_stop: &dyn Fn() -> bool) {
        while !should_stop() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A trigger with no dictation behind it. The directory holds its
    /// settings and must outlive it.
    fn trigger(
        key: watch::Receiver<AriaKey>,
        subscriptions: Arc<Subscriptions>,
        listener: Listener,
        directory: &tempfile::TempDir,
    ) -> HotkeyTrigger {
        let (health, _server) = tonic_health::server::health_reporter();
        HotkeyTrigger {
            key,
            router: Arc::new(KeyRouter::new(
                DictationHandle::detached(),
                ChatHandle::detached(),
                Arc::new(DictationRuntime::detached(Arc::new(
                    SettingsStore::open(directory.path().join("settings.toml"))
                        .expect("the settings store should open"),
                ))),
                subscriptions,
            )),
            health,
            state: watch::channel(None).0,
            listener,
        }
    }

    async fn health_of(service: &HealthService, name: &str) -> ServingStatus {
        service
            .check(tonic::Request::new(HealthCheckRequest {
                service: name.to_owned(),
            }))
            .await
            .expect("the health check should answer")
            .into_inner()
            .status()
    }

    async fn armed_count_reaches(count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while ARMED.lock().expect("armed keys").len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the listener should arm in time");
    }

    #[tokio::test]
    async fn a_key_change_rearms_on_the_new_key() {
        let (key, watch) = watch::channel(AriaKey::RightOption);
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let trigger = trigger(
            watch,
            Arc::new(Subscriptions::new()),
            |key, _take, _events, should_stop| {
                ARMED.lock().expect("armed keys").push(key);
                park_until(should_stop);
            },
            &directory,
        );
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(trigger.run(shutdown.clone()));

        armed_count_reaches(1).await;
        key.send(AriaKey::Fn).expect("the trigger is listening");
        armed_count_reaches(2).await;
        shutdown.cancel();
        running.await.expect("the trigger should stop cleanly");

        assert_eq!(
            *ARMED.lock().expect("armed keys"),
            [AriaKey::RightOption, AriaKey::Fn]
        );
    }

    #[tokio::test]
    async fn a_combo_reaches_subscribers() {
        let (_key, watch) = watch::channel(AriaKey::RightOption);
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let subscriptions = Arc::new(Subscriptions::new());
        let mut events = subscriptions.subscribe().expect("a subscriber fits");
        let trigger = trigger(
            watch,
            subscriptions,
            |_key, _take, events, should_stop| {
                events(ListenerEvent::Press(Press::Combo(Combo::Shift)));
                park_until(should_stop);
            },
            &directory,
        );
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(trigger.run(shutdown.clone()));

        let event = tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .expect("the combo should arrive in time")
            .expect("the stream stays open")
            .expect("the event is not an error");
        shutdown.cancel();
        running.await.expect("the trigger should stop cleanly");

        assert_eq!(
            event.payload,
            Some(wire::event::Payload::Hotkey(wire::HotkeyEvent {
                combo: wire::ShortcutCombo::Shift.into(),
            }))
        );
    }

    /// A live chat actor with no job answers the tap with nothing to
    /// cancel, so the tap reaches dictation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tap_with_nothing_in_flight_records() {
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let paths = RuntimePaths::in_directory(directory.path().to_path_buf())
            .prepare()
            .expect("the runtime paths should prepare");
        let context = Context::detect(&paths, DaemonLifetime::OnDemand).expect("a test context");
        let subscriptions = Arc::new(Subscriptions::new());
        let settings = Arc::new(
            SettingsStore::open(directory.path().join("settings.toml"))
                .expect("the settings store should open"),
        );
        let chat = ChatRuntime::new(
            &context,
            Arc::clone(&subscriptions),
            Arc::clone(&settings),
            Arc::new(
                aria_settings::SettingsCache::open(
                    directory.path().join("chat-settings.toml"),
                    directory.path().to_path_buf(),
                )
                .expect("the settings store should open"),
            ),
            None,
            Arc::new(crate::daemon::history::HistoryStore::new()),
            None,
        );
        let (chat, chat_actor) = chat_channel(Arc::new(chat));
        let mut dictation = DictationRuntime::detached(settings);
        let (executor, edges) = RecordsUntilStopped::new();
        dictation.execute_with(Box::new(executor));
        let dictation = Arc::new(dictation);
        let (handle, dictation_actor) = dictation_channel(Arc::clone(&dictation));
        let (_key, watch) = watch::channel(AriaKey::RightOption);
        let (health, _server) = tonic_health::server::health_reporter();
        let trigger = HotkeyTrigger {
            key: watch,
            router: Arc::new(KeyRouter::new(handle, chat, dictation, subscriptions)),
            health,
            state: watch::channel(None).0,
            listener: |_key, _take, events, should_stop| {
                events(ListenerEvent::Press(Press::Tap));
                park_until(should_stop);
            },
        };
        let shutdown = CancellationToken::new();
        let chat_actor = tokio::spawn(chat_actor.run(shutdown.clone()));
        let dictation_actor = tokio::spawn(dictation_actor.run(shutdown.clone()));
        let running = tokio::spawn(trigger.run(shutdown.clone()));

        tokio::task::spawn_blocking(move || recording_started(&edges))
            .await
            .expect("the wait does not panic");

        shutdown.cancel();
        running.await.expect("the trigger should stop cleanly");
        dictation_actor.await.expect("the dictation actor drains");
        chat_actor.await.expect("the chat actor drains");
    }

    #[tokio::test]
    async fn an_armed_listener_serves_hotkey_health() {
        let (_key, watch) = watch::channel(AriaKey::RightOption);
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let subscriptions = Arc::new(Subscriptions::new());
        let mut events = subscriptions.subscribe().expect("a subscriber fits");
        let mut trigger = trigger(
            watch,
            subscriptions,
            |_key, _take, events, should_stop| {
                events(ListenerEvent::Armed);
                // Queued behind Armed, so its arrival means Armed was handled.
                events(ListenerEvent::Press(Press::Combo(Combo::Shift)));
                park_until(should_stop);
            },
            &directory,
        );
        let (reporter, _server) = tonic_health::server::health_reporter();
        trigger.health = reporter.clone();
        let service = HealthService::from_health_reporter(reporter);
        let state = trigger.state.subscribe();
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(trigger.run(shutdown.clone()));

        tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .expect("the combo should arrive in time");
        let health = health_of(&service, HOTKEY_HEALTH_SERVICE).await;
        shutdown.cancel();
        running.await.expect("the trigger should stop cleanly");

        assert_eq!(*state.borrow(), Some(HotkeyState::Armed));
        assert_eq!(health, ServingStatus::Serving);
    }

    #[tokio::test]
    async fn a_failed_listener_degrades_only_hotkey_health() {
        let (_key, watch) = watch::channel(AriaKey::RightOption);
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let mut trigger = trigger(
            watch,
            Arc::new(Subscriptions::new()),
            |_key, _take, events, _should_stop| {
                events(ListenerEvent::Armed);
                panic!("the listener fails after arming");
            },
            &directory,
        );
        let (reporter, _server) = tonic_health::server::health_reporter();
        trigger.health = reporter.clone();
        let service = HealthService::from_health_reporter(reporter);
        let state = trigger.state.subscribe();

        trigger.run(CancellationToken::new()).await;

        assert_eq!(*state.borrow(), None);
        assert_eq!(
            health_of(&service, HOTKEY_HEALTH_SERVICE).await,
            ServingStatus::NotServing
        );
        assert_eq!(health_of(&service, "").await, ServingStatus::Serving);
    }

    #[tokio::test]
    async fn a_refused_tap_degrades_hotkey_health_and_logs_once() {
        let logs = CapturedLogs::start();
        let (_key, watch) = watch::channel(AriaKey::RightOption);
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let subscriptions = Arc::new(Subscriptions::new());
        let mut events = subscriptions.subscribe().expect("a subscriber fits");
        let mut trigger = trigger(
            watch,
            subscriptions,
            |_key, _take, events, should_stop| {
                events(ListenerEvent::TapRefused(TapRefusal::for_test(false, true)));
                events(ListenerEvent::TapRefused(TapRefusal::for_test(
                    false, false,
                )));
                events(ListenerEvent::TapRefused(TapRefusal::for_test(true, true)));
                // Queued behind the refusals, so its arrival means all were
                // handled.
                events(ListenerEvent::Press(Press::Combo(Combo::Shift)));
                park_until(should_stop);
            },
            &directory,
        );
        let (reporter, _server) = tonic_health::server::health_reporter();
        trigger.health = reporter.clone();
        let service = HealthService::from_health_reporter(reporter);
        let state = trigger.state.subscribe();
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(trigger.run(shutdown.clone()));

        tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .expect("the combo should arrive in time");
        let health = health_of(&service, HOTKEY_HEALTH_SERVICE).await;
        shutdown.cancel();
        running.await.expect("the trigger should stop cleanly");

        assert_eq!(*state.borrow(), Some(HotkeyState::Refused));
        assert_eq!(health, ServingStatus::NotServing);
        let log = logs.text();
        let refusals: Vec<_> = log
            .lines()
            .filter(|line| line.contains("refused the hotkey's event taps"))
            .collect();
        assert_eq!(refusals.len(), 2, "{log}");
        assert!(refusals[0].contains("listen_access=false"), "{log}");
        assert!(refusals[1].contains("listen_access=true"), "{log}");
    }
}
