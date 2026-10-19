use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::{Stream, StreamExt, future};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::client::danger::HandshakeSignatureValid;
use tokio_rustls::rustls::crypto::WebPkiSupportedAlgorithms;
use tokio_rustls::rustls::server::NoServerSessionStorage;
use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio_rustls::rustls::{
    self, CertificateError, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use tokio_rustls::server::TlsStream;
use tonic::transport::server::Connected;

use super::connections::Admitted;
use crate::daemon::relay::{Pairing, PairingCodes, Phones, fingerprint};

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("the pairing certificate or key is not PEM")]
    Pem(#[from] rustls_pki_types::pem::Error),
    #[error(transparent)]
    Rustls(#[from] rustls::Error),
}

/// The server side of the LAN handshake: this Mac's certificate, a phone's
/// certificate checked by [`PhoneVerifier`], over HTTP/2.
pub(super) struct Handshake {
    acceptor: TlsAcceptor,
    phones: Arc<Phones>,
}

impl Handshake {
    pub(super) fn new(pairing: &Pairing, codes: Arc<PairingCodes>) -> Result<Self, Error> {
        let certificate = CertificateDer::from_pem_slice(pairing.certificate_pem.as_bytes())?;
        let key = PrivateKeyDer::from_pem_slice(pairing.key_pem.as_bytes())?;
        let provider = rustls::crypto::ring::default_provider();
        let verifier = PhoneVerifier {
            phones: Arc::clone(&pairing.phones),
            codes,
            algorithms: provider.signature_verification_algorithms,
        };
        // TLS 1.2 sends the phone's certificate in the clear.
        let mut config = ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(Arc::new(verifier))
            .with_single_cert(vec![certificate], key)?;
        config.alpn_protocols = vec![b"h2".to_vec()];
        // A resumed session skips the verifier.
        config.session_storage = Arc::new(NoServerSessionStorage {});
        config.send_tls13_tickets = 0;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            phones: Arc::clone(&pairing.phones),
        })
    }

    /// `admitted` connections past a handshake bounded by `timeout`,
    /// `concurrent` at a time. A phone admitted on a code, not its pin, gets
    /// `lease` to pair.
    pub(super) fn secured(
        self,
        admitted: impl Stream<Item = io::Result<Admitted>>,
        timeout: Duration,
        lease: Duration,
        concurrent: usize,
    ) -> impl Stream<Item = io::Result<Leased<TlsStream<Admitted>>>> {
        let Self { acceptor, phones } = self;
        admitted
            .map(move |admitted| {
                let acceptor = acceptor.clone();
                let phones = Arc::clone(&phones);
                async move {
                    let connection = match admitted {
                        Ok(connection) => connection,
                        Err(error) => return Some(Err(error)),
                    };
                    let peer = connection.peer().map(tracing::field::display);
                    match tokio::time::timeout(timeout, acceptor.accept(connection)).await {
                        Ok(Ok(stream)) => {
                            let paired = stream
                                .get_ref()
                                .1
                                .peer_certificates()
                                .and_then(<[_]>::first)
                                .is_some_and(|leaf| phones.contains(&fingerprint(leaf)));
                            if !paired {
                                tracing::debug!(peer, "admitted an unpaired phone to pair");
                            }
                            Some(Ok(Leased::new(stream, (!paired).then_some(lease))))
                        }
                        Ok(Err(error)) => {
                            tracing::debug!(%error, peer, "a LAN handshake failed");
                            None
                        }
                        Err(_elapsed) => {
                            tracing::debug!(peer, "a LAN handshake timed out");
                            None
                        }
                    }
                }
            })
            .buffer_unordered(concurrent)
            .filter_map(future::ready)
    }
}

/// Lets a paired phone finish the handshake, or any phone while a pairing
/// code is live. The signature checks prove it holds its certificate's key.
#[derive(Debug)]
struct PhoneVerifier {
    phones: Arc<Phones>,
    codes: Arc<PairingCodes>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for PhoneVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if self.phones.contains(&fingerprint(end_entity)) || self.codes.is_live(Instant::now()) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, certificate, signature, &self.algorithms)
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

/// A connection that closes when its lease ends. Paired phones have none.
pub(super) struct Leased<IO> {
    inner: IO,
    ends: Option<Pin<Box<Sleep>>>,
    expired: bool,
}

impl<IO> Leased<IO> {
    fn new(inner: IO, lease: Option<Duration>) -> Self {
        Self {
            inner,
            ends: lease.map(|lease| Box::pin(tokio::time::sleep(lease))),
            expired: false,
        }
    }

    fn ended(&mut self, context: &mut Context<'_>) -> bool {
        if self.expired {
            return true;
        }
        let Some(ends) = self.ends.as_mut() else {
            return false;
        };
        if ends.as_mut().poll(context).is_pending() {
            return false;
        }
        tracing::debug!("closing a LAN connection whose pairing lease ended");
        self.expired = true;
        true
    }
}

impl<IO: Connected> Connected for Leased<IO> {
    type ConnectInfo = IO::ConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.inner.connect_info()
    }
}

impl<IO: AsyncRead + Unpin> AsyncRead for Leased<IO> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.ended(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl<IO: AsyncWrite + Unpin> AsyncWrite for Leased<IO> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.ended(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.ended(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.ended(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    /// A peer that never drains: every write, flush, and shutdown waits.
    struct Stalled;

    impl AsyncRead for Stalled {
        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for Stalled {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    const LEASE: Duration = Duration::from_secs(15);

    /// The error `operation` ends in, failing if it outlives twice the lease.
    async fn ended<T>(operation: impl Future<Output = io::Result<T>>) -> io::ErrorKind {
        match tokio::time::timeout(LEASE * 2, operation).await {
            Ok(Err(error)) => error.kind(),
            Ok(Ok(_)) => panic!("a stalled peer accepted the output"),
            Err(_elapsed) => panic!("the lease did not end it"),
        }
    }

    /// HTTP/2 can wait on a flush or a shutdown alone while its output is
    /// blocked, so the lease holds there too.
    #[tokio::test(start_paused = true)]
    async fn a_lease_ends_blocked_output() {
        let mut flushing = Leased::new(Stalled, Some(LEASE));
        assert_eq!(ended(flushing.flush()).await, io::ErrorKind::TimedOut);
        let mut closing = Leased::new(Stalled, Some(LEASE));
        assert_eq!(ended(closing.shutdown()).await, io::ErrorKind::TimedOut);
        let mut writing = Leased::new(Stalled, Some(LEASE));
        assert_eq!(ended(writing.write(b"h2")).await, io::ErrorKind::TimedOut);
    }
}
