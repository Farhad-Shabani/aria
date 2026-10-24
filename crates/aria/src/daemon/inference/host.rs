use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aria_stt::EngineConfig;
use futures_util::Stream;
use tokio::net::{TcpListener, UnixListener};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tonic::service::interceptor::InterceptedService;

use crate::audio::remove_all_spools;
use crate::config::{ConfigError, Environment, TranscriptionBackend};
use crate::daemon::server::lan::connections::{self, Admitted};
use crate::daemon::server::lan::tls::{Lease, Leased, secured};
use crate::daemon::server::lan::{
    LAN_KEEPALIVE_INTERVAL, LAN_KEEPALIVE_TIMEOUT, MAX_LAN_CONNECTIONS, MAX_LAN_STREAMS,
    PAIRING_LEASE, TLS_HANDSHAKE_TIMEOUT,
};
use crate::daemon::server::local::same_user;
use crate::daemon::service::{
    InferenceAdminRpc, InferencePairingRpc, InferenceRpc, authenticate_inference as authenticate,
};
use crate::daemon::shutdown::{ShutdownError, ShutdownSignal};
use crate::ipc::wire;
use crate::primitives::cancel::CancelToken;
use crate::primitives::local_socket::{ExclusiveListener, LocalSocketError};
use crate::runtime::RuntimeError;
use crate::transcription::TranscriptionError;

use super::engine::Engine;
use super::identity::{self, ServerAccess};
use super::{Paths, tls};
use crate::inference::{MAX_AUDIO_MESSAGE_BYTES, MAX_RESPONSE_BYTES};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const CONNECTION_DRAIN: Duration = Duration::from_secs(4);
const MAX_WIRE_MESSAGE: usize = MAX_AUDIO_MESSAGE_BYTES + 1024;

pub fn serve_inference(bind: SocketAddr) -> Result<(), InferenceHostError> {
    run(bind).map_err(InferenceHostError)
}

fn run(bind: SocketAddr) -> Result<(), HostFailure> {
    let shutdown = ShutdownSignal::install()?;
    let environment = Environment::capture()?;
    let config = match TranscriptionBackend::load(&environment)? {
        TranscriptionBackend::Local(engine) => EngineConfig::try_from(engine)?,
        TranscriptionBackend::Server { .. } => {
            return Err(HostFailure::Model(TranscriptionError::UnsupportedBackend {
                backend: crate::config::backend::SERVER,
            }));
        }
    };
    let inference = Paths::in_home(environment.home());
    let paths = inference.runtime().prepare()?;
    let ownership = Arc::new(ExclusiveListener::bind(
        paths.socket_path(),
        paths.lock_path(),
    )?);
    remove_all_spools(paths.spool_dir());
    let access = ServerAccess::load_or_create(&inference.server_identity())?;
    let acceptor = tls::server(&access)?;
    let listener = std::net::TcpListener::bind(bind).map_err(HostFailure::Io)?;
    listener.set_nonblocking(true).map_err(HostFailure::Io)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("aria-inference")
        .build()
        .map_err(HostFailure::Io)?;
    let result = runtime.block_on(run_prepared(
        config,
        paths.spool_dir().to_owned(),
        ownership,
        access,
        acceptor,
        listener,
        shutdown,
    ));
    runtime.shutdown_timeout(Duration::ZERO);
    result
}

async fn run_prepared(
    config: EngineConfig,
    spool: PathBuf,
    ownership: Arc<ExclusiveListener>,
    access: Arc<ServerAccess>,
    acceptor: TlsAcceptor,
    listener: std::net::TcpListener,
    mut shutdown: ShutdownSignal,
) -> Result<(), HostFailure> {
    let listener = TcpListener::from_std(listener).map_err(HostFailure::Io)?;
    let local = UnixListener::from_std(ownership.listener().try_clone().map_err(HostFailure::Io)?)
        .map_err(HostFailure::Io)?;
    let jobs = TaskTracker::new();
    let loading = load_engine(config, spool, ownership, &access, &jobs, &mut shutdown);
    let Some(engine) = loading.await? else {
        return Ok(());
    };
    let engine = Arc::new(engine);
    let router = public_router(Arc::clone(&engine), Arc::clone(&access), jobs.clone()).await;
    let admin = admin_router(Arc::clone(&access), jobs.clone());
    let address = listener.local_addr().map_err(HostFailure::Io)?;
    let stop = CancellationToken::new();
    let public = router.serve_with_incoming_shutdown(
        incoming(listener, acceptor, Arc::clone(&access)),
        stop.clone().cancelled_owned(),
    );
    let private =
        admin.serve_with_incoming_shutdown(same_user(local), stop.clone().cancelled_owned());
    tokio::pin!(public, private);
    tracing::info!(engine = engine.name(), %address, "inference server ready");
    let (result, ended) = tokio::select! {
        result = &mut public => (result.map_err(HostFailure::Transport), Ended::Public),
        result = &mut private => (result.map_err(HostFailure::Transport), Ended::Private),
        result = shutdown.triggered() => (result.map_err(HostFailure::Shutdown), Ended::Signal),
    };
    let drain_deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    access.stop();
    stop.cancel();
    jobs.close();
    let drained = tokio::time::timeout(CONNECTION_DRAIN, async {
        if ended != Ended::Public {
            (&mut public).await.map_err(HostFailure::Transport)?;
        }
        if ended != Ended::Private {
            (&mut private).await.map_err(HostFailure::Transport)?;
        }
        Ok::<_, HostFailure>(())
    })
    .await;
    if let Ok(Err(error)) = drained {
        return Err(error);
    }
    if tokio::time::timeout_at(drain_deadline, jobs.wait())
        .await
        .is_err()
    {
        return Err(HostFailure::Drain);
    }
    result
}

/// A server that stopped on its own needs no drain.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ended {
    Public,
    Private,
    Signal,
}

/// Loads the model, or stops cleanly when shutdown arrives first.
async fn load_engine(
    config: EngineConfig,
    spool: PathBuf,
    ownership: Arc<ExclusiveListener>,
    access: &ServerAccess,
    jobs: &TaskTracker,
    shutdown: &mut ShutdownSignal,
) -> Result<Option<Engine>, HostFailure> {
    let cancel = CancelToken::new();
    let loading_cancel = cancel.clone();
    let mut loading =
        jobs.spawn_blocking(move || Engine::load(config, spool, ownership, &loading_cancel));
    tokio::select! {
        result = &mut loading => {
            let engine = result.map_err(|_| HostFailure::Worker)?;
            engine.map(Some).map_err(HostFailure::Model)
        }
        signal = shutdown.triggered() => {
            cancel.cancel();
            access.stop();
            jobs.close();
            if tokio::time::timeout(SHUTDOWN_GRACE, jobs.wait()).await.is_err() {
                return Err(HostFailure::Drain);
            }
            signal.map_err(HostFailure::Shutdown).map(|()| None)
        }
    }
}

async fn public_router(
    engine: Arc<Engine>,
    access: Arc<ServerAccess>,
    jobs: TaskTracker,
) -> tonic::transport::server::Router {
    let (health, service) = tonic_health::server::health_reporter();
    health
        .set_serving::<wire::inference_service_server::InferenceServiceServer<InferenceRpc>>()
        .await;
    let authentication = Arc::clone(&access);
    let paired = move |request: tonic::Request<()>| {
        authenticate(&authentication, &request)?;
        Ok(request)
    };
    tonic::transport::Server::builder()
        .concurrency_limit_per_connection(MAX_LAN_STREAMS)
        .max_concurrent_streams(Some(MAX_LAN_STREAMS as u32))
        .http2_keepalive_interval(Some(LAN_KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(LAN_KEEPALIVE_TIMEOUT))
        .add_service(
            wire::inference_service_server::InferenceServiceServer::new(InferenceRpc::new(
                Arc::clone(&engine),
                Arc::clone(&access),
                jobs.clone(),
            ))
            .max_decoding_message_size(MAX_WIRE_MESSAGE)
            .max_encoding_message_size(MAX_RESPONSE_BYTES),
        )
        .add_service(
            wire::pairing_service_server::PairingServiceServer::new(InferencePairingRpc::new(
                Arc::clone(&access),
                jobs.clone(),
            ))
            .max_decoding_message_size(1024),
        )
        .add_service(InterceptedService::new(service, paired))
}

fn admin_router(access: Arc<ServerAccess>, jobs: TaskTracker) -> tonic::transport::server::Router {
    tonic::transport::Server::builder()
        .concurrency_limit_per_connection(MAX_LAN_STREAMS)
        .max_concurrent_streams(Some(MAX_LAN_STREAMS as u32))
        .add_service(
            wire::inference_admin_service_server::InferenceAdminServiceServer::new(
                InferenceAdminRpc::new(Arc::clone(&access), jobs),
            )
            .max_decoding_message_size(4096),
        )
}

fn incoming(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    access: Arc<ServerAccess>,
) -> impl Stream<Item = io::Result<Leased<TlsStream<Admitted>>>> {
    let admitted = connections::admitted(listener, MAX_LAN_CONNECTIONS);
    secured(
        acceptor,
        admitted,
        TLS_HANDSHAKE_TIMEOUT,
        MAX_LAN_CONNECTIONS,
        move |pin| match pin.and_then(|pin| access.connection(pin)) {
            Some(cancel) => Lease::UntilCancelled(cancel),
            None => Lease::For(PAIRING_LEASE),
        },
    )
}

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct InferenceHostError(HostFailure);

#[derive(Debug, thiserror::Error)]
enum HostFailure {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Paths(#[from] RuntimeError),
    #[error(transparent)]
    Socket(#[from] LocalSocketError),
    #[error(transparent)]
    Identity(#[from] identity::Error),
    #[error(transparent)]
    Tls(#[from] tls::Error),
    #[error(transparent)]
    Shutdown(#[from] ShutdownError),
    #[error("loading the inference engine failed")]
    Model(#[from] TranscriptionError),
    #[error("opening the inference listener or runtime failed")]
    Io(#[source] io::Error),
    #[error("the inference transport failed")]
    Transport(#[source] tonic::transport::Error),
    #[error("the inference worker stopped unexpectedly")]
    Worker,
    #[error("inference did not stop within its shutdown grace")]
    Drain,
}

#[cfg(test)]
pub(in crate::daemon) mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use rustls_pki_types::CertificateDer;
    use rustls_pki_types::pem::PemObject;
    use tokio::sync::Notify;
    use tonic::transport::Channel;

    use super::super::identity::{ClientIdentity, Invitation};
    use super::*;
    use crate::audio::RecordingReader;
    use crate::config::ServerEndpoint;
    use crate::daemon::relay::fingerprint;
    use crate::transcription::{
        AudioInputKind, ChunkStream, Emitted, Hypothesis, Language, Stream as RecognitionStream,
        Transcriber, Transcription, TranscriptionAudio, Whole,
    };

    #[derive(Default)]
    pub(in crate::daemon) struct Observed {
        pub(in crate::daemon) calls: AtomicUsize,
        pub(in crate::daemon) native: AtomicUsize,
        pub(in crate::daemon) samples: std::sync::Mutex<Vec<Vec<f32>>>,
        pub(in crate::daemon) languages: std::sync::Mutex<Vec<String>>,
        entered: Notify,
        cancelled: Notify,
        hold: AtomicBool,
    }

    struct ReleaseOnDrop(Arc<Observed>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.hold.store(false, Ordering::SeqCst);
        }
    }

    struct Fake(Arc<Observed>);

    impl Transcriber for Fake {
        fn validate_language(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }
        fn transcribe_with(
            &self,
            audio: TranscriptionAudio<'_>,
            language: &Language,
            cancel: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            assert!(matches!(audio, TranscriptionAudio::Samples(samples) if !samples.is_empty()));
            self.0
                .samples
                .lock()
                .unwrap()
                .push(audio.require_samples("whisper")?.to_vec());
            self.0
                .languages
                .lock()
                .unwrap()
                .push(language.as_str().to_owned());
            self.0.calls.fetch_add(1, Ordering::SeqCst);
            self.0.entered.notify_one();
            while self.0.hold.load(Ordering::SeqCst) {
                if cancel.is_cancelled() {
                    self.0.cancelled.notify_one();
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if cancel.is_cancelled() {
                return Err(TranscriptionError::Cancelled);
            }
            Ok(Hypothesis::new("raw model words"))
        }
        fn open_stream(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
            self.0.native.fetch_add(1, Ordering::SeqCst);
            Ok(Some(Box::new(Native)))
        }
    }

    struct Native;
    impl ChunkStream for Native {
        fn chunk_samples(&self) -> usize {
            160
        }
        fn feed(&mut self, _: &[f32]) -> Result<Emitted, TranscriptionError> {
            Ok(Emitted {
                words: Hypothesis::new("native words"),
                joins_previous: false,
            })
        }
    }

    pub(in crate::daemon) struct Fixture {
        access: Arc<ServerAccess>,
        client: ClientIdentity,
        invite: Invitation,
        channel: Channel,
        pub(in crate::daemon) observed: Arc<Observed>,
        stop: CancellationToken,
        serving: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
        jobs: TaskTracker,
        _directory: tempfile::TempDir,
    }

    impl Fixture {
        pub(in crate::daemon) async fn start() -> Self {
            let observed = Arc::new(Observed::default());
            Self::with_model(
                "whisper",
                AudioInputKind::Samples,
                Some(RecognitionStream::Native),
                Box::new(Fake(Arc::clone(&observed))),
                observed,
            )
            .await
        }

        pub(in crate::daemon) async fn with_model(
            name: &'static str,
            input: AudioInputKind,
            stream: Option<RecognitionStream>,
            transcriber: Box<dyn Transcriber>,
            observed: Arc<Observed>,
        ) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let paths =
                crate::runtime::RuntimePaths::in_directory(directory.path().join("runtime"))
                    .prepare()
                    .unwrap();
            let ownership =
                Arc::new(ExclusiveListener::bind(paths.socket_path(), paths.lock_path()).unwrap());
            let access =
                ServerAccess::load_or_create(&directory.path().join("server.json")).unwrap();
            let client =
                ClientIdentity::load_or_create(&directory.path().join("client.json")).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let invite = access
                .issue(
                    &ServerEndpoint::parse(&format!("https://{}", listener.local_addr().unwrap()))
                        .unwrap(),
                )
                .unwrap();
            let engine = Arc::new(Engine::new(
                name,
                input,
                stream,
                transcriber,
                paths.spool_dir().to_owned(),
                ownership,
            ));
            let jobs = TaskTracker::new();
            let router = public_router(engine, Arc::clone(&access), jobs.clone()).await;
            let stop = CancellationToken::new();
            let serving = tokio::spawn(router.serve_with_incoming_shutdown(
                incoming(listener, tls::server(&access).unwrap(), Arc::clone(&access)),
                stop.clone().cancelled_owned(),
            ));
            let channel = tls::invited_endpoint(&client, &invite)
                .unwrap()
                .connect()
                .await
                .unwrap();
            Self {
                access,
                client,
                invite,
                channel,
                observed,
                stop,
                serving,
                jobs,
                _directory: directory,
            }
        }

        async fn pair(&mut self) {
            wire::pairing_service_client::PairingServiceClient::new(self.channel.clone())
                .pair(wire::PairRequest {
                    code: self.invite.code.clone(),
                })
                .await
                .unwrap();
            self.client.remember(&self.invite).unwrap();
            self.channel = tls::endpoint(
                &self.client,
                self.client.pinned_server(&self.invite.endpoint).unwrap(),
            )
            .unwrap()
            .connect()
            .await
            .unwrap();
        }

        fn pin(&self) -> [u8; 32] {
            fingerprint(
                &CertificateDer::from_pem_slice(
                    self.client.credentials().certificate_pem.as_bytes(),
                )
                .unwrap(),
            )
        }

        pub(in crate::daemon) fn endpoint(&self) -> &ServerEndpoint {
            &self.invite.endpoint
        }

        /// A fresh home paired with this server.
        pub(in crate::daemon) async fn paired_client(&self) -> PairedClient {
            let home = tempfile::tempdir().unwrap();
            self.pair_home(home.path().to_owned()).await;
            PairedClient {
                home,
                endpoint: self.endpoint().to_owned(),
            }
        }

        /// Blocks the fake model until the guard drops.
        fn hold_model(&self) -> ReleaseOnDrop {
            self.observed.hold.store(true, Ordering::SeqCst);
            ReleaseOnDrop(Arc::clone(&self.observed))
        }

        async fn model_cancelled(&self) {
            tokio::time::timeout(Duration::from_secs(2), self.observed.cancelled.notified())
                .await
                .unwrap();
        }

        pub(in crate::daemon) async fn pair_home(&self, home: PathBuf) {
            let invitation = serde_json::to_string(&self.invite).unwrap();
            tokio::task::spawn_blocking(move || {
                super::super::client::pair_at(&home, &invitation).unwrap()
            })
            .await
            .unwrap();
        }

        async fn idle(&self) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match self.access.admit(self.pin()) {
                        Ok(admission) => {
                            drop(admission);
                            return;
                        }
                        Err(super::super::identity::Error::Busy) => tokio::task::yield_now().await,
                        Err(error) => panic!("unexpected admission failure: {error}"),
                    }
                }
            })
            .await
            .unwrap();
        }

        pub(in crate::daemon) async fn stop(self) {
            self.access.stop();
            self.stop.cancel();
            self.jobs.close();
            tokio::time::timeout(Duration::from_secs(2), self.jobs.wait())
                .await
                .unwrap();
            self.serving.await.unwrap().unwrap();
        }
    }

    pub(in crate::daemon) struct PairedClient {
        pub(in crate::daemon) home: tempfile::TempDir,
        endpoint: ServerEndpoint,
    }

    impl PairedClient {
        pub(in crate::daemon) fn connect(&self) -> super::super::client::RemoteTranscriber {
            super::super::client::RemoteTranscriber::connect(
                self.home.path(),
                &self.endpoint,
                &CancelToken::never(),
            )
            .unwrap()
        }
    }

    fn start(strategy: wire::RecognitionStrategy) -> wire::InferenceRequest {
        wire::InferenceRequest {
            body: Some(wire::inference_request::Body::Start(wire::InferenceStart {
                engine: "whisper".into(),
                language: "auto".into(),
                strategy: strategy as i32,
            })),
        }
    }
    fn audio(bytes: Vec<u8>) -> wire::InferenceRequest {
        wire::InferenceRequest {
            body: Some(wire::inference_request::Body::Audio(bytes)),
        }
    }
    fn stop() -> wire::InferenceRequest {
        wire::InferenceRequest {
            body: Some(wire::inference_request::Body::Stop(wire::InferenceStop {})),
        }
    }
    fn recording() -> Vec<wire::InferenceRequest> {
        vec![
            start(wire::RecognitionStrategy::Whole),
            audio([0x00, 0x10].repeat(160)),
            stop(),
        ]
    }
    async fn transcribe(
        channel: Channel,
        messages: Vec<wire::InferenceRequest>,
    ) -> Result<wire::InferenceComplete, tonic::Status> {
        let response = wire::inference_service_client::InferenceServiceClient::new(channel)
            .transcribe(tokio_stream::iter(messages))
            .await?;
        completed(response.into_inner()).await
    }

    async fn completed(
        mut stream: tonic::Streaming<wire::InferenceResponse>,
    ) -> Result<wire::InferenceComplete, tonic::Status> {
        let mut complete = None;
        while let Some(event) = stream.message().await? {
            assert!(complete.is_none(), "no response follows Complete");
            if let Some(wire::inference_response::Body::Complete(transcript)) = event.body {
                complete = Some(transcript);
            }
        }
        complete.ok_or_else(|| tonic::Status::internal("missing completion"))
    }
    async fn info(channel: Channel) -> Result<wire::InferenceInfoResponse, tonic::Status> {
        wire::inference_service_client::InferenceServiceClient::new(channel)
            .get_info(wire::InferenceInfoRequest {})
            .await
            .map(tonic::Response::into_inner)
    }

    #[tokio::test]
    async fn revocation_releases_idle_connection_slots() {
        let mut fixture = Fixture::start().await;
        fixture.pair().await;
        let mut legitimate =
            ClientIdentity::load_or_create(&fixture._directory.path().join("legitimate.json"))
                .unwrap();
        let invitation = fixture.access.issue(fixture.endpoint()).unwrap();
        let certificate =
            CertificateDer::from_pem_slice(legitimate.credentials().certificate_pem.as_bytes())
                .unwrap();
        fixture
            .access
            .pair(fingerprint(&certificate), &invitation.code)
            .unwrap();
        legitimate.remember(&invitation).unwrap();

        let endpoint = tls::endpoint(
            &fixture.client,
            fixture.client.pinned_server(fixture.endpoint()).unwrap(),
        )
        .unwrap();
        let mut held = vec![fixture.channel.clone()];
        for _ in 1..MAX_LAN_CONNECTIONS {
            let channel = endpoint.connect().await.unwrap();
            info(channel.clone()).await.unwrap();
            held.push(channel);
        }
        fixture.access.revoke(fixture.pin()).unwrap();

        let endpoint = tls::endpoint(
            &legitimate,
            legitimate.pinned_server(fixture.endpoint()).unwrap(),
        )
        .unwrap();
        let admitted = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(channel) = endpoint.connect().await
                    && info(channel).await.is_ok()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        drop(held);
        fixture.stop().await;
        assert!(admitted.is_ok(), "revoked connections retained every slot");
    }

    #[tokio::test]
    async fn paired_tls_clients_reuse_one_engine_and_cannot_reach_admin() {
        let mut fixture = Fixture::start().await;
        assert_eq!(
            info(fixture.channel.clone()).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        fixture.pair().await;
        let replay =
            wire::pairing_service_client::PairingServiceClient::new(fixture.channel.clone())
                .pair(wire::PairRequest {
                    code: fixture.invite.code.clone(),
                })
                .await
                .unwrap_err();
        assert_eq!(replay.code(), tonic::Code::Unauthenticated);
        assert_eq!(
            info(fixture.channel.clone()).await.unwrap().engine,
            "whisper"
        );
        for _ in 0..2 {
            let response = transcribe(fixture.channel.clone(), recording())
                .await
                .unwrap();
            assert_eq!(response.text, "raw model words");
            assert_eq!(response.engine, "whisper");
        }
        let response = transcribe(
            fixture.channel.clone(),
            vec![
                start(wire::RecognitionStrategy::Streaming),
                audio([0x00, 0x10].repeat(160)),
                stop(),
            ],
        )
        .await
        .unwrap();
        assert!(response.text.contains("native words"));
        assert_eq!(fixture.observed.calls.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.observed.native.load(Ordering::SeqCst), 1);
        let admin = wire::inference_admin_service_client::InferenceAdminServiceClient::new(
            fixture.channel.clone(),
        )
        .issue_invite(wire::InferenceInviteRequest {
            endpoint: fixture.invite.endpoint.to_string(),
        })
        .await
        .unwrap_err();
        assert_eq!(admin.code(), tonic::Code::Unimplemented);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn whole_only_engines_do_not_advertise_or_accept_streaming() {
        let observed = Arc::new(Observed::default());
        let mut fixture = Fixture::with_model(
            "whisper",
            AudioInputKind::Samples,
            None,
            Box::new(Fake(Arc::clone(&observed))),
            observed,
        )
        .await;
        fixture.pair().await;
        assert_eq!(
            info(fixture.channel.clone()).await.unwrap().strategies,
            vec![wire::RecognitionStrategy::Whole as i32]
        );
        let error = transcribe(
            fixture.channel.clone(),
            vec![
                start(wire::RecognitionStrategy::Streaming),
                audio(vec![0, 1]),
                stop(),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unimplemented);
        assert_eq!(fixture.observed.native.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.observed.calls.load(Ordering::SeqCst), 0);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn disconnect_while_waiting_for_audio_releases_the_worker() {
        let mut fixture = Fixture::start().await;
        fixture.pair().await;
        let (send, receive) = tokio::sync::mpsc::channel(1);
        send.send(start(wire::RecognitionStrategy::Streaming))
            .await
            .unwrap();
        let response =
            wire::inference_service_client::InferenceServiceClient::new(fixture.channel.clone())
                .transcribe(tokio_stream::wrappers::ReceiverStream::new(receive))
                .await
                .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.observed.native.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(response);
        drop(send);
        fixture.idle().await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn malformed_uploads_never_reach_the_model() {
        let mut fixture = Fixture::start().await;
        fixture.pair().await;
        let cases = vec![
            vec![audio(vec![1, 2]), stop()],
            vec![start(wire::RecognitionStrategy::Whole), audio(vec![1, 2])],
            vec![
                start(wire::RecognitionStrategy::Whole),
                audio(vec![1]),
                stop(),
            ],
            vec![start(wire::RecognitionStrategy::Whole), stop()],
            vec![
                start(wire::RecognitionStrategy::Whole),
                start(wire::RecognitionStrategy::Whole),
            ],
            vec![
                start(wire::RecognitionStrategy::Whole),
                audio(vec![1, 2]),
                stop(),
                stop(),
            ],
        ];
        for messages in cases {
            assert_eq!(
                transcribe(fixture.channel.clone(), messages)
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::InvalidArgument
            );
            fixture.idle().await;
        }
        let too_large = vec![
            start(wire::RecognitionStrategy::Whole),
            audio(vec![1; MAX_AUDIO_MESSAGE_BYTES + 2]),
            stop(),
        ];
        assert_eq!(
            transcribe(fixture.channel.clone(), too_large)
                .await
                .unwrap_err()
                .code(),
            tonic::Code::ResourceExhausted
        );
        fixture.idle().await;
        let mut too_long = vec![start(wire::RecognitionStrategy::Whole)];
        let chunks_past_ten_minutes = (600 * 16_000 * 2) / MAX_AUDIO_MESSAGE_BYTES + 1;
        too_long
            .extend((0..chunks_past_ten_minutes).map(|_| audio(vec![0; MAX_AUDIO_MESSAGE_BYTES])));
        too_long.push(stop());
        assert_eq!(
            transcribe(fixture.channel.clone(), too_long)
                .await
                .unwrap_err()
                .code(),
            tonic::Code::ResourceExhausted,
        );
        fixture.idle().await;
        assert_eq!(fixture.observed.calls.load(Ordering::SeqCst), 0);
        assert!(
            transcribe(fixture.channel.clone(), recording())
                .await
                .is_ok()
        );
        fixture.stop().await;
    }

    #[tokio::test]
    async fn cancellation_keeps_admission_until_native_work_exits() {
        let mut fixture = Fixture::start().await;
        fixture.pair().await;
        let _release = fixture.hold_model();
        let work = tokio::spawn(transcribe(fixture.channel.clone(), recording()));
        tokio::time::timeout(Duration::from_secs(2), fixture.observed.entered.notified())
            .await
            .unwrap();
        work.abort();
        let _ = work.await;
        fixture.model_cancelled().await;
        assert_eq!(
            transcribe(fixture.channel.clone(), recording())
                .await
                .unwrap_err()
                .code(),
            tonic::Code::ResourceExhausted
        );
        fixture.observed.hold.store(false, Ordering::SeqCst);
        fixture.jobs.close();
        tokio::time::timeout(Duration::from_secs(2), fixture.jobs.wait())
            .await
            .unwrap();
        assert!(
            transcribe(fixture.channel.clone(), recording())
                .await
                .is_ok()
        );
        fixture.stop().await;
    }

    #[tokio::test]
    async fn revocation_cancels_active_work_and_denies_existing_channels() {
        let mut fixture = Fixture::start().await;
        fixture.pair().await;
        let _release = fixture.hold_model();
        let work = tokio::spawn(transcribe(fixture.channel.clone(), recording()));
        tokio::time::timeout(Duration::from_secs(2), fixture.observed.entered.notified())
            .await
            .unwrap();
        fixture.access.revoke(fixture.pin()).unwrap();
        assert!(work.await.unwrap().is_err());
        assert!(info(fixture.channel.clone()).await.is_err());
        fixture.model_cancelled().await;
        fixture.observed.hold.store(false, Ordering::SeqCst);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn caller_deadline_cancels_the_blocking_model() {
        let mut fixture = Fixture::start().await;
        fixture.pair().await;
        let _release = fixture.hold_model();
        let mut request = tonic::Request::new(tokio_stream::iter(recording()));
        request.set_timeout(Duration::from_millis(200));
        let result =
            wire::inference_service_client::InferenceServiceClient::new(fixture.channel.clone())
                .transcribe(request)
                .await;
        let result = match result {
            Ok(response) => completed(response.into_inner()).await,
            Err(error) => Err(error),
        };
        assert!(result.is_err());
        fixture.model_cancelled().await;
        fixture.observed.hold.store(false, Ordering::SeqCst);
        fixture.stop().await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ARIA_TEST_WHISPER_MODEL and real native inference"]
    async fn whisper_headless_loopback_reuses_a_warm_model() {
        use super::super::client::RemoteTranscriber;
        use std::time::Instant;

        aria_stt::whisper::install_logging_hooks();
        let model_path = std::env::var_os("ARIA_TEST_WHISPER_MODEL")
            .expect("set ARIA_TEST_WHISPER_MODEL to the test model");
        let config = EngineConfig::Whisper(aria_stt::whisper::WhisperConfig::new(model_path));
        let name = config.name();
        let input = config.input();
        let stream = config.stream();
        let (model, load_time) = tokio::task::spawn_blocking(move || {
            let began = Instant::now();
            let model = config
                .load(&CancelToken::never())
                .expect("the real model loads without device setup");
            (Arc::new(model), began.elapsed())
        })
        .await
        .unwrap();
        let fixture = Fixture::with_model(
            name,
            input,
            stream,
            Box::new(Arc::clone(&model)),
            Arc::new(Observed::default()),
        )
        .await;
        let home = fixture._directory.path().to_owned();
        fixture.pair_home(home.clone()).await;
        let endpoint = fixture.endpoint().to_owned();
        tokio::task::spawn_blocking(move || {
            let fixture_audio = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../aria-wake/tests/fixtures/negative.wav");
            let mut recording = RecordingReader::from_wav_file(fixture_audio).unwrap();
            recording.fill().unwrap();
            let samples = recording.samples().to_vec();
            let began = Instant::now();
            let expected = Transcription::complete(input, model)
                .transcribe_with(&mut recording, &Language::Automatic, &CancelToken::never())
                .unwrap();
            let local_time = began.elapsed();
            let client = RemoteTranscriber::connect(&home, &endpoint, &CancelToken::never())
                .unwrap()
                .require_engine(name)
                .unwrap();
            let remote = Transcription::<_, Whole>::delegated(client);
            let mut times = Vec::new();
            for _ in 0..2 {
                let began = Instant::now();
                let actual = remote
                    .transcribe(
                        &mut RecordingReader::from_samples(&samples),
                        &Language::Automatic,
                    )
                    .unwrap();
                times.push(began.elapsed());
                assert_eq!(actual.as_str(), expected.as_str());
                assert!(!actual.as_str().is_empty());
            }
            eprintln!(
                "headless whisper: load={load_time:?}, direct={local_time:?}, loopback={times:?}"
            );
        })
        .await
        .unwrap();
        fixture.stop().await;
    }
}
