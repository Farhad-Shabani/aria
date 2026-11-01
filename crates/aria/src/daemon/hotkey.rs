//! The Aria key as a trigger, owned by a persistent daemon. A tap toggles
//! dictation, or cancels a run a client's press started; a hold records
//! here; a combo is broadcast for a client to run.

use std::sync::Arc;

use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

use crate::dictation::{
    DictationError, DictationOutcome, DictationRequest, DictationStage, StopMode,
};
use crate::hotkey::{AriaKey, Combo, ListenerEvent, PlatformHotkeyListener, Press};
use crate::ipc::HOTKEY_HEALTH_SERVICE;

use super::chat::{ChatCallError, ChatHandle};
use super::dictation::DictationRuntimeError;
use super::dictation::{AutoStopRule, DictationCallError, DictationHandle, DictationRuntime};
use super::operation::CallError;
use super::subscriptions::Subscriptions;
use super::tasks::Supervised;

/// The blocking platform listener; tests swap in a fake.
type Listener = fn(AriaKey, &dyn Fn(ListenerEvent), &dyn Fn() -> bool);

pub(super) struct HotkeyTrigger {
    /// `None` while the Aria key is off: the trigger then waits for one.
    key: watch::Receiver<Option<AriaKey>>,
    dictation: DictationHandle,
    /// Cancels a run a client's press started, so a tap does not dictate
    /// over it.
    chat: ChatHandle,
    /// Stops a hold's recording by id, the same way a client's stop does.
    runtime: Arc<DictationRuntime>,
    /// Where combos go; the Mac app runs their actions.
    subscriptions: Arc<Subscriptions>,
    health: HealthReporter,
    listener: Listener,
}

/// How a hold ended, sent to the task that owns its dictation.
#[derive(Debug, Clone, Copy)]
enum HoldEnd {
    Released,
    Cancelled,
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
        let health = self.health.clone();
        let exit = self.listen(shutdown).await;
        report_exit(exit, &health).await;
    }
}

/// With the Aria key off nothing listens, so presses reach nothing. The
/// next change of the key re-arms.
async fn wait_for_a_key(
    key: &mut watch::Receiver<Option<AriaKey>>,
    shutdown: &CancellationToken,
) -> HotkeyExit {
    tracing::info!(trigger = "hotkey", "the Aria key is off");
    tokio::select! {
        () = shutdown.cancelled() => HotkeyExit::Shutdown,
        changed = key.changed() => match changed {
            Ok(()) => HotkeyExit::Rearm,
            Err(_) => {
                tracing::error!("hotkey listener lost the settings store");
                HotkeyExit::Failed
            }
        },
    }
}

impl HotkeyTrigger {
    pub(super) fn new(
        key: watch::Receiver<Option<AriaKey>>,
        dictation: DictationHandle,
        chat: ChatHandle,
        runtime: Arc<DictationRuntime>,
        subscriptions: Arc<Subscriptions>,
        health: HealthReporter,
    ) -> Self {
        Self {
            key,
            dictation,
            chat,
            runtime,
            subscriptions,
            health,
            listener: |key, events, should_stop| {
                PlatformHotkeyListener::listen(key, events, should_stop);
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
        key: &mut watch::Receiver<Option<AriaKey>>,
        shutdown: &CancellationToken,
    ) -> HotkeyExit {
        let Some(current) = *key.borrow_and_update() else {
            return wait_for_a_key(key, shutdown).await;
        };
        // Bounded, and fed with try_send: the listener thread must never
        // block, and a full channel just means a press was already queued.
        let (events, mut incoming) = tokio::sync::mpsc::channel::<ListenerEvent>(8);
        let stop = shutdown.clone();
        // A clone shares the seen version, so has_changed flips only on a
        // change made after this listener started.
        let rearm = key.clone();
        let listener = self.listener;
        let mut listener = tokio::task::spawn_blocking(move || {
            let events = |event| {
                if events.try_send(event).is_err() {
                    tracing::warn!(trigger = "hotkey", ?event, "hotkey event dropped");
                }
            };
            let should_stop = || stop.is_cancelled() || rearm.has_changed().unwrap_or(true);
            listener(current, &events, &should_stop);
        });

        let mut waiting = false;
        // Where the end of the hold in progress goes; `None` between holds.
        let mut hold: Option<oneshot::Sender<HoldEnd>> = None;
        loop {
            tokio::select! {
                // A hold's end queued in the listener's last slice must not
                // lose to the listener's exit, or the hold is discarded.
                biased;
                Some(event) = incoming.recv() => match event {
                    ListenerEvent::Armed => {
                        waiting = false;
                        self.health
                            .set_service_status(HOTKEY_HEALTH_SERVICE, ServingStatus::Serving)
                            .await;
                    }
                    ListenerEvent::AwaitingPermission => {
                        self.health
                            .set_service_status(HOTKEY_HEALTH_SERVICE, ServingStatus::NotServing)
                            .await;
                        // The listener retries every few seconds, so only a
                        // change of state is worth a line.
                        if !waiting {
                            waiting = true;
                            tracing::info!(
                                trigger = "hotkey",
                                "the hotkey is waiting for permission; run `aria doctor` for steps"
                            );
                        }
                    }
                    ListenerEvent::KeyUnavailable => {
                        waiting = false;
                        self.health
                            .set_service_status(HOTKEY_HEALTH_SERVICE, ServingStatus::NotServing)
                            .await;
                        tracing::warn!(
                            trigger = "hotkey",
                            "the hotkey's key is unavailable; run `aria doctor` for steps"
                        );
                    }
                    ListenerEvent::Press(Press::Tap) => self.tap(),
                    ListenerEvent::Press(Press::HoldStarted) => hold = Some(self.hold()),
                    ListenerEvent::Press(Press::HoldEnded) => end_hold(&mut hold, HoldEnd::Released),
                    ListenerEvent::Press(Press::HoldCancelled) => {
                        end_hold(&mut hold, HoldEnd::Cancelled);
                    }
                    ListenerEvent::Press(Press::Combo(combo)) => self.combo(combo),
                },
                result = &mut listener => return self.classify(listener_exit(result), key, shutdown),
                () = shutdown.cancelled() => return listener_exit(listener.await),
            }
        }
    }

    /// A listener that stopped on its own did so for the key watch: a new
    /// key means re-arm, a closed watch means the store is gone.
    fn classify(
        &self,
        exit: HotkeyExit,
        key: &watch::Receiver<Option<AriaKey>>,
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

    /// One detached task per tap: a starting tap holds its call open for
    /// the whole dictation, and the next tap must not wait behind it. A tap
    /// ends what the user is in the middle of: an open manual recording
    /// first, then a run a press started; with neither, it dictates. Between
    /// a press's recording stopping and its ticket, transcription plus one
    /// round trip, a tap still dictates.
    fn tap(&self) {
        tracing::info!(trigger = "hotkey", "tap");
        let runtime = Arc::clone(&self.runtime);
        let chat = self.chat.clone();
        let dictation = self.dictation.clone();
        tokio::spawn(async move {
            if runtime.stop_manual_recording() {
                tracing::info!(
                    trigger = "hotkey",
                    status = "stop_accepted",
                    "hotkey tap stopped the running recording"
                );
                return;
            }
            match chat.cancel_tapped().await {
                Ok(true) => tracing::info!(
                    trigger = "hotkey",
                    status = "cancelled",
                    "hotkey tap cancelled the run a press started"
                ),
                Ok(false) => log_outcome(
                    dictation
                        .start(DictationRequest::new(StopMode::Manual))
                        .await,
                ),
                Err(error) => log_chat_unreached(error),
            }
        });
    }

    /// With no subscriber, the press goes nowhere.
    fn combo(&self, combo: Combo) {
        tracing::info!(trigger = "hotkey", ?combo, "combo");
        self.subscriptions.broadcast(combo);
    }

    /// One detached task per hold. It owns the hold's call, so the listener
    /// loop never waits on the actor; the hold's end reaches it through the
    /// returned sender. A release stops the recording by id, so it can never
    /// be mistaken for a fresh start, and a cancel drops the call, which
    /// discards whatever was recorded.
    fn hold(&self) -> oneshot::Sender<HoldEnd> {
        tracing::info!(trigger = "hotkey", "hold started");
        let (end, ended) = oneshot::channel();
        let dictation = self.dictation.clone();
        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            // The id arrives with the Recording stage.
            let (recording, recorded) = watch::channel(None);
            let events = runtime.emitter_for_call(move |stage| {
                if let DictationStage::Recording(id) = stage {
                    recording.send_replace(Some(id));
                }
            });
            // The release is the stop, so a pause mid-hold must not end it.
            let pending = match dictation
                .submit(
                    DictationRequest::new(StopMode::Manual),
                    AutoStopRule::Off,
                    Some(events),
                )
                .await
            {
                Ok(pending) => pending,
                Err(error) => return log_outcome(Err(error)),
            };
            // A listener that went away mid-hold cannot report its end;
            // dropping the call cancels the recording.
            let Ok(end) = ended.await else { return };
            tracing::info!(trigger = "hotkey", ?end, "hold ended");
            match end {
                // Released before capture opened: nothing was heard, and
                // stopping a recording the instant it opens would only
                // report no speech. Dropping the call cancels it quietly.
                HoldEnd::Released if recorded.borrow().is_none() => {
                    drop(pending);
                    tracing::info!(
                        trigger = "hotkey",
                        status = "cancelled",
                        "hold released before recording began"
                    );
                }
                HoldEnd::Released => {
                    let id = recorded.borrow().expect("checked above");
                    runtime.stop_recording(id);
                    log_outcome(pending.wait().await);
                }
                HoldEnd::Cancelled => {
                    drop(pending);
                    tracing::info!(
                        trigger = "hotkey",
                        status = "cancelled",
                        "hold dictation discarded"
                    );
                }
            }
        });
        end
    }
}

/// A hold task is only gone when the actor is; its end then has nothing
/// left to steer.
fn end_hold(hold: &mut Option<oneshot::Sender<HoldEnd>>, end: HoldEnd) {
    if let Some(hold) = hold.take() {
        let _ = hold.send(end);
    }
}

fn log_outcome(outcome: Result<DictationOutcome, DictationCallError>) {
    match outcome {
        Ok(DictationOutcome::Completed(_)) => tracing::info!(
            trigger = "hotkey",
            status = "completed",
            "hotkey dictation finished"
        ),
        Ok(DictationOutcome::StopAccepted) => tracing::info!(
            trigger = "hotkey",
            status = "stop_accepted",
            "hotkey press stopped the running dictation"
        ),
        Err(DictationCallError::Runtime(DictationRuntimeError::Execution(
            DictationError::Cancelled,
        ))) => tracing::info!(
            trigger = "hotkey",
            status = "cancelled",
            "hotkey dictation finished"
        ),
        Err(DictationCallError::Unavailable) => tracing::info!(
            trigger = "hotkey",
            status = "unavailable",
            "hotkey dictation did not run; the daemon is stopping"
        ),
        Err(error @ (DictationCallError::Runtime(_) | DictationCallError::JobFailed)) => {
            tracing::warn!(
                trigger = "hotkey",
                status = "failed",
                %error,
                "hotkey dictation finished"
            );
        }
    }
}

fn log_chat_unreached(error: ChatCallError) {
    match error {
        CallError::Unavailable => tracing::info!(
            trigger = "hotkey",
            status = "unavailable",
            "hotkey tap did nothing; the daemon is stopping"
        ),
        error @ (CallError::Runtime(_) | CallError::JobFailed) => tracing::warn!(
            trigger = "hotkey",
            status = "failed",
            %error,
            "hotkey tap could not ask chat"
        ),
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

async fn report_exit(exit: HotkeyExit, health: &HealthReporter) {
    if exit == HotkeyExit::Failed {
        health
            .set_service_status(HOTKEY_HEALTH_SERVICE, ServingStatus::NotServing)
            .await;
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

    use crate::dictation::testing::{RecordsUntilStopped, recording_started};
    use crate::hotkey::Combo;
    use crate::ipc::wire;
    use crate::runtime::RuntimePaths;
    use crate::settings::SettingsStore;

    use super::super::chat::{ChatRuntime, channel as chat_channel};
    use super::super::context::Context;
    use super::super::dictation::channel as dictation_channel;
    use super::super::host::DaemonLifetime;
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

    fn fake_listener(
        key: AriaKey,
        _events: &dyn Fn(ListenerEvent),
        should_stop: &dyn Fn() -> bool,
    ) {
        ARMED.lock().expect("armed keys").push(key);
        park_until(should_stop);
    }

    fn shift_combo_listener(
        _key: AriaKey,
        events: &dyn Fn(ListenerEvent),
        should_stop: &dyn Fn() -> bool,
    ) {
        events(ListenerEvent::Press(Press::Combo(Combo::Shift)));
        park_until(should_stop);
    }

    fn tap_listener(_key: AriaKey, events: &dyn Fn(ListenerEvent), should_stop: &dyn Fn() -> bool) {
        events(ListenerEvent::Press(Press::Tap));
        park_until(should_stop);
    }

    /// A trigger with no dictation behind it. The directory holds its
    /// settings and must outlive it.
    fn trigger(
        key: watch::Receiver<Option<AriaKey>>,
        subscriptions: Arc<Subscriptions>,
        listener: Listener,
        directory: &tempfile::TempDir,
    ) -> HotkeyTrigger {
        let (health, _server) = tonic_health::server::health_reporter();
        HotkeyTrigger {
            key,
            dictation: DictationHandle::detached(),
            chat: ChatHandle::detached(),
            runtime: Arc::new(DictationRuntime::detached(Arc::new(
                SettingsStore::open(directory.path().join("settings.toml"))
                    .expect("the settings store should open"),
            ))),
            subscriptions,
            health,
            listener,
        }
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
        let (key, watch) = watch::channel(Some(AriaKey::RightOption));
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let trigger = trigger(
            watch,
            Arc::new(Subscriptions::new()),
            fake_listener,
            &directory,
        );
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(trigger.run(shutdown.clone()));

        armed_count_reaches(1).await;
        key.send(Some(AriaKey::Fn))
            .expect("the trigger is listening");
        armed_count_reaches(2).await;
        shutdown.cancel();
        running.await.expect("the trigger should stop cleanly");

        assert_eq!(
            *ARMED.lock().expect("armed keys"),
            [AriaKey::RightOption, AriaKey::Fn]
        );
    }

    /// Its own record, so it never races the re-arm test over [`ARMED`].
    static ARMED_FROM_OFF: Mutex<Vec<AriaKey>> = Mutex::new(Vec::new());

    fn listener_from_off(
        key: AriaKey,
        _events: &dyn Fn(ListenerEvent),
        should_stop: &dyn Fn() -> bool,
    ) {
        ARMED_FROM_OFF.lock().expect("armed keys").push(key);
        park_until(should_stop);
    }

    async fn armed_from_off(count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while ARMED_FROM_OFF.lock().expect("armed keys").len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the listener should arm in time");
    }

    #[tokio::test]
    async fn an_aria_key_that_is_off_arms_nothing_until_it_is_on() {
        let (key, watch) = watch::channel(None);
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let trigger = trigger(
            watch,
            Arc::new(Subscriptions::new()),
            listener_from_off,
            &directory,
        );
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(trigger.run(shutdown.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(ARMED_FROM_OFF.lock().expect("armed keys").is_empty());
        key.send(Some(AriaKey::Fn)).expect("the trigger is waiting");
        armed_from_off(1).await;
        key.send(None).expect("the trigger is listening");
        tokio::time::sleep(Duration::from_millis(50)).await;
        key.send(Some(AriaKey::RightOption))
            .expect("the trigger is waiting");
        armed_from_off(2).await;
        shutdown.cancel();
        running.await.expect("the trigger should stop cleanly");

        assert_eq!(
            *ARMED_FROM_OFF.lock().expect("armed keys"),
            [AriaKey::Fn, AriaKey::RightOption]
        );
    }

    #[tokio::test]
    async fn a_combo_reaches_subscribers() {
        let (_key, watch) = watch::channel(Some(AriaKey::RightOption));
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let subscriptions = Arc::new(Subscriptions::new());
        let mut events = subscriptions.subscribe().expect("a subscriber fits");
        let trigger = trigger(watch, subscriptions, shift_combo_listener, &directory);
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
                crate::settings::SettingsCache::open(
                    directory.path().join("chat-settings.toml"),
                    directory.path().to_path_buf(),
                )
                .expect("the settings store should open"),
            ),
            None,
            crate::delivery::Desktop::new(directory.path(), Default::default()),
        );
        let (chat, chat_actor) = chat_channel(Arc::new(chat));
        let mut dictation = DictationRuntime::detached(settings);
        let (executor, edges) = RecordsUntilStopped::new();
        dictation.execute_with(Box::new(executor));
        let dictation = Arc::new(dictation);
        let (handle, dictation_actor) = dictation_channel(Arc::clone(&dictation));
        let (_key, watch) = watch::channel(Some(AriaKey::RightOption));
        let (health, _server) = tonic_health::server::health_reporter();
        let trigger = HotkeyTrigger {
            key: watch,
            dictation: handle,
            chat,
            runtime: dictation,
            subscriptions,
            health,
            listener: tap_listener,
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
    async fn a_failed_exit_degrades_only_hotkey_health() {
        let (reporter, _server) = tonic_health::server::health_reporter();
        reporter
            .set_service_status(HOTKEY_HEALTH_SERVICE, tonic_health::ServingStatus::Serving)
            .await;
        let service = HealthService::from_health_reporter(reporter.clone());

        report_exit(HotkeyExit::Failed, &reporter).await;

        for (name, expected) in [
            (HOTKEY_HEALTH_SERVICE, ServingStatus::NotServing),
            ("", ServingStatus::Serving),
        ] {
            let response = service
                .check(tonic::Request::new(HealthCheckRequest {
                    service: name.to_owned(),
                }))
                .await
                .expect("the health check should answer");
            assert_eq!(response.into_inner().status(), expected, "{name:?}");
        }
    }
}
