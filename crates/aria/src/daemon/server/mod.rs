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
use super::relay::{Advert, LanPreferences, Pairing, PairingCodes, require_bearer};
use super::service::{
    ChatRpc, DictationRpc, EventsService, HistoryRpc, HistoryWorker, InviteRpc, ListenRpc,
    PairingRpc, PreferencesRpc, RelayRpc, SpeakerRpc, SpeechRpc, VocabularyRpc,
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
/// How many streams may run on one LAN connection. Only one relay can hold
/// the recorder. The rest is slack for a settings or health call, or a
/// retry. Connections are uncapped; the keepalive below reaps dead ones.
const MAX_RELAY_STREAMS: usize = 4;
/// A preference patch is a few hundred bytes; this leaves room for a long
/// vocabulary path and nothing else.
const MAX_PREFERENCES_MESSAGE_BYTES: usize = 64 * 1024;
/// A pairing request carries one 32-character code, and anyone on the LAN
/// can send one.
const MAX_PAIR_MESSAGE_BYTES: usize = 1024;
/// A phone on the LAN finishes its handshake in well under a second. A
/// connection that has not finished by then holds a socket for nothing.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
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
            advert: _advert,
            invites,
        } = match services.relay.take() {
            Some(relay) => open_relay(relay, &services.health).await,
            None => Lan::default(),
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

        let unix = router.serve_with_incoming_shutdown(
            UnixListenerStream::new(listener),
            tasks.cancellation().cancelled_owned(),
        );
        // Two listeners, one stop. The LAN one is not allowed to end the
        // daemon: a relay that fell over must not take dictation with it.
        let cancelled = tasks.cancellation().cancelled_owned();
        let relay = async move {
            let Some((router, incoming)) = lan else {
                return;
            };
            if let Err(error) = router
                .serve_with_incoming_shutdown(incoming, cancelled)
                .await
            {
                tracing::error!(%error, "the relay listener failed");
            }
        };
        let grpc = async move {
            let (unix, ()) = tokio::join!(unix, relay);
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
/// nobody was told about. Beside it, the preferences a paired phone may
/// reach on the same port, the pairing that gates both, and the codes a new
/// phone trades for its token.
pub(super) struct Relay {
    pub(super) rpc: RelayRpc,
    pub(super) port: u16,
    pub(super) preferences: PreferencesRpc,
    pub(super) pairing: Pairing,
    pub(super) codes: Arc<PairingCodes>,
    pub(super) instance: String,
}

/// A LAN half of the daemon: a router, and the socket it answers on.
type RelayServer = (RelayRouter, TcpListenerStream);

type RelayRouter = tonic::transport::server::Router;

/// What opening the LAN produced. Any part may be missing; the unix socket
/// serves regardless.
#[derive(Default)]
struct Lan {
    relay: Option<RelayServer>,
    advert: Option<Advert>,
    /// Present exactly when `relay` is: a code is worth showing only while a
    /// phone can redeem it. Unix socket only.
    invites: Option<InviteRpc>,
}

async fn open_relay(relay: Relay, health: &HealthReporter) -> Lan {
    let Relay {
        rpc,
        port,
        preferences,
        pairing,
        codes,
        instance,
    } = relay;
    // Its own reporter, not the daemon's. Sharing one would answer a LAN
    // client's health query for every service it cannot reach, which is a
    // free inventory of what this Mac runs. The daemon's reporter learns of
    // the relay too, so a local client can ask whether it opened.
    let (relay_health, _service) = tonic_health::server::health_reporter();
    relay_health
        .set_serving::<wire::relay_service_server::RelayServiceServer<RelayRpc>>()
        .await;
    let router = match relay_router(
        rpc,
        LanPreferences::new(preferences),
        Arc::clone(&codes),
        relay_health,
        &pairing,
    ) {
        Ok(router) => router,
        Err(error) => {
            tracing::error!(
                ?error,
                "the pairing certificate is unusable; the relay is off"
            );
            return Lan::default();
        }
    };
    let invites = InviteRpc::new(codes, instance, pairing.certificate_fingerprint);
    bind_relay(
        router,
        port,
        invites,
        &pairing.certificate_fingerprint,
        health,
    )
    .await
}

/// Binds the port and, only then, lets the unix socket issue codes.
async fn bind_relay(
    router: RelayRouter,
    port: u16,
    invites: InviteRpc,
    fingerprint: &[u8; 32],
    health: &HealthReporter,
) -> Lan {
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
    health
        .set_serving::<wire::relay_service_server::RelayServiceServer<RelayRpc>>()
        .await;

    // What the OS actually gave us. A configured port of 0 means an ephemeral
    // one, and advertising the request rather than the result would send
    // every phone to the wrong place.
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            tracing::error!(%error, "the relay port is unreadable; not advertising");
            return Lan {
                relay: Some((router, TcpListenerStream::new(listener))),
                advert: None,
                invites: Some(invites),
            };
        }
    };
    let advert = Advert::register(port, fingerprint)
        .inspect_err(|error| tracing::error!(%error, "the relay is not discoverable"))
        .ok();
    tracing::info!(port, "relay listening on the LAN");
    Lan {
        relay: Some((router, TcpListenerStream::new(listener))),
        advert,
        invites: Some(invites),
    }
}

/// What a phone reaches over TCP, all behind TLS: relay, settings, and
/// health behind a bearer check on each, and Pair, which takes a pairing
/// code instead. A stranger gets the certificate and `UNAUTHENTICATED`,
/// nothing more. The generic services let tests build it without a model.
fn relay_router<R, P>(
    relay: R,
    preferences: P,
    codes: Arc<PairingCodes>,
    health: HealthReporter,
    pairing: &Pairing,
) -> std::result::Result<RelayRouter, tonic::transport::Error>
where
    R: wire::relay_service_server::RelayService,
    P: wire::preferences_service_server::PreferencesService,
{
    let identity = Identity::from_pem(&pairing.certificate_pem, &pairing.key_pem);
    let bearer = require_bearer(pairing.token.clone());
    let pair = PairingRpc::new(codes, pairing.token.clone());
    Ok(tonic::transport::Server::builder()
        .tls_config(
            ServerTlsConfig::new()
                .identity(identity)
                .timeout(TLS_HANDSHAKE_TIMEOUT),
        )?
        // Even a paired client gets no more streams than a phone needs.
        .concurrency_limit_per_connection(MAX_RELAY_STREAMS)
        .max_concurrent_streams(Some(MAX_RELAY_STREAMS as u32))
        // A phone that walks out of range sends no FIN. Without keepalive the
        // connection sits open until something else times it out.
        .http2_keepalive_interval(Some(RELAY_KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(RELAY_KEEPALIVE_TIMEOUT))
        .add_service(InterceptedService::new(
            wire::relay_service_server::RelayServiceServer::new(relay)
                // Bound each message before the handler sees it. The upload
                // collector separately caps the total session payload.
                .max_decoding_message_size(MAX_RELAY_MESSAGE_BYTES),
            bearer.clone(),
        ))
        .add_service(InterceptedService::new(
            wire::preferences_service_server::PreferencesServiceServer::new(preferences)
                .max_decoding_message_size(MAX_PREFERENCES_MESSAGE_BYTES),
            bearer.clone(),
        ))
        .add_service(
            wire::pairing_service_server::PairingServiceServer::new(pair)
                .max_decoding_message_size(MAX_PAIR_MESSAGE_BYTES),
        )
        // The phone checks the Mac is up before it records, and this is the
        // check every gRPC client already has.
        .add_service(InterceptedService::new(
            HealthServer::new(HealthService::from_health_reporter(health)),
            bearer,
        )))
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

    /// Refuses every call, like [`SilentRelay`]: an `Aborted` from it proves
    /// the bearer check passed.
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

    /// The LAN port, from the outside: TLS with the Mac's certificate, the
    /// exact token in front of relay, settings, and health, and a one-time
    /// code in front of the token itself.
    #[tokio::test]
    async fn the_lan_listener_requires_tls_and_the_token() {
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
        let (health, _service) = tonic_health::server::health_reporter();
        let codes = Arc::new(PairingCodes::default());
        let stop = tokio_util::sync::CancellationToken::new();
        let serving = tokio::spawn(
            relay_router(
                SilentRelay,
                SilentPreferences,
                Arc::clone(&codes),
                health,
                &pairing,
            )
            .expect("the minted certificate should be usable")
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                stop.clone().cancelled_owned(),
            ),
        );

        // Plaintext gets no further than the handshake.
        let plaintext = Endpoint::from_shared(format!("http://{address}"))
            .expect("the endpoint should parse")
            .connect()
            .await;
        let refused = match plaintext {
            Err(_) => true,
            Ok(channel) => HealthClient::new(channel)
                .check(HealthCheckRequest::default())
                .await
                .is_err(),
        };
        assert!(refused, "a plaintext client must not be served");

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

        for header in [None, Some("Bearer 0000")] {
            let relayed = wire::relay_service_client::RelayServiceClient::new(channel.clone())
                .relay(with_header(tokio_stream::empty(), header))
                .await
                .expect_err("the relay must refuse a missing or wrong token");
            let preferences =
                wire::preferences_service_client::PreferencesServiceClient::new(channel.clone())
                    .get(with_header(wire::GetPreferencesRequest {}, header))
                    .await
                    .expect_err("preferences must refuse a missing or wrong token");
            let checked = HealthClient::new(channel.clone())
                .check(with_header(HealthCheckRequest::default(), header))
                .await
                .expect_err("health must refuse a missing or wrong token");
            for status in [relayed, preferences, checked] {
                assert_eq!(status.code(), tonic::Code::Unauthenticated, "{header:?}");
            }
        }

        let right = bearer(&pairing);
        let right = Some(right.as_str());
        let relayed = wire::relay_service_client::RelayServiceClient::new(channel.clone())
            .relay(with_header(tokio_stream::empty(), right))
            .await
            .expect_err("the stand-in relay refuses every stream");
        assert_eq!(
            relayed.code(),
            tonic::Code::Aborted,
            "the right token should reach the relay"
        );
        let preferences =
            wire::preferences_service_client::PreferencesServiceClient::new(channel.clone())
                .get(with_header(wire::GetPreferencesRequest {}, right))
                .await
                .expect_err("the stand-in preferences refuse every call");
        assert_eq!(
            preferences.code(),
            tonic::Code::Aborted,
            "the right token should reach preferences"
        );
        let checked = HealthClient::new(channel.clone())
            .check(with_header(HealthCheckRequest::default(), right))
            .await
            .expect("the right token should reach health");
        assert_eq!(checked.into_inner().status(), ServingStatus::Serving);

        // Pair takes no bearer header: the code is the credential.
        let pairing_client =
            wire::pairing_service_client::PairingServiceClient::new(channel.clone());
        let pair = |code: &str| {
            let mut client = pairing_client.clone();
            let request = wire::PairRequest {
                code: code.to_owned(),
            };
            async move { client.pair(request).await }
        };
        let (code, _) = codes
            .issue(Instant::now())
            .expect("a pairing code should mint");
        let wrong = pair(&"0".repeat(32))
            .await
            .expect_err("a wrong code must not pair");
        assert_eq!(wrong.code(), tonic::Code::Unauthenticated);
        assert_eq!(
            crate::ipc::protocol::rpc_failure(&wrong)
                .expect("the refusal should carry an RpcFailure")
                .code,
            crate::rpc::RpcErrorCode::Unauthenticated
        );
        let paired = pair(code.raw())
            .await
            .expect("the live code should pair without a bearer header");
        assert_eq!(paired.into_inner().token, pairing.token.raw());
        let reused = pair(code.raw())
            .await
            .expect_err("a used code must not pair again");
        assert_eq!(reused.code(), tonic::Code::Unauthenticated);

        // Registered only on the unix router, so the LAN never sees it, token
        // or not.
        let history = wire::history_service_client::HistoryServiceClient::new(channel.clone())
            .list(with_header(
                wire::ListHistoryRequest {
                    query: String::new(),
                    before_id: None,
                    limit: 10,
                },
                right,
            ))
            .await
            .expect_err("history must not be reachable over TCP");
        let issued = wire::invite_service_client::InviteServiceClient::new(channel.clone())
            .issue_invite(with_header(wire::IssueInviteRequest {}, right))
            .await
            .expect_err("issuing codes must not be reachable over TCP");
        // The likeliest regression, and the worst: reflection would hand a
        // stranger the shape of every service the unix socket carries.
        let reflection =
            tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient::new(
                channel,
            )
            .server_reflection_info(with_header(tokio_stream::empty(), right))
            .await
            .expect_err("reflection must not be reachable over TCP");
        for status in [history, issued, reflection] {
            assert_eq!(status.code(), tonic::Code::Unimplemented);
        }

        stop.cancel();
        serving
            .await
            .expect("the serve task should join")
            .expect("the LAN listener should stop cleanly");
    }

    /// A taken port leaves the relay off, and with it the invite service: a
    /// code issued then would be one no phone could redeem.
    #[tokio::test]
    async fn a_taken_port_leaves_invites_unregistered() {
        let directory = tempfile::tempdir().expect("a temporary directory should be created");
        let pairing = super::super::relay::load_pairing(
            &directory.path().join("pairing.json"),
            "Aria (test)",
        )
        .expect("pairing material should mint");
        let taken = tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
            .await
            .expect("an ephemeral port should bind");
        let port = taken
            .local_addr()
            .expect("the port should be readable")
            .port();
        let (relay_health, _service) = tonic_health::server::health_reporter();
        let (health, _service) = tonic_health::server::health_reporter();
        let codes = Arc::new(PairingCodes::default());
        let router = relay_router(
            SilentRelay,
            SilentPreferences,
            Arc::clone(&codes),
            relay_health,
            &pairing,
        )
        .expect("the minted certificate should be usable");
        let invites = InviteRpc::new(
            codes,
            "Aria (test)".to_owned(),
            pairing.certificate_fingerprint,
        );

        let lan = bind_relay(
            router,
            port,
            invites,
            &pairing.certificate_fingerprint,
            &health,
        )
        .await;

        assert!(lan.relay.is_none(), "a taken port must not serve a relay");
        assert!(lan.advert.is_none());
        assert!(lan.invites.is_none(), "no relay, no codes to issue for it");
    }

    fn bearer(pairing: &Pairing) -> String {
        // The test needs the raw value the phone would send; the type hides
        // it everywhere else on purpose.
        format!("Bearer {}", pairing.token.raw())
    }

    fn with_header<T>(message: T, authorization: Option<&str>) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        if let Some(value) = authorization {
            request.metadata_mut().insert(
                "authorization",
                value.parse().expect("the header should parse"),
            );
        }
        request
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
