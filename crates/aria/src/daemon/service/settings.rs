//! The settings service: read and revision-checked update.

use tokio::sync::watch;
use tonic::{Request, Response, Status};

use crate::ipc::wire;
use crate::primitives::error_chain;
use crate::rpc::{RpcErrorCode, RpcFailure};
use crate::settings::{SettingsError, SettingsStatus};
use crate::settings::{SettingsPatch, StoreError};

use super::super::operation::CallError;
use super::super::settings::{SettingsCallError, SettingsHandle};
use super::invalid_request;

/// A project vocabulary change reaches the vocabulary runtime through the
/// settings cache's watch, the same way a hand edit does.
#[derive(Clone)]
pub(in crate::daemon) struct SettingsRpc {
    settings: SettingsHandle,
    status: watch::Receiver<SettingsStatus>,
}

impl SettingsRpc {
    pub(in crate::daemon) fn new(
        settings: SettingsHandle,
        status: watch::Receiver<SettingsStatus>,
    ) -> Self {
        Self { settings, status }
    }
}

#[tonic::async_trait]
impl wire::settings_service_server::SettingsService for SettingsRpc {
    async fn get(
        &self,
        _request: Request<wire::GetSettingsRequest>,
    ) -> Result<Response<wire::SettingsSnapshot>, Status> {
        const METHOD: &str = "SettingsService/Get";
        match self.settings.get().await {
            Ok(snapshot) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new((&snapshot).into()))
            }
            Err(error) => Err(super::failed(METHOD, failure("read", error))),
        }
    }

    async fn update(
        &self,
        request: Request<wire::UpdateSettingsRequest>,
    ) -> Result<Response<wire::UpdateSettingsResponse>, Status> {
        const METHOD: &str = "SettingsService/Update";
        let (expected_revision, patch) = <(u64, SettingsPatch)>::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        match self.settings.update(expected_revision, patch).await {
            Ok(update) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(update.into()))
            }
            Err(error) => Err(super::failed(METHOD, failure("update", error))),
        }
    }

    async fn get_status(
        &self,
        _request: Request<wire::GetSettingsStatusRequest>,
    ) -> Result<Response<wire::GetSettingsStatusResponse>, Status> {
        const METHOD: &str = "SettingsService/GetStatus";
        let status = wire::SettingsStatus::from(&*self.status.borrow());
        tracing::info!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(wire::GetSettingsStatusResponse {
            status: Some(status),
        }))
    }
}

fn failure(operation: &'static str, error: SettingsCallError) -> RpcFailure {
    match error {
        CallError::Unavailable => RpcFailure::new(
            RpcErrorCode::ShuttingDown,
            "the daemon is not accepting settings requests",
        ),
        CallError::JobFailed => RpcFailure::new(
            RpcErrorCode::Internal,
            "the settings operation ended unexpectedly",
        ),
        CallError::Runtime(error) => store_error(operation, error),
    }
}

pub(super) fn store_error(operation: &'static str, error: StoreError) -> RpcFailure {
    let code = match &error {
        StoreError::FileInvalid(fault) => {
            return RpcFailure::new(RpcErrorCode::SettingsInvalid, error_chain(&error))
                .with_settings_position(fault.position());
        }
        // A change the file cannot take names the key, never the value.
        StoreError::Settings(SettingsError::Invalid { key, .. }) => {
            return RpcFailure::new(
                RpcErrorCode::InvalidRequest,
                format!("{key} does not accept that value"),
            );
        }
        StoreError::Settings(SettingsError::Parse(_)) => {
            return RpcFailure::new(
                RpcErrorCode::InvalidRequest,
                "the change does not fit settings.toml",
            );
        }
        StoreError::BindingInUse | StoreError::InvalidModelId => RpcErrorCode::InvalidRequest,
        StoreError::ChangedRecently | StoreError::Settings(SettingsError::Locked { .. }) => {
            RpcErrorCode::Busy
        }
        StoreError::Settings(SettingsError::ReadOnly { .. }) => RpcErrorCode::PermissionDenied,
        StoreError::Settings(
            SettingsError::Io { .. }
            | SettingsError::TooLarge { .. }
            | SettingsError::InvalidUtf8 { .. }
            | SettingsError::NotAFile { .. }
            | SettingsError::TornRead { .. }
            | SettingsError::UnknownKey(_)
            | SettingsError::SerializedTooLarge
            | SettingsError::Encode { .. }
            | SettingsError::Freeze(_)
            | SettingsError::Thread(_)
            | SettingsError::KeyAndAlias { .. },
        ) => RpcErrorCode::Internal,
    };
    if code != RpcErrorCode::Internal {
        return RpcFailure::new(code, error_chain(&error));
    }
    tracing::error!(operation, error = %error_chain(&error), "settings store operation failed");
    RpcFailure::new(
        RpcErrorCode::Internal,
        format!("failed to {operation} settings"),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::daemon::DaemonEvent;
    use crate::daemon::testing::HostedDaemon;
    use crate::ipc::ClientError;
    use crate::settings::Position;
    use crate::settings::{SettingsPatch, UpdateResult};

    const INVALID: &str = "[memory]\nenabled = maybe\n";

    fn mute() -> SettingsPatch {
        SettingsPatch {
            mute_sound: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn a_daemon_start_writes_the_schema_beside_the_settings() {
        let daemon = HostedDaemon::configured(|_| String::new());

        assert!(
            daemon
                .settings_path()
                .with_file_name("settings.schema.json")
                .is_file()
        );
    }

    /// Nothing is written over an edit in progress, and the client
    /// learns where the fault is.
    #[test]
    fn a_write_over_an_invalid_file_is_refused_with_its_place() {
        let daemon = HostedDaemon::configured(|_| String::new());
        let session = daemon.session();
        let before = session.get_settings().expect("readable");
        std::fs::write(daemon.settings_path(), INVALID).expect("a hand edit");

        let refused = session
            .update_settings(before.revision, mute())
            .expect_err("an invalid file is never written over");

        let ClientError::Rpc { code, message } = refused else {
            panic!("{refused}");
        };
        assert_eq!(code, RpcErrorCode::SettingsInvalid);
        assert!(message.contains("line 2, column 11"), "{message}");
        assert_eq!(
            std::fs::read_to_string(daemon.settings_path()).expect("read"),
            INVALID
        );
        drop(session);
        daemon.stop();
    }

    #[test]
    fn a_refused_write_carries_the_position_as_a_field() {
        let fault = crate::settings::Loaded::parse(INVALID, std::path::Path::new("/home"))
            .expect_err("invalid");
        let failure = store_error("update", StoreError::FileInvalid(fault));
        assert_eq!(failure.code, RpcErrorCode::SettingsInvalid);
        assert_eq!(
            failure.settings_position,
            Some(Position {
                line: 2,
                column: 11
            })
        );
    }

    /// The poll thread's reload reaches every subscriber without a request.
    #[test]
    fn a_hand_edit_reaches_subscribers_as_a_status() {
        let daemon = HostedDaemon::configured(|_| String::new());
        let session = daemon.session();
        let events = session.events().expect("a subscriber");
        let (sender, received) = std::sync::mpsc::channel();
        let watcher = std::thread::spawn(move || {
            for event in events {
                if let Ok(DaemonEvent::Settings(status)) = event {
                    let _ = sender.send(status);
                    return;
                }
            }
        });
        std::fs::write(daemon.settings_path(), INVALID).expect("a hand edit");

        let pushed = received
            .recv_timeout(Duration::from_secs(5))
            .expect("the status arrives within the poll");

        let fault = pushed.fault.expect("a fault");
        assert_eq!(
            fault.position,
            Some(Position {
                line: 2,
                column: 11
            })
        );
        assert_eq!(
            session.settings_status().expect("readable").fault,
            Some(fault)
        );
        drop(session);
        daemon.stop();
        watcher.join().expect("the watcher ends with the stream");
    }

    #[test]
    fn an_update_reaches_every_session_and_a_stale_one_conflicts() {
        let daemon = HostedDaemon::configured(|_| String::new());
        let writer = daemon.session();
        let reader = daemon.session();

        let before = reader.get_settings().expect("settings are readable");
        let patch = SettingsPatch {
            mute_sound: Some(!before.settings.mute_sound),
            ..Default::default()
        };
        let updated = writer
            .update_settings(before.revision, patch.clone())
            .expect("the update is answered");
        let stale = writer
            .update_settings(before.revision, patch)
            .expect("the stale update is answered, not failed");

        let UpdateResult::Updated {
            snapshot: after, ..
        } = updated
        else {
            panic!("the current revision updates");
        };
        assert_eq!(reader.get_settings().expect("readable"), after);
        assert!(matches!(stale, UpdateResult::Conflict(current) if current == after));
        drop((writer, reader));
        daemon.stop();
    }
}
