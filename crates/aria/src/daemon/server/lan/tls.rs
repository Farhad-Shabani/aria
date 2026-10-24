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
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};
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
        let verifier = PhoneVerifier {
            phones: Arc::clone(&pairing.phones),
            codes,
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        };
        Ok(Self {
            acceptor: acceptor(certificate, key, Arc::new(verifier))?,
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
        secured(acceptor, admitted, timeout, concurrent, move |pin| {
            if pin.is_some_and(|pin| phones.contains(&pin)) {
                Lease::Open
            } else {
                Lease::For(lease)
            }
        })
    }
}

/// A LAN server's TLS: 1.3 only, because 1.2 sends the client certificate in
/// the clear, and no resumption, because a resumed session skips the verifier.
pub(in crate::daemon) fn acceptor(
    certificate: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    verifier: Arc<dyn ClientCertVerifier>,
) -> Result<TlsAcceptor, rustls::Error> {
    let provider = rustls::crypto::ring::default_provider();
    let mut config = ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![certificate], key)?;
    config.alpn_protocols = vec![b"h2".to_vec()];
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// `admitted` connections past a handshake bounded by `timeout`, `concurrent`
/// at a time, each leased by what `lease` makes of the peer's pin.
pub(in crate::daemon) fn secured(
    acceptor: TlsAcceptor,
    admitted: impl Stream<Item = io::Result<Admitted>>,
    timeout: Duration,
    concurrent: usize,
    lease: impl Fn(Option<[u8; 32]>) -> Lease + Clone,
) -> impl Stream<Item = io::Result<Leased<TlsStream<Admitted>>>> {
    admitted
        .map(move |admitted| {
            let acceptor = acceptor.clone();
            let lease = lease.clone();
            async move {
                let connection = match admitted {
                    Ok(connection) => connection,
                    Err(error) => return Some(Err(error)),
                };
                let peer = connection.peer().map(tracing::field::display);
                match tokio::time::timeout(timeout, acceptor.accept(connection)).await {
                    Ok(Ok(stream)) => {
                        let pin = stream
                            .get_ref()
                            .1
                            .peer_certificates()
                            .and_then(<[_]>::first)
                            .map(|leaf| fingerprint(leaf));
                        let lease = lease(pin);
                        if matches!(lease, Lease::For(_)) {
                            tracing::debug!(peer, "admitted an unpaired peer to pair");
                        }
                        Some(Ok(Leased::new(stream, lease)))
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

/// How long a connection may stay open past its handshake.
pub(in crate::daemon) enum Lease {
    Open,
    For(Duration),
    UntilCancelled(CancellationToken),
}

/// A connection that closes when its lease ends.
pub(in crate::daemon) struct Leased<IO> {
    inner: IO,
    ends: Option<ConnectionEnd>,
    ended: Option<io::ErrorKind>,
}

enum ConnectionEnd {
    Lease(Pin<Box<Sleep>>),
    Cancelled(Pin<Box<WaitForCancellationFutureOwned>>),
}

impl<IO> Leased<IO> {
    pub(in crate::daemon) fn new(inner: IO, lease: Lease) -> Self {
        let ends = match lease {
            Lease::Open => None,
            Lease::For(lease) => Some(ConnectionEnd::Lease(Box::pin(tokio::time::sleep(lease)))),
            Lease::UntilCancelled(cancel) => {
                Some(ConnectionEnd::Cancelled(Box::pin(cancel.cancelled_owned())))
            }
        };
        Self {
            inner,
            ends,
            ended: None,
        }
    }

    fn ended(&mut self, context: &mut Context<'_>) -> Option<io::ErrorKind> {
        if self.ended.is_some() {
            return self.ended;
        }
        let error = match self.ends.as_mut()? {
            ConnectionEnd::Lease(ends) => {
                if ends.as_mut().poll(context).is_pending() {
                    return None;
                }
                io::ErrorKind::TimedOut
            }
            ConnectionEnd::Cancelled(ends) => {
                if ends.as_mut().poll(context).is_pending() {
                    return None;
                }
                io::ErrorKind::ConnectionAborted
            }
        };
        tracing::debug!(?error, "closing an expired or cancelled connection");
        self.ended = Some(error);
        self.ended
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
        if let Some(error) = self.ended(context) {
            return Poll::Ready(Err(error.into()));
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
        if let Some(error) = self.ended(context) {
            return Poll::Ready(Err(error.into()));
        }
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(error) = self.ended(context) {
            return Poll::Ready(Err(error.into()));
        }
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(error) = self.ended(context) {
            return Poll::Ready(Err(error.into()));
        }
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        let mut flushing = Leased::new(Stalled, Lease::For(LEASE));
        assert_eq!(ended(flushing.flush()).await, io::ErrorKind::TimedOut);
        let mut closing = Leased::new(Stalled, Lease::For(LEASE));
        assert_eq!(ended(closing.shutdown()).await, io::ErrorKind::TimedOut);
        let mut writing = Leased::new(Stalled, Lease::For(LEASE));
        assert_eq!(ended(writing.write(b"h2")).await, io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn cancellation_wakes_blocked_input_and_output() {
        for operation in ["read", "write", "flush", "shutdown"] {
            let cancel = CancellationToken::new();
            let mut connection = Leased::new(Stalled, Lease::UntilCancelled(cancel.clone()));
            let polled = Arc::new(tokio::sync::Notify::new());
            let ready = Arc::clone(&polled);
            let waiting = tokio::spawn(async move {
                let operation = async {
                    match operation {
                        "read" => connection.read(&mut [0]).await.map(|_| ()),
                        "write" => connection.write(b"h2").await.map(|_| ()),
                        "flush" => connection.flush().await,
                        "shutdown" => connection.shutdown().await,
                        _ => unreachable!(),
                    }
                };
                tokio::pin!(operation);
                std::future::poll_fn(|context| {
                    let result = operation.as_mut().poll(context);
                    ready.notify_one();
                    result
                })
                .await
            });
            polled.notified().await;
            cancel.cancel();
            let result = tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        }
    }
}
