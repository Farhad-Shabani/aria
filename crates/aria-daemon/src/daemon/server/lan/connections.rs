use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::{Stream, StreamExt};
use tonic::transport::server::{Connected, TcpConnectInfo};

/// Connections one address may hold open at once, so one peer holding idle
/// sockets can't take every slot.
pub(in crate::daemon) const MAX_CONNECTIONS_PER_PEER: usize = 4;

/// Accepted connections, at most `limit` open at once and
/// [`MAX_CONNECTIONS_PER_PEER`] from one address. One past either limit is
/// closed as it arrives.
pub(in crate::daemon) fn admitted(
    listener: TcpListener,
    limit: usize,
) -> impl Stream<Item = io::Result<Admitted>> {
    let open = Arc::new(Semaphore::new(limit));
    let mut open_by_peer: HashMap<IpAddr, Arc<Semaphore>> = HashMap::new();
    // EMFILE repeats on every poll until a descriptor frees up.
    let mut failing = false;
    // A flood of refusals would rotate older entries out of the daemon log,
    // so only the first after an admission is logged.
    let mut refusing = false;
    TcpListenerStream::new(listener).filter_map(move |accepted| match accepted {
        Err(error) => {
            if !failing {
                tracing::warn!(%error, "failed to accept a LAN connection");
            }
            failing = true;
            Some(Err(error))
        }
        Ok(stream) => {
            failing = false;
            let peer = stream.peer_addr().ok()?.ip().to_canonical();
            open_by_peer.retain(|_, open| open.available_permits() < MAX_CONNECTIONS_PER_PEER);
            let peer_open = open_by_peer
                .entry(peer)
                .or_insert_with(|| Arc::new(Semaphore::new(MAX_CONNECTIONS_PER_PEER)));
            let admitted = Arc::clone(peer_open)
                .try_acquire_owned()
                .ok()
                .and_then(|peer_slot| {
                    let slot = Arc::clone(&open).try_acquire_owned().ok()?;
                    Some(Admitted {
                        stream,
                        _slot: slot,
                        _peer_slot: peer_slot,
                    })
                });
            if admitted.is_some() {
                refusing = false;
            } else if !refusing {
                refusing = true;
                tracing::warn!(
                    limit,
                    per_peer = MAX_CONNECTIONS_PER_PEER,
                    %peer,
                    "closing LAN connections past the limit"
                );
            }
            admitted.map(Ok)
        }
    })
}

/// A connection holding its slots, overall and for its address, until it
/// drops.
pub(in crate::daemon) struct Admitted {
    stream: TcpStream,
    _slot: OwnedSemaphorePermit,
    _peer_slot: OwnedSemaphorePermit,
}

impl Admitted {
    pub(super) fn peer(&self) -> Option<SocketAddr> {
        self.stream.peer_addr().ok()
    }
}

impl Connected for Admitted {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.stream.connect_info()
    }
}

impl AsyncRead for Admitted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for Admitted {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    async fn a_connection_past_the_limit_is_closed_until_one_drops() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut incoming = Box::pin(admitted(listener, 1));
        let _first_client = TcpStream::connect(address).await.unwrap();
        let first = incoming.next().await.unwrap().unwrap();

        let mut second_client = TcpStream::connect(address).await.unwrap();
        let mut byte = [0u8; 1];
        tokio::select! {
            _ = incoming.next() => panic!("a connection past the limit was admitted"),
            read = second_client.read(&mut byte) => assert_eq!(read.unwrap(), 0),
        }

        drop(first);
        let _third_client = TcpStream::connect(address).await.unwrap();
        assert!(incoming.next().await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn one_peer_past_its_share_is_closed_while_slots_remain() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut incoming = Box::pin(admitted(listener, MAX_CONNECTIONS_PER_PEER * 2));
        let mut clients = Vec::new();
        let mut held = Vec::new();
        for _ in 0..MAX_CONNECTIONS_PER_PEER {
            clients.push(TcpStream::connect(address).await.unwrap());
            held.push(incoming.next().await.unwrap().unwrap());
        }

        let mut past_share = TcpStream::connect(address).await.unwrap();
        let mut byte = [0u8; 1];
        tokio::select! {
            _ = incoming.next() => panic!("a connection past the peer's share was admitted"),
            read = past_share.read(&mut byte) => assert_eq!(read.unwrap(), 0),
        }

        held.pop();
        let _next_client = TcpStream::connect(address).await.unwrap();
        assert!(incoming.next().await.unwrap().is_ok());
    }
}
