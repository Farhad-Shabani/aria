//! The gRPC server for the Aria daemon.
//!
//! This module owns the tokio runtime, the unix listener, and the shutdown
//! choreography; `lan` owns the LAN port. Request execution lives in the
//! injected [`Services`].

mod activity;
mod lan;
mod local;

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tonic_health::pb::health_server::HealthServer;
use tonic_health::server::{HealthReporter, HealthService};

use crate::ipc::wire;
use crate::primitives::error_chain;
use crate::primitives::local_socket::ExclusiveListener;

pub(in crate::daemon) use activity::ActivityGuard;
use activity::{Activity, TrackActivityLayer};
pub(super) use lan::Relay;

use super::chat::ChatActor;
use super::dictation::DictationActor;
use super::host::DaemonLifetime;
use super::hotkey::HotkeyTrigger;
use super::listen::ListenState;
use super::memory::MemoryBacklog;
use super::preferences::PreferencesActor;
use super::service::{
    ChatRpc, DictationRpc, EventsService, HistoryRpc, HistoryWorker, InviteRpc, ListenRpc,
    PreferencesRpc, SpeakerRpc, SpeechRpc, VocabularyRpc,
};
use super::shutdown::ShutdownSignal;
use super::speaker::SpeakerActor;
use super::speech::SpeechActor;
use super::subscriptions::Subscriptions;
use super::tasks::RuntimeTasks;
use super::wake::WakeTrigger;

const ON_DEMAND_IDLE_TIMEOUT: Duration = Duration::from_millis(250);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long a fresh daemon waits for its first request. Only requests count as
/// activity, so without this grace an on-demand daemon could idle out between
/// its spawn and the spawning client's first RPC — which is why it is the
/// client's own startup deadline rather than a number of its own.
const STARTUP_IDLE_GRACE: Duration = super::session::STARTUP_TIMEOUT;
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);
/// Shorter than the shutdown grace, so an idle client alone never fails a
/// shutdown.
const CONNECTION_DRAIN_GRACE: Duration =
    SHUTDOWN_GRACE_PERIOD.saturating_sub(Duration::from_secs(1));

/// The daemon's own descriptors plus everything they import, for reflection.
const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/aria_daemon_descriptor.bin"));

type Result<T> = std::result::Result<T, ServerError>;

/// A bound daemon RPC server with exclusive ownership of its socket namespace.
pub(super) struct Server {
    socket: ExclusiveListener,
    lifetime: DaemonLifetime,
    services: Services,
    tasks: RuntimeTasks,
    actors: Actors,
    wake: Option<WakeTrigger>,
    hotkey: Option<HotkeyTrigger>,
}

/// The capability actors the server spawns and waits for, one per service
/// that executes domain work.
pub(super) struct Actors {
    pub(super) history: HistoryWorker,
    pub(super) dictation: DictationActor,
    pub(super) speech: SpeechActor,
    pub(super) chat: ChatActor,
    pub(super) preferences: PreferencesActor,
    pub(super) speaker: SpeakerActor,
    pub(super) memory: Option<MemoryBacklog>,
    pub(super) vocabulary: super::vocabulary::Worker<
        crate::vocabulary::discover::LocalCollector<crate::vocabulary::extract::LocalExtractor>,
    >,
}

impl Server {
    pub(super) fn new(
        socket: ExclusiveListener,
        lifetime: DaemonLifetime,
        services: Services,
        tasks: RuntimeTasks,
        actors: Actors,
        wake: Option<WakeTrigger>,
        hotkey: Option<HotkeyTrigger>,
    ) -> Self {
        Self {
            socket,
            lifetime,
            services,
            tasks,
            actors,
            wake,
            hotkey,
        }
    }

    /// Serves connections until its configured lifetime requests a graceful
    /// stop. Blocks the calling thread on an owned tokio runtime.
    pub(super) fn run(self, shutdown: ShutdownSignal) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .enable_time()
            .thread_name("aria-daemon-worker")
            .build()
            .map_err(ServerError::Runtime)?;
        runtime.block_on(self.serve(shutdown))
    }

    async fn serve(self, mut shutdown: ShutdownSignal) -> Result<()> {
        let Self {
            socket,
            lifetime,
            mut services,
            tasks,
            actors,
            wake,
            hotkey,
        } = self;

        let listener = adopt_listener(&socket)?;
        let activity = Activity::new();
        let subscriptions = services.events.subscriptions();
        // Held past the router, which otherwise owns the only handle: a live
        // listen session has to be stopped inside the drain rather than by
        // the router's own drop, which runs after it.
        let listen = services.listen.state();
        // Bound and advertised before the unix router is built, so a taken
        // port is reported while there is still a plain daemon to fall back
        // to. `_advert` unregisters when this scope ends, panic included.
        let lan::Opened {
            bound,
            advert: _advert,
            invites,
        } = match services.relay.take() {
            Some(relay) => lan::open(relay, &services.health).await,
            None => lan::Opened::default(),
        };
        let router = build_router(services, invites, &activity).await?;
        // Before wake or a request can submit work. Tracked, so waiting on
        // tasks waits for each actor — and through it, that capability's jobs.
        tasks.supervise(actors.dictation);
        tasks.supervise(actors.speech);
        tasks.supervise(actors.chat);
        tasks.supervise(actors.preferences);
        tasks.supervise(actors.speaker);
        tasks.supervise(actors.vocabulary);
        tasks.supervise(actors.history);
        if let Some(backlog) = actors.memory {
            tasks.supervise(backlog);
        }
        // Each trigger owns its health lifecycle. Announced before the
        // listener accepts, so a health check never finds an enabled
        // trigger's service unregistered.
        if let Some(wake) = wake {
            wake.announce().await;
            tasks.supervise(wake);
        }
        if let Some(hotkey) = hotkey {
            hotkey.announce().await;
            tasks.supervise(hotkey);
        }

        tracing::info!(
            socket = %socket.path().display(),
            lifetime = lifetime.label(),
            "daemon ready"
        );

        let unix = serve_local(router, listener, tasks.cancellation());
        // Two listeners, one stop. The LAN one is not allowed to end the
        // daemon: a relay that fell over must not take dictation with it.
        let lan_serving = lan::serve(bound, tasks.cancellation());
        let grpc = async move {
            let (unix, ()) = tokio::join!(unix, lan_serving);
            unix
        };
        tokio::pin!(grpc);

        let (reason, serve_result) = tokio::select! {
            // Nothing requested a stop, so the serve loop failed.
            result = &mut grpc => (StopReason::ServeFailed, Some(result)),
            reason = stop_requested(&mut shutdown, lifetime, &activity) => (reason, None),
        };

        let grpc_outcome = async move {
            match serve_result {
                Some(result) => result,
                None => grpc.await,
            }
        };
        drain(
            grpc_outcome,
            reason,
            &subscriptions,
            &tasks,
            &activity,
            listen,
        )
        .await
    }
}

// The ExclusiveListener keeps owning the lock and the socket path.
fn adopt_listener(socket: &ExclusiveListener) -> Result<tokio::net::UnixListener> {
    let listener = socket
        .listener()
        .try_clone()
        .map_err(ServerError::Listener)?;
    listener
        .set_nonblocking(true)
        .map_err(ServerError::Listener)?;
    tokio::net::UnixListener::from_std(listener).map_err(ServerError::Listener)
}

enum Drained {
    Closed,
    /// The grace ran out. Open connections, and any request on them, drop
    /// with the runtime.
    CutOff,
}

/// Ends at most `CONNECTION_DRAIN_GRACE` after `stop`.
async fn serve_local(
    router: ActivityRouter,
    listener: tokio::net::UnixListener,
    stop: CancellationToken,
) -> std::result::Result<Drained, tonic::transport::Error> {
    let serving = router
        .serve_with_incoming_shutdown(local::same_user(listener), stop.clone().cancelled_owned());
    match until_grace_after(serving, stop.cancelled_owned(), CONNECTION_DRAIN_GRACE).await {
        Some(result) => result.map(|()| Drained::Closed),
        None => {
            tracing::warn!("connections outlived the drain; they close with the daemon");
            Ok(Drained::CutOff)
        }
    }
}

/// `None` when `work` is still running `grace` after `stopped` resolves.
async fn until_grace_after<F: Future>(
    work: F,
    stopped: impl Future<Output = ()>,
    grace: Duration,
) -> Option<F::Output> {
    tokio::select! {
        output = work => Some(output),
        () = async {
            stopped.await;
            tokio::time::sleep(grace).await;
        } => None,
    }
}

type ActivityRouter = tonic::transport::server::Router<
    tower::layer::util::Stack<TrackActivityLayer, tower::layer::util::Identity>,
>;

/// Everything the daemon serves, one struct per Aria service, plus the
/// health reporter shared with the triggers.
pub(super) struct Services {
    pub(super) history: HistoryRpc,
    pub(super) dictation: DictationRpc,
    pub(super) speech: SpeechRpc,
    pub(super) chat: ChatRpc,
    pub(super) listen: ListenRpc,
    pub(super) preferences: PreferencesRpc,
    pub(super) vocabulary: VocabularyRpc,
    pub(super) speaker: SpeakerRpc,
    pub(super) events: EventsService,
    /// None when the relay is switched off, or on a daemon too short-lived
    /// to serve one. It is never added to the unix router.
    pub(super) relay: Option<Relay>,
    pub(super) health: HealthReporter,
}

async fn build_router(
    services: Services,
    invites: Option<InviteRpc>,
    activity: &Activity,
) -> Result<ActivityRouter> {
    let health_reporter = &services.health;
    health_reporter
        .set_serving::<wire::dictation_service_server::DictationServiceServer<DictationRpc>>()
        .await;
    health_reporter
        .set_serving::<wire::speech_service_server::SpeechServiceServer<SpeechRpc>>()
        .await;
    health_reporter
        .set_serving::<wire::chat_service_server::ChatServiceServer<ChatRpc>>()
        .await;
    health_reporter
        .set_serving::<wire::listen_service_server::ListenServiceServer<ListenRpc>>()
        .await;
    health_reporter
        .set_serving::<wire::preferences_service_server::PreferencesServiceServer<PreferencesRpc>>()
        .await;
    health_reporter
        .set_serving::<wire::speaker_service_server::SpeakerServiceServer<SpeakerRpc>>()
        .await;
    health_reporter
        .set_serving::<wire::events_service_server::EventsServiceServer<EventsService>>()
        .await;
    health_reporter
        .set_serving::<wire::vocabulary_service_server::VocabularyServiceServer<VocabularyRpc>>()
        .await;
    health_reporter
        .set_serving::<wire::history_service_server::HistoryServiceServer<HistoryRpc>>()
        .await;
    if invites.is_some() {
        health_reporter
            .set_serving::<wire::invite_service_server::InviteServiceServer<InviteRpc>>()
            .await;
    }
    let health = HealthServer::new(HealthService::from_health_reporter(health_reporter.clone()));

    // grpcurl still speaks v1alpha, so both reflection versions serve.
    let reflection = || {
        tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
            .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
    };
    let reflection_v1 = reflection().build_v1().map_err(ServerError::Reflection)?;
    let reflection_v1alpha = reflection()
        .build_v1alpha()
        .map_err(ServerError::Reflection)?;

    let router = tonic::transport::Server::builder()
        .layer(TrackActivityLayer {
            activity: activity.clone(),
        })
        .add_service(wire::dictation_service_server::DictationServiceServer::new(
            services.dictation,
        ))
        .add_service(wire::speech_service_server::SpeechServiceServer::new(
            services.speech,
        ))
        .add_service(wire::chat_service_server::ChatServiceServer::new(
            services.chat,
        ))
        .add_service(wire::listen_service_server::ListenServiceServer::new(
            services.listen,
        ))
        .add_service(
            wire::preferences_service_server::PreferencesServiceServer::new(services.preferences),
        )
        .add_service(wire::speaker_service_server::SpeakerServiceServer::new(
            services.speaker,
        ))
        .add_service(wire::events_service_server::EventsServiceServer::new(
            services.events,
        ))
        .add_service(
            wire::vocabulary_service_server::VocabularyServiceServer::new(services.vocabulary),
        )
        .add_service(wire::history_service_server::HistoryServiceServer::new(
            services.history,
        ))
        .add_optional_service(invites.map(wire::invite_service_server::InviteServiceServer::new))
        .add_service(health)
        .add_service(reflection_v1)
        .add_service(reflection_v1alpha);
    Ok(router)
}

async fn drain(
    grpc: impl Future<Output = std::result::Result<Drained, tonic::transport::Error>>,
    reason: StopReason,
    subscriptions: &Subscriptions,
    tasks: &RuntimeTasks,
    activity: &Activity,
    listen: Arc<ListenState>,
) -> Result<()> {
    tracing::info!(
        reason = reason.label(),
        active_requests = activity.active_count(),
        "daemon shutdown requested; draining active requests"
    );
    let shutdown_started = Instant::now();
    // The session joins its own threads, so it goes to the blocking pool —
    // and it starts here rather than after the drain, so the two wind down
    // together and the grace period covers both.
    let stopping = tokio::task::spawn_blocking(move || listen.stop());
    // Open event streams would otherwise hold the drain until the grace
    // period expires on every stop.
    subscriptions.close_all();
    tasks.request_stop();

    let shutdown = async {
        let grpc_result = grpc.await;
        tasks.wait().await;
        match stopping.await {
            Ok(Ok(_stopped)) => {}
            Ok(Err(failure)) => tracing::warn!(
                code = failure.code.label(),
                message = failure.message,
                "the listen session failed while the daemon stopped"
            ),
            Err(_join) => tracing::error!("the listen stop task panicked"),
        }
        grpc_result
    };
    let (result, timed_out_active_requests) =
        await_completion(shutdown, SHUTDOWN_GRACE_PERIOD, || {
            let active_requests = activity.active_count();
            tracing::error!(
                active_requests,
                grace_seconds = SHUTDOWN_GRACE_PERIOD.as_secs(),
                elapsed_ms = shutdown_started.elapsed().as_millis(),
                "daemon shutdown grace period expired; waiting for task completion"
            );
            active_requests
        })
        .await;
    // Any request still counted after a cut-off died with its connection.
    let cut_off_requests = match result.map_err(ServerError::Serve)? {
        Drained::CutOff => activity.active_count(),
        Drained::Closed => 0,
    };
    match timed_out_active_requests {
        Some(active_requests) => Err(ServerError::ShutdownTimeout { active_requests }),
        None if cut_off_requests > 0 => {
            tracing::error!(
                active_requests = cut_off_requests,
                elapsed_ms = shutdown_started.elapsed().as_millis(),
                "daemon shutdown cut off requests still running"
            );
            Err(ServerError::ShutdownTimeout {
                active_requests: cut_off_requests,
            })
        }
        None => {
            tracing::info!(
                elapsed_ms = shutdown_started.elapsed().as_millis(),
                "daemon shutdown completed"
            );
            Ok(())
        }
    }
}

async fn await_completion<F, G, X>(
    work: F,
    grace: Duration,
    grace_expired: G,
) -> (F::Output, Option<X>)
where
    F: Future,
    G: FnOnce() -> X,
{
    tokio::pin!(work);
    match tokio::time::timeout(grace, work.as_mut()).await {
        Ok(output) => (output, None),
        Err(_grace_expired) => {
            let detail = grace_expired();
            (work.await, Some(detail))
        }
    }
}

async fn stop_requested(
    shutdown: &mut ShutdownSignal,
    lifetime: DaemonLifetime,
    activity: &Activity,
) -> StopReason {
    match lifetime {
        DaemonLifetime::Persistent => {
            signal_triggered(shutdown).await;
            StopReason::Signal
        }
        DaemonLifetime::OnDemand => tokio::select! {
            () = signal_triggered(shutdown) => StopReason::Signal,
            () = idle_elapsed(activity) => StopReason::IdleTimeout,
        },
    }
}

/// A watcher failure must still stop the daemon: better a clean early exit
/// than serving on without any way to be stopped.
async fn signal_triggered(shutdown: &mut ShutdownSignal) {
    if let Err(error) = shutdown.triggered().await {
        tracing::error!(
            error = %error_chain(&error),
            "the shutdown signal watcher failed; stopping"
        );
    }
}

async fn idle_elapsed(activity: &Activity) {
    tokio::time::sleep(STARTUP_IDLE_GRACE).await;
    loop {
        if activity.is_idle_for(ON_DEMAND_IDLE_TIMEOUT) {
            return;
        }
        tokio::time::sleep(IDLE_POLL_INTERVAL).await;
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum StopReason {
    Signal,
    IdleTimeout,
    ServeFailed,
}

impl StopReason {
    fn label(self) -> &'static str {
        match self {
            Self::Signal => "signal",
            Self::IdleTimeout => "idle_timeout",
            Self::ServeFailed => "serve_failed",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("failed to build the daemon async runtime")]
    Runtime(#[source] io::Error),

    #[error("failed to adopt the daemon listener into the async runtime")]
    Listener(#[source] io::Error),

    #[error("failed to build the daemon reflection service")]
    Reflection(#[source] tonic_reflection::server::Error),

    #[error("the daemon gRPC server failed")]
    Serve(#[source] tonic::transport::Error),

    #[error("daemon shutdown timed out with {active_requests} active request(s)")]
    ShutdownTimeout { active_requests: usize },
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tonic::server::NamedService;
    use tonic::transport::{Channel, Endpoint};
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_client::HealthClient;

    use crate::daemon::{Daemon, DaemonLifetime, RunningDaemon};

    use super::*;

    #[tokio::test]
    async fn grace_expiry_still_awaits_completion() {
        let release = tokio_util::sync::CancellationToken::new();
        let delayed_release = release.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            delayed_release.cancel();
        });

        let (value, expired) = await_completion(
            async {
                release.cancelled().await;
                42
            },
            Duration::from_millis(5),
            || "grace expired",
        )
        .await;

        assert_eq!(value, 42);
        assert_eq!(expired, Some("grace expired"));
    }

    #[tokio::test(start_paused = true)]
    async fn work_that_never_ends_ends_a_grace_after_the_stop() {
        let stop = CancellationToken::new();
        let waiting = tokio::spawn(until_grace_after(
            std::future::pending::<()>(),
            stop.clone().cancelled_owned(),
            CONNECTION_DRAIN_GRACE,
        ));

        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(!waiting.is_finished(), "nothing ends it before the stop");
        let stopped_at = tokio::time::Instant::now();
        stop.cancel();

        assert_eq!(waiting.await.expect("the wait does not panic"), None);
        assert_eq!(stopped_at.elapsed(), CONNECTION_DRAIN_GRACE);
    }

    // OnDemand defers the model, so these daemons run on machines without one.
    fn on_demand_daemon() -> (tempfile::TempDir, RunningDaemon) {
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let daemon = Daemon::builder()
            .runtime_dir(directory.path())
            .lifetime(DaemonLifetime::OnDemand)
            .open()
            .expect("the daemon should configure")
            .spawn()
            .expect("the daemon should spawn");
        (directory, daemon)
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("the test runtime should build")
    }

    async fn connect(daemon: &RunningDaemon) -> Channel {
        let endpoint = Endpoint::from_shared(format!("unix://{}", daemon.socket_path().display()))
            .expect("the endpoint should parse");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match endpoint.connect().await {
                Ok(channel) => return channel,
                Err(error) if Instant::now() < deadline => {
                    tracing::debug!(%error, "the daemon is not accepting yet");
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => panic!("the daemon never accepted: {error}"),
            }
        }
    }

    #[test]
    fn an_idle_on_demand_daemon_exits_on_its_own() {
        let (_directory, daemon) = on_demand_daemon();

        let started = Instant::now();
        daemon.join().expect("the idle daemon should exit cleanly");

        assert!(
            started.elapsed() < STARTUP_IDLE_GRACE + Duration::from_secs(5),
            "the idle exit should fire soon after the startup grace"
        );
    }

    // What a temporary session saves by stopping its daemon once the outcome
    // is delivered, rather than joining on the idle exit.
    #[test]
    fn an_explicit_stop_does_not_wait_out_the_startup_grace() {
        let (_directory, daemon) = on_demand_daemon();

        let started = Instant::now();
        daemon.stop().expect("the daemon should stop cleanly");

        assert!(
            started.elapsed() < STARTUP_IDLE_GRACE,
            "a requested stop must not sit through the idle grace"
        );
    }

    // Asserted by name, so a service left out of the health registrations
    // fails here.
    #[test]
    fn every_aria_service_reports_healthy() {
        let (_directory, daemon) = on_demand_daemon();
        let runtime = test_runtime();

        runtime.block_on(async {
            let mut health = HealthClient::new(connect(&daemon).await);
            for service in [
                "",
                wire::dictation_service_server::DictationServiceServer::<DictationRpc>::NAME,
                wire::speech_service_server::SpeechServiceServer::<SpeechRpc>::NAME,
                wire::chat_service_server::ChatServiceServer::<ChatRpc>::NAME,
                wire::preferences_service_server::PreferencesServiceServer::<PreferencesRpc>::NAME,
                wire::history_service_server::HistoryServiceServer::<HistoryRpc>::NAME,
                wire::listen_service_server::ListenServiceServer::<ListenRpc>::NAME,
                wire::speaker_service_server::SpeakerServiceServer::<SpeakerRpc>::NAME,
                wire::events_service_server::EventsServiceServer::<EventsService>::NAME,
            ] {
                let response = health
                    .check(HealthCheckRequest {
                        service: service.to_owned(),
                    })
                    .await
                    .expect("the health check should answer");
                assert_eq!(
                    response.into_inner().status(),
                    ServingStatus::Serving,
                    "{service:?} should be serving"
                );
            }
            // An on-demand daemon never runs the relay, so it has no code to
            // issue.
            let issuing = health
                .check(HealthCheckRequest {
                    service: wire::invite_service_server::InviteServiceServer::<InviteRpc>::NAME
                        .to_owned(),
                })
                .await
                .expect_err("pairing codes should not be registered without the relay");
            assert_eq!(issuing.code(), tonic::Code::NotFound);
        });
    }

    #[test]
    fn history_is_available_over_the_unix_socket_without_loading_a_model() {
        let (_directory, daemon) = on_demand_daemon();
        let runtime = test_runtime();
        runtime.block_on(async {
            let mut history =
                wire::history_service_client::HistoryServiceClient::new(connect(&daemon).await);
            let page = history
                .list(wire::ListHistoryRequest {
                    limit: 50,
                    ..Default::default()
                })
                .await
                .unwrap()
                .into_inner();
            assert!(page.entries.is_empty());
            let cleared = history
                .clear(wire::ClearHistoryRequest {})
                .await
                .unwrap()
                .into_inner();
            assert_eq!(cleared.deleted_entry_count, 0);
            assert!(!cleared.audio_cleanup_pending);
            assert_eq!(
                history
                    .get(wire::GetHistoryRequest { id: 1 })
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::NotFound
            );
            assert_eq!(
                history
                    .regenerate(wire::RegenerateHistoryRequest {
                        id: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::NotFound
            );
            for (engine, language) in [(Some("unknown"), None), (None, Some(""))] {
                assert_eq!(
                    history
                        .regenerate(wire::RegenerateHistoryRequest {
                            id: 1,
                            engine: engine.map(str::to_owned),
                            language: language.map(str::to_owned),
                        })
                        .await
                        .unwrap_err()
                        .code(),
                    tonic::Code::InvalidArgument
                );
            }
            history
                .delete(wire::DeleteHistoryRequest { id: 1 })
                .await
                .unwrap();
        });
        drop(runtime);
        daemon.stop().unwrap();
    }

    #[test]
    fn a_stop_ends_open_event_streams_within_the_grace() {
        let (_directory, daemon) = on_demand_daemon();
        let stopper = daemon.stopper();
        let runtime = test_runtime();

        runtime.block_on(async {
            let mut events =
                wire::events_service_client::EventsServiceClient::new(connect(&daemon).await);
            let mut stream = events
                .subscribe(wire::SubscribeRequest {})
                .await
                .expect("the subscription should be admitted")
                .into_inner();

            let stopped = Instant::now();
            stopper.stop();
            let end = stream
                .message()
                .await
                .expect("the stream should end cleanly on shutdown");

            assert!(
                end.is_none(),
                "shutdown should end the stream, not error it"
            );
            assert!(
                stopped.elapsed() < SHUTDOWN_GRACE_PERIOD,
                "an open subscription must not consume the shutdown grace"
            );
        });

        // Closing the client lets the drain end without waiting out the grace.
        drop(runtime);
        daemon
            .join()
            .expect("the stopped daemon should exit cleanly");
    }

    #[test]
    fn a_stop_returns_while_a_client_stays_connected() {
        let (_directory, daemon) = on_demand_daemon();
        let runtime = test_runtime();
        // A connection that has served a call and is never polled again never
        // answers the daemon's HTTP/2 GOAWAY.
        let _idle = runtime.block_on(async {
            let channel = connect(&daemon).await;
            HealthClient::new(channel.clone())
                .check(HealthCheckRequest::default())
                .await
                .expect("the daemon should answer a health check");
            channel
        });

        daemon.stopper().stop();
        let (joined, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || joined.send(daemon.join()));

        outcome
            .recv_timeout(SHUTDOWN_GRACE_PERIOD + Duration::from_secs(1))
            .expect("the daemon should stop within its grace")
            .expect("the stopped daemon should exit cleanly");
    }
}
