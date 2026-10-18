use std::io;

use tokio::net::{UnixListener, UnixStream};
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::{Stream, StreamExt};

/// Accepted connections from processes running as the daemon's own user.
/// The private runtime directory already keeps other users out. This check
/// still holds if its permissions are ever wrong.
pub(super) fn same_user(listener: UnixListener) -> impl Stream<Item = io::Result<UnixStream>> {
    let uid = rustix::process::getuid().as_raw();
    UnixListenerStream::new(listener).filter(move |accepted| match accepted {
        Err(_) => true,
        Ok(stream) => runs_as(stream, uid),
    })
}

fn runs_as(stream: &UnixStream, uid: u32) -> bool {
    match stream.peer_cred() {
        Ok(peer) if peer.uid() == uid => true,
        Ok(peer) => {
            tracing::warn!(
                peer_uid = peer.uid(),
                "refused a connection from another user"
            );
            false
        }
        Err(error) => {
            tracing::warn!(%error, "refused a connection whose user is unknown");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_the_daemons_own_user_is_accepted() {
        let (client, _server) = UnixStream::pair().expect("socket pair");
        let uid = rustix::process::getuid().as_raw();

        assert!(runs_as(&client, uid));
        assert!(!runs_as(&client, uid + 1));
    }
}
