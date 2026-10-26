//! The shared-preferences service: read and revision-checked update.

use tonic::{Request, Response, Status};

use crate::ipc::wire;
use crate::preferences::{PreferencePatch, PreferenceStoreError};
use crate::primitives::error_chain;
use crate::rpc::{RpcErrorCode, RpcFailure};

use super::super::operation::CallError;
use super::super::preferences::{PreferencesCallError, PreferencesHandle};
use super::invalid_request;

#[derive(Clone)]
pub(in crate::daemon) struct PreferencesRpc {
    preferences: PreferencesHandle,
    vocabulary: std::sync::Arc<super::super::vocabulary::Runtime>,
}

impl PreferencesRpc {
    pub(in crate::daemon) fn new(
        preferences: PreferencesHandle,
        vocabulary: std::sync::Arc<super::super::vocabulary::Runtime>,
    ) -> Self {
        Self {
            preferences,
            vocabulary,
        }
    }
}

#[tonic::async_trait]
impl wire::preferences_service_server::PreferencesService for PreferencesRpc {
    async fn get(
        &self,
        _request: Request<wire::GetPreferencesRequest>,
    ) -> Result<Response<wire::PreferenceSnapshot>, Status> {
        const METHOD: &str = "PreferencesService/Get";
        match self.preferences.get().await {
            Ok(snapshot) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new((&snapshot).into()))
            }
            Err(error) => Err(super::failed(METHOD, failure("read", error))),
        }
    }

    async fn update(
        &self,
        request: Request<wire::UpdatePreferencesRequest>,
    ) -> Result<Response<wire::UpdatePreferencesResponse>, Status> {
        const METHOD: &str = "PreferencesService/Update";
        let (expected_revision, patch) = <(u64, PreferencePatch)>::try_from(request.into_inner())
            .map_err(|error| invalid_request(METHOD, &error))?;
        let refresh_vocabulary =
            patch.project_vocabulary.is_some() || patch.vocabulary_directory.is_some();
        match self.preferences.update(expected_revision, patch).await {
            Ok(update) => {
                if refresh_vocabulary
                    && let crate::preferences::UpdateResult::Updated(snapshot) = &update
                {
                    self.vocabulary
                        .prepare(if snapshot.preferences.project_vocabulary {
                            snapshot.preferences.vocabulary_directory.clone()
                        } else {
                            None
                        });
                }
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(update.into()))
            }
            Err(error) => Err(super::failed(METHOD, failure("update", error))),
        }
    }
}

fn failure(operation: &'static str, error: PreferencesCallError) -> RpcFailure {
    match error {
        CallError::Unavailable => RpcFailure::new(
            RpcErrorCode::ShuttingDown,
            "the daemon is not accepting preference requests",
        ),
        CallError::JobFailed => RpcFailure::new(
            RpcErrorCode::Internal,
            "the preferences operation ended unexpectedly",
        ),
        CallError::Runtime(error) => store_error(operation, error),
    }
}

pub(super) fn store_error(operation: &'static str, error: PreferenceStoreError) -> RpcFailure {
    if matches!(
        error,
        PreferenceStoreError::ChordInUse | PreferenceStoreError::InvalidModelId
    ) {
        return RpcFailure::new(RpcErrorCode::InvalidRequest, error.to_string());
    }
    let durability_unconfirmed =
        matches!(error, PreferenceStoreError::DurabilityUnconfirmed { .. });
    tracing::error!(operation, error = %error_chain(&error), "preference store operation failed");
    if durability_unconfirmed {
        RpcFailure::new(
            RpcErrorCode::DurabilityUnconfirmed,
            "preferences were updated, but durability could not be confirmed; fetch preferences before retrying",
        )
    } else {
        RpcFailure::new(
            RpcErrorCode::Internal,
            format!("failed to {operation} preferences"),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::daemon::testing::HostedDaemon;
    use crate::preferences::{PreferencePatch, UpdateResult};

    #[test]
    fn an_update_reaches_every_session_and_a_stale_one_conflicts() {
        let daemon = HostedDaemon::configured(|_| String::new());
        let writer = daemon.session();
        let reader = daemon.session();

        let before = reader.get_preferences().expect("preferences are readable");
        let patch = PreferencePatch {
            mute_output: Some(!before.preferences.mute_output),
            ..Default::default()
        };
        let updated = writer
            .update_preferences(before.revision, patch.clone())
            .expect("the update is answered");
        let stale = writer
            .update_preferences(before.revision, patch)
            .expect("the stale update is answered, not failed");

        let UpdateResult::Updated(after) = updated else {
            panic!("the current revision updates");
        };
        assert_eq!(reader.get_preferences().expect("readable"), after);
        assert!(matches!(stale, UpdateResult::Conflict(current) if current == after));
        drop((writer, reader));
        daemon.stop();
    }
}
