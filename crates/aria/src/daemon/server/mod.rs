//! The gRPC server for the Aria daemon.
//!
//! This module owns the tokio runtime, the listener, and the shutdown
//! choreography. Request execution lives in the injected [`Services`].

mod activity;

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_stream::wrappers::{TcpListenerStream, UnixListenerStream};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Identity, ServerTlsConfig};
use tonic_health::pb::health_server::HealthServer;
use tonic_health::server::{HealthReporter, HealthService};

use crate::ipc::wire;
use crate::primitives::local_socket::ExclusiveListener;

pub(in crate::daemon) use activity::ActivityGuard;
use activity::{Activity, TrackActivityLayer};

use super::chat::ChatActor;
use super::dictation::DictationActor;
use super::host::DaemonLifetime;
use super::hotkey::HotkeyTrigger;
use super::listen::ListenState;
use super::memory::MemoryBacklog;
use super::preferences::PreferencesActor;
use super::relay::{Advert, LanPreferences, Pairing, require_bearer};
use super::service::{
    ChatRpc, DictationRpc, EventsService, HistoryRpc, HistoryWorker, ListenRpc, PreferencesRpc,
    RelayRpc, SpeakerRpc, SpeechRpc, VocabularyRpc,
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
/// The largest `RelayRequest` the relay will decode. The phone ships 256 KiB
/// chunks; this is generous for one of those and nowhere near a session.
const MAX_RELAY_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
/// How many relay streams may run on one connection. One is doing work —
/// only one can hold the recorder — and the rest are slack for a client
/// retrying over a connection it has not yet given up on. Connections
/// themselves are uncapped; the keepalive below is what reaps the dead ones.
const MAX_RELAY_STREAMS: usize = 4;
/// A preference patch is a few hundred bytes; this leaves room for a long
/// vocabulary path and nothing else.
const MAX_PREFERENCES_MESSAGE_BYTES: usize = 64 * 1024;
const RELAY_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const RELAY_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

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
    pub(super) vocabulary: super::vocabulary::Worker<crate::vocabulary::extract::LocalExtractor>,
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
        let Lan {
            relay: lan,
            preferences: lan_preferences,
            advert: _advert,
        } = match services.relay.take() {
            Some(relay) => open_relay(relay, &services.health).await,
            None => Lan::default(),
        };
        let router = build_router(services, &activity).await?;
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

        let unix = router.serve_with_incoming_shutdown(
            UnixListenerStream::new(listener),
            tasks.cancellation().cancelled_owned(),
        );
        // Three listeners, one stop. The LAN ones are not allowed to end the
        // daemon: a relay that fell over must not take dictation with it.
        let serve_lan = |listener: Option<RelayServer>, name: &'static str| {
            let cancelled = tasks.cancellation().cancelled_owned();
            async move {
                let Some((router, incoming)) = listener else {
                    return;
                };
                if let Err(error) = router
                    .serve_with_incoming_shutdown(incoming, cancelled)
                    .await
                {
                    tracing::error!(%error, name, "a LAN listener failed");
                }
            }
        };
        let relay = serve_lan(lan, "relay");
        let preferences = serve_lan(lan_preferences, "settings");
        let grpc = async move {
            let (unix, (), ()) = tokio::join!(unix, relay, preferences);
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

/// The relay and the port it answers on, together because "served" and
/// "where" are the same decision: carried apart, a mismatch binds a port
/// nobody was told about. Beside it, what a paired phone may reach: the
/// preferences service and the material that gates it, absent when the
/// daemon could not mint it.
pub(super) struct Relay {
    pub(super) rpc: RelayRpc,
    pub(super) port: u16,
    pub(super) preferences: PreferencesRpc,
    pub(super) pairing: Option<Pairing>,
}

/// A LAN half of the daemon: a router, and the socket it answers on.
type RelayServer = (RelayRouter, TcpListenerStream);

type RelayRouter = tonic::transport::server::Router;

/// What opening the LAN produced. Any part may be missing; the unix socket
/// serves regardless.
#[derive(Default)]
struct Lan {
    relay: Option<RelayServer>,
    preferences: Option<RelayServer>,
    advert: Option<Advert>,
}

async fn open_relay(relay: Relay, health: &HealthReporter) -> Lan {
    let Relay {
        rpc,
        port,
        preferences,
        pairing,
    } = relay;
    let listener = match tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        .await
    {
        Ok(listener) => listener,
        Err(error) => {
            // Not fatal by design: the daemon still serves every local client
            // over its socket. Only the phone loses its way in.
            tracing::error!(%error, port, "failed to bind the relay port; serving locally only");
            return Lan::default();
        }
    };
    // Its own reporter, not the daemon's. Sharing one would answer a LAN
    // client's health query for every service it cannot reach, which is a
    // free inventory of what this Mac runs. The daemon's reporter learns of
    // the relay too, so a local client can ask whether it opened.
    let (relay_health, _service) = tonic_health::server::health_reporter();
    relay_health
        .set_serving::<wire::relay_service_server::RelayServiceServer<RelayRpc>>()
        .await;
    health
        .set_serving::<wire::relay_service_server::RelayServiceServer<RelayRpc>>()
        .await;
    let router = relay_router(rpc, relay_health);

    // What the OS actually gave us. A configured port of 0 means an ephemeral
    // one, and advertising the request rather than the result would send
    // every phone to the wrong place.
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            tracing::error!(%error, "the relay port is unreadable; not advertising");
            return Lan {
                relay: Some((router, TcpListenerStream::new(listener))),
                ..Lan::default()
            };
        }
    };
    let (preferences, preferences_port) = match pairing {
        Some(pairing) => open_preferences(LanPreferences::new(preferences), &pairing).await,
        None => (None, None),
    };
    let advert = Advert::register(port, preferences_port)
        .inspect_err(|error| tracing::error!(%error, "the relay is not discoverable"))
        .ok();
    tracing::info!(port, "relay listening on the LAN");
    Lan {
        relay: Some((router, TcpListenerStream::new(listener))),
        preferences,
        advert,
    }
}

/// The settings channel for paired phones: an ephemeral TLS port carrying
/// only the preferences service. Its port travels in the Bonjour TXT record.
async fn open_preferences(
    preferences: LanPreferences,
    pairing: &Pairing,
) -> (Option<RelayServer>, Option<u16>) {
    let router = match preferences_router(preferences, pairing) {
        Ok(router) => router,
        Err(error) => {
            tracing::error!(%error, "the pairing certificate is unusable; settings stay Mac-only");
            return (None, None);
        }
    };
    let listener = match tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, "failed to bind the settings port; settings stay Mac-only");
            return (None, None);
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            tracing::error!(%error, "the settings port is unreadable; settings stay Mac-only");
            return (None, None);
        }
    };
    tracing::info!(port, "settings listening for paired phones");
    (Some((router, TcpListenerStream::new(listener))), Some(port))
}

/// TLS with the Mac's own certificate, and a bearer check in front of the
/// one service registered. No health and no reflection: a stranger gets the
/// certificate and an `UNAUTHENTICATED` status, nothing more.
fn preferences_router<P>(
    preferences: P,
    pairing: &Pairing,
) -> std::result::Result<RelayRouter, tonic::transport::Error>
where
    P: wire::preferences_service_server::PreferencesService,
{
    let identity = Identity::from_pem(&pairing.certificate_pem, &pairing.key_pem);
    let service = InterceptedService::new(
        wire::preferences_service_server::PreferencesServiceServer::new(preferences)
            .max_decoding_message_size(MAX_PREFERENCES_MESSAGE_BYTES),
        require_bearer(pairing.token.clone()),
    );
    Ok(tonic::transport::Server::builder()
        .tls_config(ServerTlsConfig::new().identity(identity))?
        .concurrency_limit_per_connection(MAX_RELAY_STREAMS)
        .max_concurrent_streams(Some(MAX_RELAY_STREAMS as u32))
        .http2_keepalive_interval(Some(RELAY_KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(RELAY_KEEPALIVE_TIMEOUT))
        .add_service(service))
}

/// Exposes only relay and health services over TCP. The generic relay lets
/// registration tests run without a model.
fn relay_router<R>(relay: R, health: HealthReporter) -> RelayRouter
where
    R: wire::relay_service_server::RelayService,
{
    tonic::transport::Server::builder()
        // Bound streams per connection because LAN clients are unauthenticated.
        .concurrency_limit_per_connection(MAX_RELAY_STREAMS)
        .max_concurrent_streams(Some(MAX_RELAY_STREAMS as u32))
        // A phone that walks out of range sends no FIN. Without keepalive the
        // connection sits open until something else times it out.
        .http2_keepalive_interval(Some(RELAY_KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(RELAY_KEEPALIVE_TIMEOUT))
        .add_service(
            wire::relay_service_server::RelayServiceServer::new(relay)
                // Bound each message before the handler sees it. The upload
                // collector separately caps the total session payload.
                .max_decoding_message_size(MAX_RELAY_MESSAGE_BYTES),
        )
        // The phone checks the Mac is up before it records, and this is the
        // check every gRPC client already has.
        .add_service(HealthServer::new(HealthService::from_health_reporter(
            health,
        )))
}

async fn build_router(services: Services, activity: &Activity) -> Result<ActivityRouter> {
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
        .add_service(health)
        .add_service(reflection_v1)
        .add_service(reflection_v1alpha);
    Ok(router)
}

async fn drain(
    grpc: impl Future<Output = std::result::Result<(), tonic::transport::Error>>,
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
    result.map_err(ServerError::Serve)?;
    match timed_out_active_requests {
        None => {
            tracing::info!(
                elapsed_ms = shutdown_started.elapsed().as_millis(),
                "daemon shutdown completed"
            );
            Ok(())
        }
        Some(active_requests) => Err(ServerError::ShutdownTimeout { active_requests }),
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
        tracing::error!(?error, "the shutdown signal watcher failed; stopping");
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

    use tokio_stream::Stream;
    use tonic::Status;
    use tonic::server::NamedService;
    use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_client::HealthClient;

    use crate::daemon::{Daemon, DaemonLifetime, RunningDaemon};

    use super::*;

    /// Refuses every stream it is handed. What it stands in for is the
    /// registration, so the relay router can be built without a model behind
    /// it.
    struct SilentRelay;

    #[tonic::async_trait]
    impl wire::relay_service_server::RelayService for SilentRelay {
        type RelayStream = std::pin::Pin<
            Box<dyn Stream<Item = std::result::Result<wire::RelayEvent, Status>> + Send>,
        >;

        async fn relay(
            &self,
            _request: tonic::Request<tonic::Streaming<wire::RelayRequest>>,
        ) -> std::result::Result<tonic::Response<Self::RelayStream>, Status> {
            // Not Unimplemented: that is what the router says about a service
            // it does not carry, and this test is about telling the two
            // apart.
            Err(Status::aborted("the stand-in relay was reached"))
        }
    }

    /// The service restriction, from the outside: a client on the TCP port
    /// reaches the relay and the health check and nothing else, however many
    /// services the unix router grows.
    #[tokio::test]
    async fn the_lan_listener_serves_the_relay_and_refuses_everything_else() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("an ephemeral port should bind");
        let address = listener.local_addr().expect("the port should be readable");
        let (health, _service) = tonic_health::server::health_reporter();
        let stop = tokio_util::sync::CancellationToken::new();
        let serving = tokio::spawn(
            relay_router(SilentRelay, health).serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                stop.clone().cancelled_owned(),
            ),
        );

        let channel = Endpoint::from_shared(format!("http://{address}"))
            .expect("the endpoint should parse")
            .connect()
            .await
            .expect("the relay listener should accept");

        // Registered here, so it answers on its own terms.
        let relayed = wire::relay_service_client::RelayServiceClient::new(channel.clone())
            .relay(tokio_stream::empty())
            .await
            .expect_err("the stand-in refuses every stream");
        // Registered only on the unix router, so the LAN never sees it.
        let preferences =
            wire::preferences_service_client::PreferencesServiceClient::new(channel.clone())
                .get(wire::GetPreferencesRequest {})
                .await
                .expect_err("preferences must not be reachable over TCP");
        let mut history = wire::history_service_client::HistoryServiceClient::new(channel.clone());
        let list = history
            .list(wire::ListHistoryRequest {
                query: String::new(),
                before_id: None,
                limit: 10,
            })
            .await
            .expect_err("history must not be reachable over TCP");
        let get = history
            .get(wire::GetHistoryRequest { id: 1 })
            .await
            .expect_err("history get must not be reachable over TCP");
        let delete = history
            .delete(wire::DeleteHistoryRequest { id: 1 })
            .await
            .expect_err("history delete must not be reachable over TCP");
        let retry = history
            .regenerate(wire::RegenerateHistoryRequest {
                id: 1,
                ..Default::default()
            })
            .await
            .expect_err("history retry must not be reachable over TCP");
        let clear = history
            .clear(wire::ClearHistoryRequest {})
            .await
            .expect_err("history clear must not be reachable over TCP");
        for status in [list, get, delete, retry, clear] {
            assert_eq!(status.code(), tonic::Code::Unimplemented);
        }
        // The likeliest regression, and the worst: reflection would hand a
        // stranger the shape of every service the unix socket carries.
        let reflection =
            tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient::new(
                channel,
            )
            .server_reflection_info(tokio_stream::empty())
            .await
            .expect_err("reflection must not be reachable over TCP");

        assert_eq!(
            relayed.code(),
            tonic::Code::Aborted,
            "the relay's own handler should answer over TCP"
        );
        assert_eq!(
            preferences.code(),
            tonic::Code::Unimplemented,
            "the router should not know preferences over TCP"
        );
        assert_eq!(
            reflection.code(),
            tonic::Code::Unimplemented,
            "the router should not reflect over TCP"
        );

        stop.cancel();
        serving
            .await
            .expect("the serve task should join")
            .expect("the relay listener should stop cleanly");
    }

    /// Reached only through the interceptor, so a call that lands here
    /// proves the bearer check passed.
    struct SilentPreferences;

    #[tonic::async_trait]
    impl wire::preferences_service_server::PreferencesService for SilentPreferences {
        async fn get(
            &self,
            _request: tonic::Request<wire::GetPreferencesRequest>,
        ) -> std::result::Result<tonic::Response<wire::PreferenceSnapshot>, Status> {
            Err(Status::aborted("the stand-in preferences were reached"))
        }

        async fn update(
            &self,
            _request: tonic::Request<wire::UpdatePreferencesRequest>,
        ) -> std::result::Result<tonic::Response<wire::UpdatePreferencesResponse>, Status> {
            Err(Status::aborted("the stand-in preferences were reached"))
        }
    }

    /// The settings port, from the outside: TLS with the Mac's certificate,
    /// the exact token, and nothing but the preferences service.
    #[tokio::test]
    async fn the_settings_listener_requires_tls_and_the_token() {
        let directory = tempfile::tempdir().expect("a temporary directory should be created");
        let pairing = super::super::relay::load_pairing(
            &directory.path().join("pairing.json"),
            "Aria (test)",
        )
        .expect("pairing material should mint");
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("an ephemeral port should bind");
        let address = listener.local_addr().expect("the port should be readable");
        let stop = tokio_util::sync::CancellationToken::new();
        let serving = tokio::spawn(
            preferences_router(SilentPreferences, &pairing)
                .expect("the minted certificate should be usable")
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(listener),
                    stop.clone().cancelled_owned(),
                ),
        );

        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&pairing.certificate_pem))
            .domain_name("aria.local");
        let channel = Endpoint::from_shared(format!("https://{address}"))
            .expect("the endpoint should parse")
            .tls_config(tls)
            .expect("the client TLS config should build")
            .connect()
            .await
            .expect("a pinned client should connect");

        let unauthenticated =
            wire::preferences_service_client::PreferencesServiceClient::new(channel.clone())
                .get(wire::GetPreferencesRequest {})
                .await
                .expect_err("no token must be refused");
        assert_eq!(unauthenticated.code(), tonic::Code::Unauthenticated);

        let wrong = call_with_bearer(channel.clone(), "Bearer 0000").await;
        assert_eq!(wrong.code(), tonic::Code::Unauthenticated);

        let right = call_with_bearer(channel.clone(), &bearer(&pairing)).await;
        assert_eq!(
            right.code(),
            tonic::Code::Aborted,
            "the right token should reach the handler"
        );

        // Nothing else lives here, token or not.
        let mut relay_request = tonic::Request::new(tokio_stream::empty());
        relay_request.metadata_mut().insert(
            "authorization",
            bearer(&pairing).parse().expect("the header should parse"),
        );
        let relayed = wire::relay_service_client::RelayServiceClient::new(channel.clone())
            .relay(relay_request)
            .await
            .expect_err("the relay must not answer on the settings port");
        assert_eq!(relayed.code(), tonic::Code::Unimplemented);
        let health = HealthClient::new(channel.clone())
            .check(HealthCheckRequest {
                service: String::new(),
            })
            .await
            .expect_err("health must not answer on the settings port");
        assert_eq!(health.code(), tonic::Code::Unimplemented);
        let reflection =
            tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient::new(
                channel.clone(),
            )
            .server_reflection_info(tokio_stream::empty())
            .await
            .expect_err("reflection must not answer on the settings port");
        assert_eq!(reflection.code(), tonic::Code::Unimplemented);

        // Plaintext gets no further than the handshake.
        let plaintext = Endpoint::from_shared(format!("http://{address}"))
            .expect("the endpoint should parse")
            .connect()
            .await;
        let refused = match plaintext {
            Err(_) => true,
            Ok(channel) => wire::preferences_service_client::PreferencesServiceClient::new(channel)
                .get(wire::GetPreferencesRequest {})
                .await
                .is_err(),
        };
        assert!(refused, "a plaintext client must not be served");

        stop.cancel();
        serving
            .await
            .expect("the serve task should join")
            .expect("the settings listener should stop cleanly");
    }

    fn bearer(pairing: &Pairing) -> String {
        // The test needs the raw value the phone would send; the type hides
        // it everywhere else on purpose.
        format!("Bearer {}", pairing.token.raw())
    }

    async fn call_with_bearer(channel: Channel, header: &str) -> Status {
        let mut request = tonic::Request::new(wire::GetPreferencesRequest {});
        request.metadata_mut().insert(
            "authorization",
            header.parse().expect("the header should parse"),
        );
        wire::preferences_service_client::PreferencesServiceClient::new(channel)
            .get(request)
            .await
            .expect_err("the stand-in refuses every call")
    }

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
    fn a_stale_preferences_update_answers_a_conflict() {
        let (_directory, daemon) = on_demand_daemon();
        let runtime = test_runtime();

        runtime.block_on(async {
            let mut preferences = wire::preferences_service_client::PreferencesServiceClient::new(
                connect(&daemon).await,
            );
            let snapshot = preferences
                .get(wire::GetPreferencesRequest {})
                .await
                .expect("preferences should be readable")
                .into_inner();

            let stale = wire::UpdatePreferencesRequest {
                expected_revision: snapshot.revision + 1,
                patch: Some(wire::PreferencePatch::default()),
            };
            let update = preferences
                .update(stale)
                .await
                .expect("the stale update should answer, not fail")
                .into_inner();

            assert!(
                matches!(
                    update.result,
                    Some(wire::update_preferences_response::Result::Conflict(_))
                ),
                "a stale revision should answer a conflict"
            );
        });
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

        // The daemon's drain waits for the client connection to close, and
        // nothing polls the client runtime once block_on returns.
        drop(runtime);
        daemon
            .join()
            .expect("the stopped daemon should exit cleanly");
    }
}
