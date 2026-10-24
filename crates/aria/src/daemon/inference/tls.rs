use std::sync::Arc;
use std::time::Duration;

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::WebPkiSupportedAlgorithms;
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio_rustls::rustls::{
    self, CertificateError, DigitallySignedStruct, DistinguishedName, RootCertStore,
    SignatureScheme,
};
use tonic::transport::{ClientTlsConfig, Endpoint, Identity};

use super::identity::{
    CERTIFICATE_NAME, ClientIdentity, Credentials, Invitation, PinnedServer, ServerIdentity,
};
use crate::daemon::relay::fingerprint;

pub(super) fn server(identity: &Arc<ServerIdentity>) -> Result<TlsAcceptor, Error> {
    let verifier = ClientVerifier {
        identity: Arc::clone(identity),
        algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
    };
    let certificate =
        CertificateDer::from_pem_slice(identity.credentials().certificate_pem.as_bytes())?;
    let key = PrivateKeyDer::from_pem_slice(identity.credentials().key_pem.as_bytes())?;
    Ok(crate::daemon::server::lan::tls::acceptor(
        certificate,
        key,
        Arc::new(verifier),
    )?)
}

pub(super) fn invited_endpoint(
    client: &ClientIdentity,
    invitation: &Invitation,
) -> Result<Endpoint, Error> {
    invitation
        .validate()
        .map_err(|_| Error::InvalidInvitation)?;
    configured_endpoint(
        client,
        &invitation.endpoint,
        &invitation.certificate_pem,
        invitation.fingerprint,
    )
}

pub(super) fn endpoint(client: &ClientIdentity, server: &PinnedServer) -> Result<Endpoint, Error> {
    configured_endpoint(
        client,
        &server.endpoint,
        &server.certificate_pem,
        server.fingerprint,
    )
}

fn configured_endpoint(
    client: &ClientIdentity,
    address: &str,
    pem: &str,
    pin: [u8; 32],
) -> Result<Endpoint, Error> {
    super::identity::validate_endpoint(address).map_err(|_| Error::InvalidEndpoint)?;
    let certificate = CertificateDer::from_pem_slice(pem.as_bytes())?;
    if fingerprint(&certificate) != pin {
        return Err(Error::InvalidPin);
    }
    let verifier = Arc::new(PinnedVerifier {
        pin,
        verifier: certificate_verifier(certificate)?,
    });
    let credentials = client.credentials();
    let tls = ClientTlsConfig::new()
        .domain_name(CERTIFICATE_NAME)
        .identity(Identity::from_pem(
            &credentials.certificate_pem,
            &credentials.key_pem,
        ))
        .timeout(Duration::from_secs(5));
    Ok(Endpoint::from_shared(address.to_owned())?
        .connect_timeout(Duration::from_secs(5))
        .tls_config_with_verifier(tls, verifier)?)
}

pub(super) fn validate_credentials(credentials: &Credentials) -> Result<(), Error> {
    let certificate = CertificateDer::from_pem_slice(credentials.certificate_pem.as_bytes())?;
    let key = PrivateKeyDer::from_pem_slice(credentials.key_pem.as_bytes())?;
    rustls::sign::CertifiedKey::from_der(
        vec![certificate.clone()],
        key,
        &rustls::crypto::ring::default_provider(),
    )?
    .keys_match()?;
    validate_certificate(&certificate)
}

pub(super) fn validate_certificate(certificate: &CertificateDer<'_>) -> Result<(), Error> {
    certificate_verifier(certificate.clone().into_owned())?.verify_server_cert(
        certificate,
        &[],
        &ServerName::try_from(CERTIFICATE_NAME).map_err(|_| Error::InvalidPin)?,
        &[],
        UnixTime::now(),
    )?;
    Ok(())
}

fn certificate_verifier(
    certificate: CertificateDer<'static>,
) -> Result<Arc<WebPkiServerVerifier>, Error> {
    let mut roots = RootCertStore::empty();
    roots.add(certificate)?;
    WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .build()
    .map_err(|_| Error::InvalidPin)
}

struct ClientVerifier {
    identity: Arc<ServerIdentity>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for ClientVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ClientVerifier")
    }
}

impl ClientCertVerifier for ClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        certificate: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if !self.identity.contains(&fingerprint(certificate)) && !self.identity.pairing_is_open() {
            return Err(rejected());
        }
        let mut roots = RootCertStore::empty();
        roots.add(certificate.clone().into_owned())?;
        let verifier = WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .map_err(|_| rejected())?;
        verifier.verify_client_cert(certificate, intermediates, now)
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rejected())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, certificate, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedVerifier {
    pin: [u8; 32],
    verifier: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        certificate: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(certificate) != self.pin {
            return Err(rejected());
        }
        self.verifier
            .verify_server_cert(certificate, intermediates, name, ocsp, now)
    }

    // Tonic still offers TLS 1.2. Failing its server proof ends the handshake
    // before the client authenticates.
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rejected())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verifier
            .verify_tls13_signature(message, certificate, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.verifier.supported_verify_schemes()
    }
}

fn rejected() -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("the remote certificate or key is not PEM")]
    Pem(#[from] rustls_pki_types::pem::Error),
    #[error("remote TLS authentication failed")]
    Rustls(#[from] rustls::Error),
    #[error("the pinned remote certificate is invalid")]
    InvalidPin,
    #[error("the remote invitation is invalid")]
    InvalidInvitation,
    #[error("the remote endpoint is invalid")]
    InvalidEndpoint,
    #[error("the remote TLS endpoint could not be configured")]
    Transport(#[from] tonic::transport::Error),
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_client::HealthClient;

    use super::*;

    async fn listening(
        acceptor: TlsAcceptor,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<bool>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let (accepted, observed) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let incoming = futures_util::stream::once(async move {
                let (socket, _) = listener.accept().await?;
                let secured = acceptor.accept(socket).await;
                let _ = accepted.send(secured.is_ok());
                secured
            });
            let (_reporter, health) = tonic_health::server::health_reporter();
            let _ = tonic::transport::Server::builder()
                .add_service(health)
                .serve_with_incoming(incoming)
                .await;
        });
        (endpoint, task, observed)
    }

    fn identities(directory: &tempfile::TempDir) -> (Arc<ServerIdentity>, ClientIdentity) {
        let parent = directory.path().join("inference");
        (
            ServerIdentity::load_or_create(&parent.join("server.json")).unwrap(),
            ClientIdentity::load_or_create(&parent.join("client.json")).unwrap(),
        )
    }

    #[tokio::test]
    async fn production_client_authenticates_both_sides_and_refuses_revocation() {
        let directory = tempfile::tempdir().unwrap();
        let (identity, client) = identities(&directory);
        let (address, task, accepted) = listening(server(&identity).unwrap()).await;
        let invitation = identity.issue(&address).unwrap();
        let certificate =
            CertificateDer::from_pem_slice(client.credentials().certificate_pem.as_bytes())
                .unwrap();
        let pin = fingerprint(&certificate);
        identity.pair(pin, &invitation.code).unwrap();
        let channel = invited_endpoint(&client, &invitation)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let response = HealthClient::new(channel)
            .check(HealthCheckRequest {
                service: String::new(),
            })
            .await;
        assert!(response.is_ok());
        assert!(accepted.await.unwrap());
        task.abort();

        identity.revoke(pin).unwrap();
        let (address, task, accepted) = listening(server(&identity).unwrap()).await;
        let pinned = PinnedServer {
            endpoint: address,
            certificate_pem: invitation.certificate_pem,
            fingerprint: invitation.fingerprint,
        };
        let connected = endpoint(&client, &pinned).unwrap().connect().await;
        assert!(!accepted.await.unwrap());
        if let Ok(channel) = connected {
            assert!(
                HealthClient::new(channel)
                    .check(HealthCheckRequest {
                        service: String::new(),
                    })
                    .await
                    .is_err()
            );
        }
        task.abort();
    }

    #[tokio::test]
    async fn the_production_client_rejects_a_substituted_server() {
        let directory = tempfile::tempdir().unwrap();
        let (trusted, client) = identities(&directory);
        let other = tempfile::tempdir().unwrap();
        let (substitute, _) = identities(&other);
        let (address, task, _) = listening(server(&substitute).unwrap()).await;
        substitute.issue(&address).unwrap();
        let invitation = trusted.issue(&address).unwrap();
        assert!(
            invited_endpoint(&client, &invitation)
                .unwrap()
                .connect()
                .await
                .is_err()
        );
        task.abort();
    }

    #[tokio::test]
    async fn the_production_client_refuses_tls12() {
        let directory = tempfile::tempdir().unwrap();
        let (identity, client) = identities(&directory);
        let certificate =
            CertificateDer::from_pem_slice(identity.credentials().certificate_pem.as_bytes())
                .unwrap();
        let key = PrivateKeyDer::from_pem_slice(identity.credentials().key_pem.as_bytes()).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS12])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let (address, task, _) = listening(TlsAcceptor::from(Arc::new(config))).await;
        let invitation = identity.issue(&address).unwrap();
        assert!(
            invited_endpoint(&client, &invitation)
                .unwrap()
                .connect()
                .await
                .is_err()
        );
        task.abort();
    }

    #[test]
    fn expired_certificates_and_wrong_private_keys_are_rejected() {
        let mut parameters =
            rcgen::CertificateParams::new(vec![CERTIFICATE_NAME.to_owned()]).unwrap();
        parameters.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(2);
        parameters.not_after = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = parameters.self_signed(&key).unwrap();
        assert!(validate_certificate(certificate.der()).is_err());
        let other_key = rcgen::KeyPair::generate().unwrap();
        assert!(
            validate_credentials(&Credentials {
                certificate_pem: certificate.pem(),
                key_pem: other_key.serialize_pem()
            })
            .is_err()
        );
    }
}
