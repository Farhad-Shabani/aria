//! Sends paste keys through the Wayland RemoteDesktop portal.
//! A private restore token lets later sessions reuse a keyboard-control grant.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::{PersistMode, ResponseError, Session};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::APP_ID;
use super::desktop::{Refusal, SendFailure};
use crate::primitives::private_document::PrivateDocument;

const RELATIVE_PATH: &str = ".aria/desktop/remote-desktop.json";
const MAX_TOKEN_BYTES: u64 = 4096;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const KEY_TIMEOUT: Duration = Duration::from_secs(1);
// linux/input-event-codes.h
const KEY_LEFTSHIFT: i32 = 42;
const KEY_INSERT: i32 = 110;
const PASTE_CHORD: [(i32, KeyState); 4] = [
    (KEY_LEFTSHIFT, KeyState::Pressed),
    (KEY_INSERT, KeyState::Pressed),
    (KEY_INSERT, KeyState::Released),
    (KEY_LEFTSHIFT, KeyState::Released),
];

pub(super) struct PortalKeyboard {
    portal: RemoteDesktop,
    session: Arc<Session<RemoteDesktop>>,
    /// Stopped first on drop, so closing this session reports no loss.
    closed_watcher: JoinHandle<()>,
    runtime: &'static Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyFailure {
    Refused,
    /// A late answer can still mean the key arrived.
    NoAnswer,
}

/// Lives as long as the process. The portal client keeps one D-Bus connection
/// for the whole process, on the runtime that first opened it. A worker thread
/// runs it, because `Handle::block_on` alone drives no IO on a current-thread
/// runtime.
fn runtime() -> Result<&'static Runtime, Refusal> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("aria-portal")
                .enable_all()
                .build()
                .inspect_err(|error| tracing::warn!(%error, "failed to start the portal runtime"))
                .ok()
        })
        .as_ref()
        .ok_or(Refusal::Failed)
}

/// Connects in the background. Shutdown closes any pending consent dialog.
/// `on_closed` runs if the session later ends without this process closing it.
pub(super) fn connect_in_background(
    home: &Path,
    shutdown: &CancellationToken,
    on_closed: impl FnOnce() + Send + 'static,
    done: impl FnOnce(Result<PortalKeyboard, Refusal>) + Send + 'static,
) {
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(refusal) => return done(Err(refusal)),
    };
    let token = RestoreToken::at(home);
    let shutdown = shutdown.clone();
    runtime.spawn(async move {
        let started = start(&token, &shutdown).await;
        done(started.map(|(portal, session)| {
            let session = Arc::new(session);
            PortalKeyboard {
                portal,
                closed_watcher: runtime.spawn(watch_closed(Arc::clone(&session), on_closed)),
                session,
                runtime,
            }
        }));
    });
}

/// A stream that ends means the bus connection died, which ends the session too.
async fn watch_closed(session: Arc<Session<RemoteDesktop>>, on_closed: impl FnOnce()) {
    match session.receive_closed().await {
        Ok(closed) => {
            let mut closed = std::pin::pin!(closed);
            let _ = closed.next().await;
            on_closed();
        }
        Err(error) => tracing::warn!(%error, "cannot watch the portal session"),
    }
}

impl PortalKeyboard {
    pub(super) fn paste(&self) -> Result<(), SendFailure> {
        self.runtime.block_on(async {
            let mut pressed = Vec::new();
            let mut chord_sent = false;
            for (key, state) in PASTE_CHORD {
                if let Err(failure) = self.key(key, state).await {
                    for key in pressed.into_iter().rev() {
                        let _ = self.key(key, KeyState::Released).await;
                    }
                    return Err(send_failure(chord_sent, key, failure));
                }
                match state {
                    KeyState::Pressed => pressed.push(key),
                    KeyState::Released => pressed.retain(|held| *held != key),
                }
                chord_sent |= key == KEY_INSERT;
            }
            Ok(())
        })
    }

    async fn key(&self, key: i32, state: KeyState) -> Result<(), KeyFailure> {
        let sent =
            self.portal
                .notify_keyboard_keycode(&self.session, key, state, Default::default());
        match tokio::time::timeout(KEY_TIMEOUT, sent).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                tracing::warn!(%error, "the portal refused a key");
                Err(KeyFailure::Refused)
            }
            Err(_elapsed) => {
                tracing::warn!("the portal did not answer a key");
                Err(KeyFailure::NoAnswer)
            }
        }
    }
}

/// Shift alone pastes nothing, so only Insert can have reached the app.
fn send_failure(chord_sent: bool, key: i32, failure: KeyFailure) -> SendFailure {
    if chord_sent || (key == KEY_INSERT && failure == KeyFailure::NoAnswer) {
        SendFailure::PartlySent
    } else {
        SendFailure::NothingSent
    }
}

impl Drop for PortalKeyboard {
    /// Spawned, not awaited: the last owner may be on an async thread.
    fn drop(&mut self) {
        self.closed_watcher.abort();
        let session = Arc::clone(&self.session);
        self.runtime.spawn(async move { close(&session).await });
    }
}

async fn close(session: &Session<RemoteDesktop>) {
    let closed = tokio::time::timeout(CLOSE_TIMEOUT, session.close()).await;
    if !matches!(closed, Ok(Ok(()))) {
        tracing::debug!("the portal session did not close cleanly");
    }
}

async fn start(
    token: &RestoreToken,
    shutdown: &CancellationToken,
) -> Result<(RemoteDesktop, Session<RemoteDesktop>), Refusal> {
    let (portal, session) = tokio::select! {
        opened = open_session() => opened?,
        () = shutdown.cancelled() => return Err(Refusal::Failed),
    };
    let granted = tokio::select! {
        granted = grant_keyboard(&portal, &session, token) => granted,
        () = shutdown.cancelled() => Err(Refusal::Failed),
    };
    match granted {
        Ok(()) => Ok((portal, session)),
        Err(refusal) => {
            // Ends any open dialog. A retry would otherwise leave one open
            // session per ask.
            close(&session).await;
            Err(refusal)
        }
    }
}

async fn open_session() -> Result<(RemoteDesktop, Session<RemoteDesktop>), Refusal> {
    register().await;
    let portal = RemoteDesktop::new().await.map_err(|error| {
        tracing::info!(%error, "no RemoteDesktop portal");
        Refusal::Unsupported
    })?;
    let session = portal
        .create_session(Default::default())
        .await
        .map_err(failed)?;
    Ok((portal, session))
}

async fn grant_keyboard(
    portal: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
    token: &RestoreToken,
) -> Result<(), Refusal> {
    let saved = token.read();
    portal
        .select_devices(
            session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::KEYBOARD)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(saved.as_deref()),
        )
        .await
        .and_then(|request| request.response())
        .map_err(failed)?;
    let started = portal
        .start(session, None, Default::default())
        .await
        .and_then(|request| request.response())
        .map_err(|error| match error {
            ashpd::Error::Response(ResponseError::Cancelled) => {
                tracing::info!("the user declined keyboard control");
                Refusal::NotPermitted
            }
            error => failed(error),
        })?;
    if !started.devices().contains(DeviceType::KEYBOARD) {
        tracing::info!("the portal started without the keyboard");
        return Err(Refusal::NotPermitted);
    }
    // A token works once; each start hands out the next.
    if let Some(next) = started.restore_token() {
        token.write(next);
    }
    Ok(())
}

/// Names Aria in the consent dialog. A portal older than the registry
/// still works, unnamed.
async fn register() {
    let registered = match ashpd::AppID::try_from(APP_ID) {
        Ok(app_id) => ashpd::register_host_app(app_id).await,
        Err(error) => Err(error),
    };
    if let Err(error) = registered {
        tracing::info!(%error, "the portal did not register Aria by name");
    }
}

fn failed(error: ashpd::Error) -> Refusal {
    tracing::warn!(%error, "the RemoteDesktop portal failed");
    Refusal::Failed
}

/// The portal's restore token: it lets a session start without the
/// consent dialog, so it is kept like a credential.
struct RestoreToken {
    document: PrivateDocument,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredToken {
    restore_token: String,
}

impl RestoreToken {
    fn at(home: &Path) -> Self {
        Self {
            document: PrivateDocument::new(home.join(RELATIVE_PATH), MAX_TOKEN_BYTES),
        }
    }

    /// A missing or unreadable token means asking the user again.
    fn read(&self) -> Option<String> {
        match self.document.read::<StoredToken>() {
            Ok(stored) => stored.map(|stored| stored.restore_token),
            Err(error) => {
                tracing::warn!(%error, "the portal restore token is unreadable");
                None
            }
        }
    }

    fn write(&self, token: &str) {
        let stored = StoredToken {
            restore_token: token.to_owned(),
        };
        let written = self
            .document
            .prepare(&stored)
            .and_then(|write| write.commit())
            .and_then(|committed| committed.confirm_durability());
        if let Err(error) = written {
            tracing::warn!(%error, "the portal restore token was not saved; the next start asks again");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_token_reads_back() {
        let home = tempfile::tempdir().expect("test home");
        std::fs::create_dir(home.path().join(".aria")).expect("the Aria directory");
        let token = RestoreToken::at(home.path());
        assert_eq!(token.read(), None);
        token.write("first");
        token.write("second");
        assert_eq!(token.read().as_deref(), Some("second"));
    }

    #[test]
    fn the_portal_runtime_drives_timers_while_blocking() {
        let runtime = runtime().expect("a runtime");
        let task = runtime.spawn(async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            7
        });
        assert_eq!(runtime.block_on(task).expect("the task finished"), 7);
    }

    #[test]
    fn a_paste_counts_as_sent_once_insert_may_have_arrived() {
        let unsent = SendFailure::NothingSent;
        let sent = SendFailure::PartlySent;
        assert_eq!(
            send_failure(false, KEY_LEFTSHIFT, KeyFailure::NoAnswer),
            unsent
        );
        assert_eq!(send_failure(false, KEY_INSERT, KeyFailure::Refused), unsent);
        assert_eq!(send_failure(false, KEY_INSERT, KeyFailure::NoAnswer), sent);
        assert_eq!(send_failure(true, KEY_LEFTSHIFT, KeyFailure::Refused), sent);
    }

    #[test]
    fn the_app_id_is_a_valid_dbus_id() {
        assert!(ashpd::AppID::try_from(APP_ID).is_ok());
    }
}
