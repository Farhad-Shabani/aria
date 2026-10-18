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
        // The app's liveness probe hangs up before its user can be read.
        Err(error) if error.kind() == io::ErrorKind::NotConnected => {
            tracing::debug!("a peer hung up before its user could be read");
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

    // Linux keeps the peer's credentials after it leaves; macOS does not.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_peer_that_already_hung_up_has_no_user() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("probe.sock");
        let listener = UnixListener::bind(&path).expect("listener");

        drop(std::os::unix::net::UnixStream::connect(&path).expect("probe"));
        let (stream, _) = listener.accept().await.expect("accepted probe");

        let error = stream.peer_cred().expect_err("the probe is gone");
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    }
}
