//! The settings a paired phone may change: the ones a relayed recording
//! reads. Everything else is refused here, so pairing grants no Aria key, no
//! Mac-side audio control, and no folder path on this Mac. Prompt paths, the
//! vocabulary folder and per-app choices stay on the Mac in reads too.

use tonic::{Request, Response, Status};

use crate::ipc::wire;
use crate::rpc::{RpcErrorCode, RpcFailure};
use crate::settings::{Key, Scope};

use super::super::service::SettingsRpc;
use super::redacted;

pub(in crate::daemon) struct LanSettings {
    inner: SettingsRpc,
}

impl LanSettings {
    pub(in crate::daemon) fn new(inner: SettingsRpc) -> Self {
        Self { inner }
    }
}

#[tonic::async_trait]
impl wire::settings_service_server::SettingsService for LanSettings {
    async fn get(
        &self,
        request: Request<wire::GetSettingsRequest>,
    ) -> Result<Response<wire::SettingsSnapshot>, Status> {
        let mut response =
            wire::settings_service_server::SettingsService::get(&self.inner, request)
                .await
                .map_err(redacted)?;
        without_mac_choices(response.get_mut());
        Ok(response)
    }

    /// Only the settings reach a phone, never the restart keys a write
    /// left pending on the Mac.
    async fn update(
        &self,
        request: Request<wire::UpdateSettingsRequest>,
    ) -> Result<Response<wire::UpdateSettingsResponse>, Status> {
        let (metadata, extensions, mut update) = request.into_parts();
        if let Some(patch) = update.patch.take() {
            update.patch = Some(scoped(patch)?);
        }
        let mut response = wire::settings_service_server::SettingsService::update(
            &self.inner,
            Request::from_parts(metadata, extensions, update),
        )
        .await
        .map_err(redacted)?;
        let update = response.get_mut();
        update.restart_required.clear();
        match &mut update.result {
            Some(wire::update_settings_response::Result::Snapshot(snapshot)) => {
                without_mac_choices(snapshot);
            }
            Some(wire::update_settings_response::Result::Conflict(conflict)) => {
                if let Some(current) = &mut conflict.current {
                    without_mac_choices(current);
                }
            }
            None => {}
        }
        Ok(response)
    }

    /// The status names keys and quotes the file, which stay on the Mac.
    async fn get_status(
        &self,
        _request: Request<wire::GetSettingsStatusRequest>,
    ) -> Result<Response<wire::GetSettingsStatusResponse>, Status> {
        Err(RpcFailure::new(
            RpcErrorCode::PermissionDenied,
            "the settings status is read on the Mac",
        )
        .into())
    }
}

/// A path and an app's bundle ID can name the person. An empty folder still
/// tells the phone that one is set, so it can offer to clear it.
fn without_mac_choices(snapshot: &mut wire::SettingsSnapshot) {
    if let Some(settings) = &mut snapshot.settings {
        if settings.vocabulary_dir.is_some() {
            settings.vocabulary_dir = Some(String::new());
        }
        settings.prompts = Some(wire::Prompts::default());
        settings.application_overrides = Some(wire::ApplicationOverrides::default());
    }
}

/// Keeps the relay settings, which the key table scopes to the phone,
/// and refuses the rest. Every proto field is named, so a new one fails to
/// compile until it names the key it writes. Clearing the vocabulary folder
/// is allowed; choosing one from a phone is not. Copy output cannot stream,
/// so it carries the delivery switch the Mac app would have sent alongside.
pub(super) fn scoped(patch: wire::SettingsPatch) -> Result<wire::SettingsPatch, Status> {
    let wire::SettingsPatch {
        output_mode,
        delivery_mode,
        silence_duration_ms,
        auto_stop_ms,
        mute_sound,
        cleanup,
        backtrack,
        own_voice_only,
        vocabulary_enabled,
        vocabulary_dir,
        history_enabled,
        history_keep_audio,
        assistant_id,
        assistant_models,
        memory_enabled,
        prompts,
        application_overrides,
        aria_key,
        ask_shortcut,
        prompt_list_shortcut,
        stt_language,
        tts_rate,
    } = patch;
    let written = [
        (Key::DictationOutputMode, output_mode.is_some()),
        (Key::DictationDeliveryMode, delivery_mode.is_some()),
        (
            Key::DictationSilenceDurationMs,
            silence_duration_ms.is_some(),
        ),
        (Key::DictationAutoStopMs, auto_stop_ms.is_some()),
        (Key::DictationMuteSound, mute_sound.is_some()),
        (Key::DictationCleanup, cleanup.is_some()),
        (Key::DictationBacktrack, backtrack.is_some()),
        (Key::DictationOwnVoiceOnly, own_voice_only.is_some()),
        (
            Key::DictationVocabularyEnabled,
            vocabulary_enabled.is_some(),
        ),
        (Key::DictationVocabularyDir, vocabulary_dir.is_some()),
        (Key::HistoryEnabled, history_enabled.is_some()),
        (Key::HistoryKeepAudio, history_keep_audio.is_some()),
        (Key::AssistantId, assistant_id.is_some()),
        (Key::AssistantModels, !assistant_models.is_empty()),
        (Key::MemoryEnabled, memory_enabled.is_some()),
        (Key::Prompts, prompts.is_some()),
        (Key::ApplicationOverrides, application_overrides.is_some()),
        (Key::ShortcutsAriaKey, aria_key.is_some()),
        (Key::ShortcutsAsk, ask_shortcut.is_some()),
        (Key::ShortcutsPromptList, prompt_list_shortcut.is_some()),
        (Key::SttLanguage, stt_language.is_some()),
        (Key::TtsRate, tts_rate.is_some()),
    ];
    if let Some((key, _)) = written
        .iter()
        .find(|(key, set)| *set && key.scope() != Scope::Phone)
    {
        return Err(RpcFailure::new(
            RpcErrorCode::PermissionDenied,
            format!("{key} is only changed on the Mac"),
        )
        .into());
    }
    if vocabulary_dir
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
    Ok(wire::SettingsPatch {
        stt_language,
        output_mode,
        delivery_mode: copies.then_some(wire::DeliveryMode::Complete as i32),
        own_voice_only,
        vocabulary_enabled,
        vocabulary_dir,
        history_enabled,
        history_keep_audio,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio_util::sync::CancellationToken;

    use super::super::super::settings::channel;
    use super::super::super::tasks::Supervised;
    use super::super::SETTINGS_NEED_FIXING;
    use super::*;
    use crate::settings::SettingsStatus;
    use crate::settings::SettingsStore;
    use wire::settings_service_server::SettingsService;

    struct RunningLan {
        lan: LanSettings,
        shutdown: CancellationToken,
        actor: tokio::task::JoinHandle<()>,
        _status: tokio::sync::watch::Sender<SettingsStatus>,
    }

    fn serve(store: Arc<SettingsStore>) -> RunningLan {
        let (handle, actor) = channel(store);
        let shutdown = CancellationToken::new();
        let actor = tokio::spawn(actor.run(shutdown.clone()));
        let (status, receiver) = tokio::sync::watch::channel(SettingsStatus::default());
        RunningLan {
            lan: LanSettings::new(SettingsRpc::new(handle, receiver)),
            shutdown,
            actor,
            _status: status,
        }
    }

    impl RunningLan {
        async fn stop(self) {
            self.shutdown.cancel();
            self.actor.await.unwrap();
        }
    }

    /// A fault in the file reaches the phone as a fixed sentence: nothing
    /// the file says, and no place in it.
    #[tokio::test]
    async fn a_phone_learns_only_that_the_mac_needs_fixing() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("settings.toml");
        let store = Arc::new(SettingsStore::open(&path).unwrap());
        std::fs::write(&path, "[tts]\nrate = \"secret-value\"\n").unwrap();
        let running = serve(store);

        let refused = running
            .lan
            .update(Request::new(wire::UpdateSettingsRequest {
                expected_revision: 0,
                patch: Some(wire::SettingsPatch {
                    own_voice_only: Some(true),
                    ..Default::default()
                }),
            }))
            .await
            .expect_err("an invalid file is never written over");
        running.stop().await;

        let failure = crate::ipc::protocol::rpc_failure(&refused).expect("an Aria failure");
        assert_eq!(failure.code, RpcErrorCode::SettingsInvalid);
        assert_eq!(failure.message, SETTINGS_NEED_FIXING);
        assert_eq!(failure.settings_position, None);
        assert_eq!(refused.message(), SETTINGS_NEED_FIXING);
    }

    /// A read-only or locked file names its path on the Mac, and
    /// the home directory stays off the network.
    #[tokio::test]
    async fn no_lan_failure_holds_the_home_path() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let home_text = home.path().display().to_string();
        let path = home.path().join("settings.toml");
        std::fs::write(&path, "").unwrap();
        let store = Arc::new(SettingsStore::open(&path).unwrap());
        let revision = store.snapshot().unwrap().revision;
        let running = serve(store);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();

        let refused = running
            .lan
            .update(Request::new(wire::UpdateSettingsRequest {
                expected_revision: revision,
                patch: Some(wire::SettingsPatch {
                    history_enabled: Some(false),
                    ..Default::default()
                }),
            }))
            .await
            .expect_err("a read-only file refuses the write");
        running.stop().await;

        assert!(
            !refused.message().contains(&home_text),
            "{}",
            refused.message()
        );
        let failure = crate::ipc::protocol::rpc_failure(&refused).expect("an Aria failure");
        assert!(!failure.message.contains(&home_text), "{}", failure.message);
    }

    #[test]
    fn only_a_code_known_to_be_safe_keeps_its_message() {
        let path = "/Users/you/.aria/settings.lock";
        for code in [
            RpcErrorCode::Busy,
            RpcErrorCode::Internal,
            RpcErrorCode::PermissionDenied,
            RpcErrorCode::SettingsInvalid,
            RpcErrorCode::TranscriptionFailed,
            RpcErrorCode::Unknown(999),
        ] {
            let status = redacted(RpcFailure::new(code, path).into());
            assert!(!status.message().contains(path), "{code:?}");
        }
        let kept = redacted(
            RpcFailure::new(RpcErrorCode::InvalidRequest, "memory.enabled is refused").into(),
        );
        assert_eq!(kept.message(), "memory.enabled is refused");
        let bare = redacted(Status::internal(path));
        assert!(!bare.message().contains(path));
    }

    /// A phone reads the settings and never the settings status,
    /// which names keys and quotes the file.
    #[tokio::test]
    async fn a_phone_reads_the_settings_and_not_the_status() {
        let home = tempfile::tempdir().unwrap();
        let store = Arc::new(SettingsStore::open(home.path().join("settings.toml")).unwrap());
        let running = serve(store);

        let snapshot = running
            .lan
            .get(Request::new(wire::GetSettingsRequest {}))
            .await
            .expect("the view is readable")
            .into_inner();
        let refused = running
            .lan
            .get_status(Request::new(wire::GetSettingsStatusRequest {}))
            .await
            .expect_err("the status stays on the Mac");
        running.stop().await;

        assert!(snapshot.settings.is_some());
        assert_eq!(refused.code(), tonic::Code::PermissionDenied);
    }

    #[test]
    fn a_phone_snapshot_holds_no_paths_or_app_choices() {
        let mut snapshot = wire::SettingsSnapshot {
            revision: 1,
            settings: Some(wire::Settings {
                prompts: Some(wire::Prompts {
                    entries: vec![wire::Prompt {
                        file: "/Users/you/reply.md".to_owned(),
                        ..Default::default()
                    }],
                }),
                vocabulary_dir: Some("/Users/you/project".to_owned()),
                ..Default::default()
            }),
        };

        without_mac_choices(&mut snapshot);

        let settings = snapshot.settings.unwrap();
        assert_eq!(settings.prompts, Some(wire::Prompts::default()));
        assert_eq!(settings.vocabulary_dir.as_deref(), Some(""));
        assert_eq!(
            settings.application_overrides,
            Some(wire::ApplicationOverrides::default())
        );
    }

    #[test]
    fn the_seven_relay_settings_pass_unchanged() {
        let patch = wire::SettingsPatch {
            stt_language: Some("auto".to_owned()),
            output_mode: Some(wire::OutputMode::Insert as i32),
            own_voice_only: Some(true),
            vocabulary_enabled: Some(true),
            vocabulary_dir: Some(String::new()),
            history_enabled: Some(false),
            history_keep_audio: Some(true),
            ..Default::default()
        };
        assert_eq!(scoped(patch.clone()).unwrap(), patch);
    }

    #[test]
    fn copy_output_switches_delivery_to_complete() {
        let scoped = scoped(wire::SettingsPatch {
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
            wire::SettingsPatch {
                assistant_id: Some(wire::AssistantId::Codex as i32),
                ..Default::default()
            },
            wire::SettingsPatch {
                assistant_models: vec![wire::AssistantModelSetting {
                    assistant_id: wire::AssistantId::ClaudeCode as i32,
                    model_id: "opus".to_owned(),
                }],
                ..Default::default()
            },
            wire::SettingsPatch {
                assistant_models: vec![wire::AssistantModelSetting {
                    assistant_id: wire::AssistantId::Codex as i32,
                    model_id: String::new(),
                }],
                ..Default::default()
            },
            wire::SettingsPatch {
                delivery_mode: Some(wire::DeliveryMode::Streaming as i32),
                ..Default::default()
            },
            wire::SettingsPatch {
                silence_duration_ms: Some(500),
                ..Default::default()
            },
            wire::SettingsPatch {
                auto_stop_ms: Some(0),
                ..Default::default()
            },
            wire::SettingsPatch {
                aria_key: Some(wire::AriaKey::Fn as i32),
                ..Default::default()
            },
            wire::SettingsPatch {
                ask_shortcut: Some(wire::Shortcut::default()),
                ..Default::default()
            },
            wire::SettingsPatch {
                prompt_list_shortcut: Some(wire::Shortcut::default()),
                ..Default::default()
            },
            wire::SettingsPatch {
                prompts: Some(wire::Prompts::default()),
                ..Default::default()
            },
            wire::SettingsPatch {
                mute_sound: Some(true),
                ..Default::default()
            },
            wire::SettingsPatch {
                application_overrides: Some(wire::ApplicationOverrides::default()),
                ..Default::default()
            },
            wire::SettingsPatch {
                memory_enabled: Some(true),
                ..Default::default()
            },
            wire::SettingsPatch {
                backtrack: Some(true),
                ..Default::default()
            },
            wire::SettingsPatch {
                cleanup: Some(true),
                ..Default::default()
            },
            wire::SettingsPatch {
                tts_rate: Some(2.0),
                ..Default::default()
            },
            wire::SettingsPatch {
                vocabulary_dir: Some("/Users/someone/project".to_owned()),
                ..Default::default()
            },
        ];
        for patch in cases {
            let status = scoped(patch).expect_err("a Mac-only field must be refused");
            assert!(status.message().contains("Mac"), "{}", status.message());
            assert_eq!(status.code(), tonic::Code::PermissionDenied);
            assert_eq!(
                crate::ipc::protocol::rpc_failure(&status).unwrap().code,
                RpcErrorCode::PermissionDenied
            );
        }
    }
}
