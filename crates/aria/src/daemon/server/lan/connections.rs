use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::{Stream, StreamExt};
use tonic::transport::server::{Connected, TcpConnectInfo};

/// Accepted connections, at most `limit` open at once. One past the limit
/// is closed as it arrives.
pub(super) fn admitted(
    listener: TcpListener,
    limit: usize,
) -> impl Stream<Item = io::Result<Admitted>> {
    let open = Arc::new(Semaphore::new(limit));
    TcpListenerStream::new(listener).filter_map(move |accepted| match accepted {
        Err(error) => Some(Err(error)),
        Ok(stream) => match Arc::clone(&open).try_acquire_owned() {
            Ok(slot) => Some(Ok(Admitted {
                stream,
                _slot: slot,
            })),
            Err(_full) => {
                tracing::warn!(limit, "closing a LAN connection past the limit");
                None
            }
        },
    })
}

/// A connection holding one of the open slots until it drops.
pub(super) struct Admitted {
    stream: TcpStream,
    _slot: OwnedSemaphorePermit,
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
}
