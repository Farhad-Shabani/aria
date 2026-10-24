//! The preferences a paired phone may change: the ones a relayed recording
//! reads. Everything else is refused here, so pairing grants no hotkey, no
//! Mac-side audio control, and no folder path on this Mac. Reads return the
//! whole snapshot; only writes are scoped.

use tonic::{Request, Response, Status};

use crate::ipc::wire;
use crate::rpc::{RpcErrorCode, RpcFailure};

use super::super::service::PreferencesRpc;

pub(in crate::daemon) struct LanPreferences {
    inner: PreferencesRpc,
}

impl LanPreferences {
    pub(in crate::daemon) fn new(inner: PreferencesRpc) -> Self {
        Self { inner }
    }
}

#[tonic::async_trait]
impl wire::preferences_service_server::PreferencesService for LanPreferences {
    async fn get(
        &self,
        request: Request<wire::GetPreferencesRequest>,
    ) -> Result<Response<wire::PreferenceSnapshot>, Status> {
        wire::preferences_service_server::PreferencesService::get(&self.inner, request).await
    }

    async fn update(
        &self,
        request: Request<wire::UpdatePreferencesRequest>,
    ) -> Result<Response<wire::UpdatePreferencesResponse>, Status> {
        let (metadata, extensions, mut update) = request.into_parts();
        if let Some(patch) = update.patch.take() {
            update.patch = Some(scoped(patch)?);
        }
        wire::preferences_service_server::PreferencesService::update(
            &self.inner,
            Request::from_parts(metadata, extensions, update),
        )
        .await
    }
}

/// Keeps the seven relay preferences and refuses the rest. Every proto field
/// is named, so a new one fails to compile until it is placed on one side.
/// Clearing the vocabulary folder is allowed; choosing one from a phone is
/// not. Copy output cannot stream, so it carries the delivery switch the Mac
/// app would have sent alongside.
pub(super) fn scoped(patch: wire::PreferencePatch) -> Result<wire::PreferencePatch, Status> {
    let wire::PreferencePatch {
        recognition_language,
        output_mode,
        delivery_mode,
        silence_duration_ms,
        own_voice_only,
        dictation_hotkey,
        ask_hotkey,
        prompt_list_hotkey,
        project_vocabulary,
        vocabulary_directory,
        dictation_history,
        history_audio_retention,
        mute_output,
        application_overrides,
        prompts,
    } = patch;
    let mac_only = [
        ("delivery_mode", delivery_mode.is_some()),
        ("silence_duration_ms", silence_duration_ms.is_some()),
        ("dictation_hotkey", dictation_hotkey.is_some()),
        ("ask_hotkey", ask_hotkey.is_some()),
        ("prompt_list_hotkey", prompt_list_hotkey.is_some()),
        ("mute_output", mute_output.is_some()),
        ("application_overrides", application_overrides.is_some()),
        ("prompts", prompts.is_some()),
    ];
    if let Some((field, _)) = mac_only.iter().find(|(_, set)| *set) {
        return Err(RpcFailure::new(
            RpcErrorCode::PermissionDenied,
            format!("{field} is only changed on the Mac"),
        )
        .into());
    }
    if vocabulary_directory
        .as_deref()
        .is_some_and(|path| !path.is_empty())
    {
        return Err(RpcFailure::new(
            RpcErrorCode::PermissionDenied,
            "choose the vocabulary folder on the Mac; a phone can only clear it",
        )
        .into());
    }
    let copies = output_mode == Some(wire::OutputMode::Copy as i32);
    Ok(wire::PreferencePatch {
        recognition_language,
        output_mode,
        delivery_mode: copies.then_some(wire::DeliveryMode::Complete as i32),
        own_voice_only,
        project_vocabulary,
        vocabulary_directory,
        dictation_history,
        history_audio_retention,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seven_relay_preferences_pass_unchanged() {
        let patch = wire::PreferencePatch {
            recognition_language: Some("auto".to_owned()),
            output_mode: Some(wire::OutputMode::Insert as i32),
            own_voice_only: Some(true),
            project_vocabulary: Some(true),
            vocabulary_directory: Some(String::new()),
            dictation_history: Some(false),
            history_audio_retention: Some(true),
            ..Default::default()
        };
        assert_eq!(scoped(patch.clone()).unwrap(), patch);
    }

    #[test]
    fn copy_output_switches_delivery_to_complete() {
        let scoped = scoped(wire::PreferencePatch {
            output_mode: Some(wire::OutputMode::Copy as i32),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            scoped.delivery_mode,
            Some(wire::DeliveryMode::Complete as i32)
        );
    }

    #[test]
    fn mac_only_fields_are_refused() {
        let cases = [
            wire::PreferencePatch {
                delivery_mode: Some(wire::DeliveryMode::Streaming as i32),
                ..Default::default()
            },
            wire::PreferencePatch {
                silence_duration_ms: Some(500),
                ..Default::default()
            },
            wire::PreferencePatch {
                dictation_hotkey: Some(wire::HotkeyKey::Fn as i32),
                ..Default::default()
            },
            wire::PreferencePatch {
                ask_hotkey: Some(wire::Hotkey::default()),
                ..Default::default()
            },
            wire::PreferencePatch {
                prompt_list_hotkey: Some(wire::Hotkey::default()),
                ..Default::default()
            },
            wire::PreferencePatch {
                prompts: Some(wire::Prompts::default()),
                ..Default::default()
            },
            wire::PreferencePatch {
                mute_output: Some(true),
                ..Default::default()
            },
            wire::PreferencePatch {
                application_overrides: Some(wire::ApplicationOverrides::default()),
                ..Default::default()
            },
            wire::PreferencePatch {
                vocabulary_directory: Some("/Users/someone/project".to_owned()),
                ..Default::default()
            },
        ];
        for patch in cases {
            let status = scoped(patch).expect_err("a Mac-only field must be refused");
            assert_eq!(status.code(), tonic::Code::PermissionDenied);
            assert_eq!(
                crate::ipc::protocol::rpc_failure(&status).unwrap().code,
                RpcErrorCode::PermissionDenied
            );
        }
    }
}
