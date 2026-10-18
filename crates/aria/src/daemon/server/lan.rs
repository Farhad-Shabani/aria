mod connections;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Identity, ServerTlsConfig};
use tonic_health::pb::health_server::HealthServer;
use tonic_health::server::{HealthReporter, HealthService};

use crate::daemon::relay::{Advert, LanPreferences, Pairing, PairingCodes, require_bearer};
use crate::daemon::service::{InviteRpc, PairingRpc, PreferencesRpc, RelayRpc};
use crate::ipc::wire;

/// LAN connections open at once: a few phones, plus dead ones the keepalive
/// has not reaped yet.
const MAX_LAN_CONNECTIONS: usize = 16;
/// How many streams may run on one LAN connection. Only one relay can hold
/// the recorder. The rest is slack for a settings or health call, or a
/// retry.
const MAX_RELAY_STREAMS: usize = 4;

/// The largest `RelayRequest` the relay will decode. The phone ships 256 KiB
/// chunks; this is generous for one of those and nowhere near a session.
const MAX_RELAY_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
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
/// How long LAN connections may drain once the daemon stops: a second
/// inside the daemon's own grace.
const LAN_DRAIN_GRACE: Duration =
    super::SHUTDOWN_GRACE_PERIOD.saturating_sub(Duration::from_secs(1));

/// The relay and the port it answers on, together because "served" and
/// "where" are the same decision: carried apart, a mismatch binds a port
/// nobody was told about. Beside it, the preferences a paired phone may
/// reach on the same port, the pairing that gates both, and the codes a new
/// phone trades for its token.
pub(in crate::daemon) struct Relay {
    pub(in crate::daemon) rpc: RelayRpc,
    pub(in crate::daemon) port: u16,
    pub(in crate::daemon) preferences: PreferencesRpc,
    pub(in crate::daemon) pairing: Pairing,
    pub(in crate::daemon) codes: Arc<PairingCodes>,
    pub(in crate::daemon) instance: String,
}

/// The LAN router and the socket it will serve.
type Bound = (RelayRouter, tokio::net::TcpListener);

type RelayRouter = tonic::transport::server::Router;

/// What opening the LAN produced. Any part may be missing; the unix socket
/// serves regardless.
#[derive(Default)]
pub(super) struct Opened {
    pub(super) bound: Option<Bound>,
    pub(super) advert: Option<Advert>,
    /// Present exactly when `bound` is: a code is worth showing only while a
    /// phone can redeem it. Unix socket only.
    pub(super) invites: Option<InviteRpc>,
}

pub(super) async fn open(relay: Relay, health: &HealthReporter) -> Opened {
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
    let router = match router(
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
            return Opened::default();
        }
    };
    let invites = InviteRpc::new(codes, instance, pairing.certificate_fingerprint);
    bind(
        router,
        port,
        invites,
        &pairing.certificate_fingerprint,
        health,
    )
    .await
}

/// What a phone reaches over TCP, all behind TLS: relay, settings, and
/// health behind a bearer check on each, and Pair, which takes a pairing
/// code instead. A stranger gets the certificate and `UNAUTHENTICATED`,
/// nothing more. The generic services let tests build it without a model.
fn router<R, P>(
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

/// Binds the port and, only then, lets the unix socket issue codes.
async fn bind(
    router: RelayRouter,
    port: u16,
    invites: InviteRpc,
    fingerprint: &[u8; 32],
    health: &HealthReporter,
) -> Opened {
    let listener = match tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        .await
    {
        Ok(listener) => listener,
        Err(error) => {
            // Not fatal by design: the daemon still serves every local client
            // over its socket. Only the phone loses its way in.
            tracing::error!(%error, port, "failed to bind the relay port; serving locally only");
            return Opened::default();
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
            return Opened {
                bound: Some((router, listener)),
                advert: None,
                invites: Some(invites),
            };
        }
    };
    let advert = Advert::register(port, fingerprint)
        .inspect_err(|error| tracing::error!(%error, "the relay is not discoverable"))
        .ok();
    tracing::info!(port, "relay listening on the LAN");
    Opened {
        bound: Some((router, listener)),
        advert,
        invites: Some(invites),
    }
}

/// Ends at most `LAN_DRAIN_GRACE` after `stop`.
pub(super) async fn serve(bound: Option<Bound>, stop: CancellationToken) {
    let Some((router, listener)) = bound else {
        return;
    };
    let serving = router.serve_with_incoming_shutdown(
        connections::admitted(listener, MAX_LAN_CONNECTIONS),
        stop.clone().cancelled_owned(),
    );
    match until_grace_after(serving, stop.cancelled_owned(), LAN_DRAIN_GRACE).await {
        Some(Ok(())) => {}
        Some(Err(error)) => tracing::error!(%error, "the relay listener failed"),
        None => tracing::warn!("LAN connections outlived the drain; they close with the daemon"),
    }
}

/// `work`'s output, or `None` once `stopped` resolves and `grace` passes
/// first.
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tokio_stream::Stream;
    use tonic::Status;
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_client::HealthClient;

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
    async fn the_lan_listener_requires_tls_and_the_token() {
        let directory = tempfile::tempdir().expect("a temporary directory should be created");
        let pairing = crate::daemon::relay::load_pairing(
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
            router(
                SilentRelay,
                SilentPreferences,
                Arc::clone(&codes),
                health,
                &pairing,
            )
            .expect("the minted certificate should be usable")
            .serve_with_incoming_shutdown(
                connections::admitted(listener, MAX_LAN_CONNECTIONS),
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
        let pairing = crate::daemon::relay::load_pairing(
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
        let router = router(
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

        let lan = bind(
            router,
            port,
            invites,
            &pairing.certificate_fingerprint,
            &health,
        )
        .await;

        assert!(lan.bound.is_none(), "a taken port must not serve a relay");
        assert!(lan.advert.is_none());
        assert!(lan.invites.is_none(), "no relay, no codes to issue for it");
    }

    #[tokio::test(start_paused = true)]
    async fn lan_work_that_never_ends_ends_a_grace_after_the_stop() {
        let stop = tokio_util::sync::CancellationToken::new();
        let waiting = tokio::spawn(until_grace_after(
            std::future::pending::<()>(),
            stop.clone().cancelled_owned(),
            LAN_DRAIN_GRACE,
        ));

        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(!waiting.is_finished(), "nothing ends it before the stop");
        let stopped_at = tokio::time::Instant::now();
        stop.cancel();

        assert_eq!(waiting.await.expect("the wait does not panic"), None);
        assert_eq!(stopped_at.elapsed(), LAN_DRAIN_GRACE);
    }
}
