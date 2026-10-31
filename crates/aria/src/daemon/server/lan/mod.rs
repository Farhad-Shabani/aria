pub(in crate::daemon) mod connections;
pub(in crate::daemon) mod tls;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_rustls::server::TlsStream;
use tokio_stream::Stream;
use tokio_util::sync::CancellationToken;
use tonic::service::interceptor::InterceptedService;
use tonic_health::pb::health_server::HealthServer;
use tonic_health::server::{HealthReporter, HealthService};

use crate::daemon::relay::{Advert, LanSettings, Pairing, PairingCodes, Phones, require_paired};
use crate::daemon::service::{InviteRpc, PairingRpc, RelayRpc, SettingsRpc};
use crate::ipc::wire;
use crate::primitives::error_chain;
use connections::Admitted;

use super::{CONNECTION_DRAIN_GRACE, until_grace_after};

/// LAN connections open at once: a few devices, plus dead ones the keepalive
/// has not reaped yet.
pub(in crate::daemon) const MAX_LAN_CONNECTIONS: usize = 16;
/// How many streams may run on one LAN connection. One call at a time holds
/// the recorder or the model. The rest is slack for a settings, info or
/// health call, or a retry.
pub(in crate::daemon) const MAX_LAN_STREAMS: usize = 4;

/// The largest `RelayRequest` the relay will decode. The phone ships 256 KiB
/// chunks; this is generous for one of those and nowhere near a session.
const MAX_RELAY_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
/// A settings patch is a few hundred bytes; this leaves room for a long
/// vocabulary path and nothing else.
const MAX_SETTINGS_MESSAGE_BYTES: usize = 64 * 1024;
/// A pairing request carries one 32-character code, and any phone on the
/// LAN can send one while a code is live.
const MAX_PAIR_MESSAGE_BYTES: usize = 1024;

/// A phone on the LAN finishes its handshake in well under a second. A
/// connection that has not finished by then holds a socket for nothing.
pub(in crate::daemon) const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a connection admitted on a pairing code stays open. Pair is one
/// short call, and the phone opens a fresh connection for everything after.
pub(in crate::daemon) const PAIRING_LEASE: Duration = Duration::from_secs(15);
pub(in crate::daemon) const LAN_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
pub(in crate::daemon) const LAN_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// The relay and the port it answers on, together because "served" and
/// "where" are the same decision: carried apart, a mismatch binds a port
/// nobody was told about.
pub(in crate::daemon) struct Relay {
    pub(in crate::daemon) rpc: RelayRpc,
    pub(in crate::daemon) port: u16,
    pub(in crate::daemon) settings: SettingsRpc,
    pub(in crate::daemon) pairing: Pairing,
    pub(in crate::daemon) codes: Arc<PairingCodes>,
    pub(in crate::daemon) instance: String,
}

pub(super) struct Bound {
    router: RelayRouter,
    handshake: tls::Handshake,
    listener: TcpListener,
}

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
        settings,
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
    let handshake = match tls::Handshake::new(&pairing, Arc::clone(&codes)) {
        Ok(handshake) => handshake,
        Err(error) => {
            tracing::error!(
                error = %error_chain(&error),
                "the pairing certificate is unusable; the relay is off"
            );
            return Opened::default();
        }
    };
    let router = router(
        rpc,
        LanSettings::new(settings),
        Arc::clone(&codes),
        relay_health,
        Arc::clone(&pairing.phones),
    );
    let invites = InviteRpc::new(codes, instance, pairing.certificate_fingerprint);
    bind(
        router,
        handshake,
        port,
        invites,
        &pairing.certificate_fingerprint,
        health,
    )
    .await
}

/// What a phone reaches over TCP, all behind TLS: relay, settings, and
/// health for a paired phone, and Pair, which takes a pairing code instead.
/// The generic services let tests build it without a model.
fn router<R, P>(
    relay: R,
    settings: P,
    codes: Arc<PairingCodes>,
    health: HealthReporter,
    phones: Arc<Phones>,
) -> RelayRouter
where
    R: wire::relay_service_server::RelayService,
    P: wire::settings_service_server::SettingsService,
{
    let paired = require_paired(Arc::clone(&phones));
    let pair = PairingRpc::new(codes, phones);
    tonic::transport::Server::builder()
        // Even a paired client gets no more streams than a phone needs.
        .concurrency_limit_per_connection(MAX_LAN_STREAMS)
        .max_concurrent_streams(Some(MAX_LAN_STREAMS as u32))
        // A phone that walks out of range sends no FIN. Without keepalive the
        // connection sits open until something else times it out.
        .http2_keepalive_interval(Some(LAN_KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(LAN_KEEPALIVE_TIMEOUT))
        .add_service(InterceptedService::new(
            wire::relay_service_server::RelayServiceServer::new(relay)
                // Bound each message before the handler sees it. The upload
                // collector separately caps the total session payload.
                .max_decoding_message_size(MAX_RELAY_MESSAGE_BYTES),
            paired.clone(),
        ))
        .add_service(InterceptedService::new(
            wire::settings_service_server::SettingsServiceServer::new(settings)
                .max_decoding_message_size(MAX_SETTINGS_MESSAGE_BYTES),
            paired.clone(),
        ))
        .add_service(
            wire::pairing_service_server::PairingServiceServer::new(pair)
                .max_decoding_message_size(MAX_PAIR_MESSAGE_BYTES),
        )
        // The phone checks the Mac is up before it records, and this is the
        // check every gRPC client already has.
        .add_service(InterceptedService::new(
            HealthServer::new(HealthService::from_health_reporter(health)),
            paired,
        ))
}

/// Binds the port and, only then, lets the unix socket issue codes.
async fn bind(
    router: RelayRouter,
    handshake: tls::Handshake,
    port: u16,
    invites: InviteRpc,
    fingerprint: &[u8; 32],
    health: &HealthReporter,
) -> Opened {
    let listener = match TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).await {
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
                bound: Some(Bound {
                    router,
                    handshake,
                    listener,
                }),
                advert: None,
                invites: Some(invites),
            };
        }
    };
    let advert = Advert::register(port, fingerprint)
        .inspect_err(|error| {
            tracing::error!(error = %error_chain(error), "the relay is not discoverable");
        })
        .ok();
    tracing::info!(port, "relay listening on the LAN");
    Opened {
        bound: Some(Bound {
            router,
            handshake,
            listener,
        }),
        advert,
        invites: Some(invites),
    }
}

/// Ends at most `CONNECTION_DRAIN_GRACE` after `stop`.
pub(super) async fn serve(bound: Option<Bound>, stop: CancellationToken) {
    let Some(Bound {
        router,
        handshake,
        listener,
    }) = bound
    else {
        return;
    };
    let serving = router.serve_with_incoming_shutdown(
        incoming(listener, handshake),
        stop.clone().cancelled_owned(),
    );
    match until_grace_after(serving, stop.cancelled_owned(), CONNECTION_DRAIN_GRACE).await {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            tracing::error!(error = %error_chain(&error), "the relay listener failed");
        }
        None => tracing::warn!("LAN connections outlived the drain; they close with the daemon"),
    }
}

fn incoming(
    listener: TcpListener,
    handshake: tls::Handshake,
) -> impl Stream<Item = io::Result<tls::Leased<TlsStream<Admitted>>>> {
    handshake.secured(
        connections::admitted(listener, MAX_LAN_CONNECTIONS),
        TLS_HANDSHAKE_TIMEOUT,
        PAIRING_LEASE,
        MAX_LAN_CONNECTIONS,
    )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;
    use tokio_rustls::rustls::{AlertDescription, ClientConfig, HandshakeKind, RootCertStore};
    use tokio_rustls::{TlsConnector, client, rustls};
    use tokio_stream::StreamExt;
    use tonic::Status;
    use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
    use tonic_health::pb::HealthCheckRequest;
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
    /// the pairing check passed.
    struct SilentSettings;

    #[tonic::async_trait]
    impl wire::settings_service_server::SettingsService for SilentSettings {
        async fn get(
            &self,
            _request: tonic::Request<wire::GetSettingsRequest>,
        ) -> std::result::Result<tonic::Response<wire::SettingsSnapshot>, Status> {
            Err(Status::aborted("the stand-in settings were reached"))
        }

        async fn update(
            &self,
            _request: tonic::Request<wire::UpdateSettingsRequest>,
        ) -> std::result::Result<tonic::Response<wire::UpdateSettingsResponse>, Status> {
            Err(Status::aborted("the stand-in settings were reached"))
        }

        async fn get_status(
            &self,
            _request: tonic::Request<wire::GetSettingsStatusRequest>,
        ) -> std::result::Result<tonic::Response<wire::GetSettingsStatusResponse>, Status> {
            Err(Status::aborted("the stand-in settings were reached"))
        }
    }

    /// Real sockets keep tokio's paused clock from advancing, so the
    /// timeout and lease tests run on the wall clock with short bounds.
    const SHORT: Duration = Duration::from_millis(200);

    /// A self-signed client certificate, the kind the phone app mints.
    struct Phone {
        certificate_pem: String,
        key_pem: String,
        fingerprint: [u8; 32],
    }

    impl Phone {
        fn mint() -> Self {
            let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = rcgen::IsCa::ExplicitNoCa;
            params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
            let key = rcgen::KeyPair::generate().unwrap();
            let certificate = params.self_signed(&key).unwrap();
            Self {
                certificate_pem: certificate.pem(),
                key_pem: key.serialize_pem(),
                fingerprint: crate::daemon::relay::fingerprint(certificate.der()),
            }
        }

        fn identity(&self) -> Identity {
            Identity::from_pem(&self.certificate_pem, &self.key_pem)
        }
    }

    /// The LAN port on loopback: stand-in services behind the real TLS and
    /// pairing checks.
    struct Lan {
        address: std::net::SocketAddr,
        pairing: Pairing,
        codes: Arc<PairingCodes>,
        stop: CancellationToken,
        serving: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
        _directory: tempfile::TempDir,
    }

    fn material() -> (tempfile::TempDir, Pairing, Arc<PairingCodes>) {
        let directory = tempfile::tempdir().unwrap();
        let pairing = crate::daemon::relay::load_pairing(
            &directory.path().join("pairing.json"),
            "Aria (test)",
        )
        .unwrap();
        (directory, pairing, Arc::new(PairingCodes::default()))
    }

    async fn loopback() -> (TcpListener, std::net::SocketAddr) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        (listener, address)
    }

    fn client(pairing: &Pairing, phone: Option<&Phone>) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(pairing.certificate_pem.as_bytes()).unwrap())
            .unwrap();
        let builder = ClientConfig::builder().with_root_certificates(roots);
        let config = match phone {
            Some(phone) => builder
                .with_client_auth_cert(
                    vec![CertificateDer::from_pem_slice(phone.certificate_pem.as_bytes()).unwrap()],
                    PrivateKeyDer::from_pem_slice(phone.key_pem.as_bytes()).unwrap(),
                )
                .unwrap(),
            None => builder.with_no_client_auth(),
        };
        Arc::new(config)
    }

    /// Shakes hands with `address` and waits for the Mac's verdict. In TLS
    /// 1.3 the client finishes first, so a refusal arrives as an alert on
    /// the first read.
    async fn handshake(
        address: std::net::SocketAddr,
        client: Arc<ClientConfig>,
    ) -> io::Result<client::TlsStream<TcpStream>> {
        let tcp = TcpStream::connect(address).await?;
        let server = ServerName::try_from("aria.local").unwrap();
        let mut stream = TlsConnector::from(client).connect(server, tcp).await?;
        let mut byte = [0u8; 1];
        match tokio::time::timeout(Duration::from_millis(200), stream.read(&mut byte)).await {
            Ok(Ok(0)) => Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(Ok(_)) => Ok(stream),
            Ok(Err(error)) => Err(error),
            // Accepted: the Mac waits for the HTTP/2 preface.
            Err(_elapsed) => Ok(stream),
        }
    }

    impl Lan {
        async fn start() -> Self {
            let (directory, pairing, codes) = material();
            let (listener, address) = loopback().await;
            let (health, _service) = tonic_health::server::health_reporter();
            let handshake = tls::Handshake::new(&pairing, Arc::clone(&codes)).unwrap();
            let stop = CancellationToken::new();
            let serving = tokio::spawn(
                router(
                    SilentRelay,
                    SilentSettings,
                    Arc::clone(&codes),
                    health,
                    Arc::clone(&pairing.phones),
                )
                .serve_with_incoming_shutdown(
                    incoming(listener, handshake),
                    stop.clone().cancelled_owned(),
                ),
            );
            Self {
                address,
                pairing,
                codes,
                stop,
                serving,
                _directory: directory,
            }
        }

        /// The alert a refused handshake ends in.
        async fn refusal(&self, phone: Option<&Phone>) -> Option<AlertDescription> {
            let error = handshake(self.address, client(&self.pairing, phone))
                .await
                .err()?;
            match error.get_ref()?.downcast_ref::<rustls::Error>()? {
                rustls::Error::AlertReceived(alert) => Some(*alert),
                _ => None,
            }
        }

        async fn connect(&self, phone: &Phone) -> Channel {
            let tls = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(&self.pairing.certificate_pem))
                .domain_name("aria.local")
                .identity(phone.identity());
            Endpoint::from_shared(format!("https://{}", self.address))
                .unwrap()
                .tls_config(tls)
                .unwrap()
                .connect()
                .await
                .unwrap()
        }

        async fn stop(self) {
            self.stop.cancel();
            self.serving.await.unwrap().unwrap();
        }
    }

    async fn health(channel: &Channel) -> Result<(), Status> {
        HealthClient::new(channel.clone())
            .check(HealthCheckRequest::default())
            .await
            .map(drop)
    }

    async fn relay(channel: &Channel) -> tonic::Code {
        wire::relay_service_client::RelayServiceClient::new(channel.clone())
            .relay(tokio_stream::empty())
            .await
            .expect_err("the stand-in relay refuses every stream")
            .code()
    }

    async fn settings(channel: &Channel) -> tonic::Code {
        wire::settings_service_client::SettingsServiceClient::new(channel.clone())
            .get(wire::GetSettingsRequest {})
            .await
            .expect_err("the stand-in settings refuse every call")
            .code()
    }

    async fn pair(channel: &Channel, code: &str) -> Result<(), Status> {
        wire::pairing_service_client::PairingServiceClient::new(channel.clone())
            .pair(wire::PairRequest {
                code: code.to_owned(),
            })
            .await
            .map(drop)
    }

    /// Whether `path` reaches a service, by the one status a router gives a
    /// path it does not carry. Every call sends an empty message, which each
    /// method decodes as its default request.
    async fn served(channel: &Channel, path: &'static str) -> bool {
        let mut grpc = tonic::client::Grpc::new(channel.clone());
        grpc.ready().await.expect("the channel is ready");
        let call = grpc
            .unary::<(), (), _>(
                tonic::Request::new(()),
                tonic::codegen::http::uri::PathAndQuery::from_static(path),
                tonic_prost::ProstCodec::default(),
            )
            .await;
        !matches!(call, Err(status) if status.code() == tonic::Code::Unimplemented)
    }

    /// The LAN serves exactly Relay, scoped Settings, Pairing and Health.
    /// Nothing that records or speaks, and no reflection: one method from
    /// every service the daemon has.
    #[tokio::test]
    async fn the_lan_serves_exactly_four_services() {
        let lan = Lan::start().await;
        let phone = Phone::mint();
        lan.pairing.phones.admit(phone.fingerprint).unwrap();
        let channel = lan.connect(&phone).await;

        let mut reached = Vec::new();
        for path in [
            "/aria.daemon.v1.RelayService/Relay",
            "/aria.daemon.v1.SettingsService/Get",
            "/aria.daemon.v1.PairingService/Pair",
            "/grpc.health.v1.Health/Check",
            "/aria.daemon.v1.ChatService/Ask",
            "/aria.daemon.v1.DictationService/Stop",
            "/aria.daemon.v1.EventsService/Subscribe",
            "/aria.daemon.v1.HistoryService/List",
            "/aria.daemon.v1.InferenceService/GetInfo",
            "/aria.daemon.v1.InferenceAdminService/IssueInvite",
            "/aria.daemon.v1.InviteService/IssueInvite",
            "/aria.daemon.v1.ListenService/GetStatus",
            "/aria.daemon.v1.SpeakerService/List",
            "/aria.daemon.v1.SpeechService/Stop",
            "/aria.daemon.v1.VocabularyService/GetStatus",
            "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
        ] {
            if served(&channel, path).await {
                reached.push(path);
            }
        }

        assert_eq!(
            reached,
            [
                "/aria.daemon.v1.RelayService/Relay",
                "/aria.daemon.v1.SettingsService/Get",
                "/aria.daemon.v1.PairingService/Pair",
                "/grpc.health.v1.Health/Check",
            ]
        );
        lan.stop().await;
    }

    #[tokio::test]
    async fn a_phone_reaches_the_lan_only_once_paired() {
        let lan = Lan::start().await;
        let phone = Phone::mint();

        let plaintext = Endpoint::from_shared(format!("http://{}", lan.address))
            .unwrap()
            .connect()
            .await;
        let refused = match plaintext {
            Err(_) => true,
            Ok(channel) => health(&channel).await.is_err(),
        };
        assert!(refused, "a plaintext client must not be served");
        assert_eq!(
            lan.refusal(None).await,
            Some(AlertDescription::CertificateRequired),
            "no certificate, no handshake"
        );
        assert_eq!(
            lan.refusal(Some(&phone)).await,
            Some(AlertDescription::AccessDenied),
            "an unpaired phone gets no handshake while no code is live"
        );

        let (code, _) = lan.codes.issue(Instant::now()).unwrap();
        let channel = lan.connect(&phone).await;
        let unpaired = health(&channel)
            .await
            .expect_err("health is for paired phones");
        assert_eq!(unpaired.code(), tonic::Code::Unauthenticated);
        assert_eq!(
            crate::ipc::protocol::rpc_failure(&unpaired).unwrap().code,
            crate::rpc::RpcErrorCode::Unauthenticated
        );
        assert_eq!(relay(&channel).await, tonic::Code::Unauthenticated);
        assert_eq!(settings(&channel).await, tonic::Code::Unauthenticated);

        let wrong = pair(&channel, &"0".repeat(32))
            .await
            .expect_err("a wrong code must not pair");
        assert_eq!(
            crate::ipc::protocol::rpc_failure(&wrong).unwrap().code,
            crate::rpc::RpcErrorCode::Unauthenticated
        );
        pair(&channel, code.raw())
            .await
            .expect("the live code should pair");
        assert_eq!(relay(&channel).await, tonic::Code::Aborted);
        assert_eq!(settings(&channel).await, tonic::Code::Aborted);
        let reused = pair(&channel, code.raw())
            .await
            .expect_err("a used code must not pair again");
        assert_eq!(reused.code(), tonic::Code::Unauthenticated);

        health(&lan.connect(&phone).await)
            .await
            .expect("a paired phone reaches health on a fresh connection");
        assert_eq!(
            lan.refusal(Some(&Phone::mint())).await,
            Some(AlertDescription::AccessDenied),
            "pairing one phone opens nothing for the next"
        );
        lan.stop().await;
    }

    /// A resumed session would skip the verifier.
    #[tokio::test]
    async fn every_handshake_is_a_full_one() {
        let lan = Lan::start().await;
        let phone = Phone::mint();
        lan.pairing.phones.admit(phone.fingerprint).unwrap();
        let client = client(&lan.pairing, Some(&phone));

        for _ in 0..2 {
            let stream = handshake(lan.address, Arc::clone(&client)).await.unwrap();
            assert_eq!(
                stream.get_ref().1.handshake_kind(),
                Some(HandshakeKind::Full)
            );
        }
        lan.stop().await;
    }

    /// A silent TCP client holds its slot only until the handshake times out.
    #[tokio::test]
    async fn a_stalled_handshake_frees_its_slot_at_the_timeout() {
        let (_directory, pairing, codes) = material();
        let phone = Phone::mint();
        pairing.phones.admit(phone.fingerprint).unwrap();
        let (listener, address) = loopback().await;
        let mut incoming = Box::pin(tls::Handshake::new(&pairing, codes).unwrap().secured(
            connections::admitted(listener, 1),
            SHORT,
            SHORT,
            1,
        ));

        let mut silent = TcpStream::connect(address).await.unwrap();
        let mut byte = [0u8; 1];
        tokio::select! {
            _ = incoming.next() => panic!("a silent client shook no hands"),
            read = silent.read(&mut byte) => assert_eq!(read.unwrap(), 0, "closed at the timeout"),
        }

        let shaking = tokio::spawn(handshake(address, client(&pairing, Some(&phone))));
        let _admitted = incoming
            .next()
            .await
            .unwrap()
            .expect("the freed slot admits the next phone");
        shaking.await.unwrap().unwrap();
    }

    /// A connection admitted on a code closes at its lease; a paired one stays.
    #[tokio::test]
    async fn a_connection_admitted_on_a_code_closes_when_its_lease_ends() {
        let (_directory, pairing, codes) = material();
        let (listener, address) = loopback().await;
        let paired = Phone::mint();
        pairing.phones.admit(paired.fingerprint).unwrap();
        codes.issue(Instant::now()).unwrap();
        let mut incoming = Box::pin(tls::Handshake::new(&pairing, codes).unwrap().secured(
            connections::admitted(listener, 1),
            SHORT,
            SHORT,
            1,
        ));
        let mut byte = [0u8; 1];

        let shaking = tokio::spawn(handshake(address, client(&pairing, Some(&Phone::mint()))));
        let mut stranger = incoming.next().await.unwrap().unwrap();
        let _stranger_client = shaking.await.unwrap().unwrap();
        let closed = stranger
            .read(&mut byte)
            .await
            .expect_err("the lease ends it");
        assert_eq!(closed.kind(), io::ErrorKind::TimedOut);
        drop(stranger);

        let shaking = tokio::spawn(handshake(address, client(&pairing, Some(&paired))));
        let mut phone = incoming.next().await.unwrap().unwrap();
        let _phone_client = shaking.await.unwrap().unwrap();
        let open = tokio::time::timeout(SHORT * 2, phone.read(&mut byte)).await;
        assert!(open.is_err(), "a paired phone's connection has no lease");
    }

    #[tokio::test]
    async fn a_paired_phone_reaches_nothing_from_the_unix_socket() {
        let lan = Lan::start().await;
        let phone = Phone::mint();
        lan.pairing.phones.admit(phone.fingerprint).unwrap();
        let channel = lan.connect(&phone).await;

        let history = wire::history_service_client::HistoryServiceClient::new(channel.clone())
            .list(wire::ListHistoryRequest {
                query: String::new(),
                before_id: None,
                limit: 10,
            })
            .await
            .expect_err("history must not be reachable over TCP");
        let issued = wire::invite_service_client::InviteServiceClient::new(channel.clone())
            .issue_invite(wire::IssueInviteRequest {})
            .await
            .expect_err("issuing codes must not be reachable over TCP");
        // The likeliest regression, and the worst: reflection would hand a
        // stranger the shape of every service the unix socket carries.
        let reflection =
            tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient::new(
                channel,
            )
            .server_reflection_info(tokio_stream::empty())
            .await
            .expect_err("reflection must not be reachable over TCP");
        for status in [history, issued, reflection] {
            assert_eq!(status.code(), tonic::Code::Unimplemented);
        }
        lan.stop().await;
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
        let taken = TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
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
            SilentSettings,
            Arc::clone(&codes),
            relay_health,
            Arc::clone(&pairing.phones),
        );
        let handshake = tls::Handshake::new(&pairing, Arc::clone(&codes))
            .expect("the minted certificate should be usable");
        let invites = InviteRpc::new(
            codes,
            "Aria (test)".to_owned(),
            pairing.certificate_fingerprint,
        );

        let lan = bind(
            router,
            handshake,
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
}
