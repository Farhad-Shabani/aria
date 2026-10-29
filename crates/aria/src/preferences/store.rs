use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::watch;

use super::{
    ApplicationOverrides, Chord, Language, PreferenceStoreError, Prompts, UserPreferences, file,
};
use crate::audio::{AutoStop, SilenceDuration};
use crate::config::AssistantId;
use crate::delivery::OutputMode;
use crate::dictation::DeliveryMode;
use crate::hotkey::HotkeyKey;

const DEFAULT_PREFERENCE_RELATIVE_PATH: &str = ".aria/preferences.json";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreferenceSnapshot {
    pub revision: u64,
    pub preferences: UserPreferences,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreferencePatch {
    pub assistant_id: Option<AssistantId>,
    pub assistant_models: BTreeMap<AssistantId, String>,
    pub dictation_history: Option<bool>,
    pub history_audio_retention: Option<bool>,
    pub mute_output: Option<bool>,
    pub recognition_language: Option<Language>,
    pub output_mode: Option<OutputMode>,
    pub delivery_mode: Option<DeliveryMode>,
    pub silence_duration: Option<SilenceDuration>,
    pub auto_stop: Option<Option<AutoStop>>,
    pub own_voice_only: Option<bool>,
    pub dictation_hotkey: Option<HotkeyKey>,
    pub ask_hotkey: Option<Option<Chord>>,
    pub prompt_list_hotkey: Option<Option<Chord>>,
    pub project_vocabulary: Option<bool>,
    pub vocabulary_directory: Option<Option<crate::vocabulary::Directory>>,
    /// Replaces every entry.
    pub application_overrides: Option<ApplicationOverrides>,
    pub prompts: Option<Prompts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateResult {
    Updated(PreferenceSnapshot),
    Conflict(PreferenceSnapshot),
}

pub(crate) struct PreferenceStore {
    path: PathBuf,
    state: Mutex<PreferenceSnapshot>,
    /// Fires only when the key changes, so the hotkey listener is not
    /// re-armed by unrelated preference writes.
    dictation_hotkey: watch::Sender<HotkeyKey>,
}

impl PreferenceStore {
    pub(crate) fn default_path(home: &Path) -> PathBuf {
        home.join(DEFAULT_PREFERENCE_RELATIVE_PATH)
    }

    pub(crate) fn open(path: impl Into<PathBuf>) -> Result<Self, PreferenceStoreError> {
        let path = path.into();
        let snapshot = file::load(&path)?;
        let (dictation_hotkey, _) = watch::channel(snapshot.preferences.dictation_hotkey);
        Ok(Self {
            path,
            state: Mutex::new(snapshot),
            dictation_hotkey,
        })
    }

    pub(crate) fn dictation_hotkey(&self) -> watch::Receiver<HotkeyKey> {
        self.dictation_hotkey.subscribe()
    }

    pub(crate) fn snapshot(&self) -> Result<PreferenceSnapshot, PreferenceStoreError> {
        Ok(self.lock_state()?.clone())
    }

    pub(crate) fn update(
        &self,
        expected_revision: u64,
        patch: PreferencePatch,
    ) -> Result<UpdateResult, PreferenceStoreError> {
        let mut current = self.lock_state()?;
        if current.revision != expected_revision {
            return Ok(UpdateResult::Conflict(current.clone()));
        }

        let preferences = patch.apply_to(&current.preferences);
        preferences.validate_assistant_models()?;
        if preferences.shares_a_chord() {
            return Err(PreferenceStoreError::ChordInUse);
        }
        if preferences == current.preferences {
            return Ok(UpdateResult::Updated(current.clone()));
        }

        let revision = current
            .revision
            .checked_add(1)
            .ok_or(PreferenceStoreError::RevisionExhausted)?;
        let updated = PreferenceSnapshot {
            revision,
            preferences,
        };
        let prepared = file::prepare(&self.path, &updated)?;
        let committed = file::commit(prepared, &self.path)?;
        *current = updated.clone();
        let key = updated.preferences.dictation_hotkey;
        self.dictation_hotkey.send_if_modified(|seen| {
            let changed = *seen != key;
            *seen = key;
            changed
        });
        committed.confirm_durability()?;
        Ok(UpdateResult::Updated(updated))
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, PreferenceSnapshot>, PreferenceStoreError> {
        self.state
            .lock()
            .map_err(|_| PreferenceStoreError::LockPoisoned)
    }
}

impl PreferencePatch {
    fn apply_to(self, current: &UserPreferences) -> UserPreferences {
        let output_mode = self.output_mode.unwrap_or(current.output_mode);
        let delivery_mode = self.delivery_mode.unwrap_or(current.delivery_mode);
        let mut assistant_models = current.assistant_models.clone();
        assistant_models.extend(self.assistant_models);
        assistant_models.retain(|_, model_id| !model_id.is_empty());
        UserPreferences {
            assistant_id: self.assistant_id.or(current.assistant_id),
            assistant_models,
            dictation_history: self.dictation_history.unwrap_or(current.dictation_history),
            history_audio_retention: self
                .history_audio_retention
                .unwrap_or(current.history_audio_retention),
            mute_output: self.mute_output.unwrap_or(current.mute_output),
            recognition_language: self
                .recognition_language
                .unwrap_or_else(|| current.recognition_language.clone()),
            output_mode,
            delivery_mode,
            silence_duration: self.silence_duration.unwrap_or(current.silence_duration),
            auto_stop: self.auto_stop.unwrap_or(current.auto_stop),
            own_voice_only: self.own_voice_only.unwrap_or(current.own_voice_only),
            dictation_hotkey: self.dictation_hotkey.unwrap_or(current.dictation_hotkey),
            ask_hotkey: self.ask_hotkey.unwrap_or(current.ask_hotkey),
            prompt_list_hotkey: self
                .prompt_list_hotkey
                .unwrap_or(current.prompt_list_hotkey),
            project_vocabulary: self
                .project_vocabulary
                .unwrap_or(current.project_vocabulary),
            vocabulary_directory: self
                .vocabulary_directory
                .unwrap_or_else(|| current.vocabulary_directory.clone()),
            application_overrides: self
                .application_overrides
                .unwrap_or_else(|| current.application_overrides.clone())
                .without_inert(output_mode, delivery_mode),
            prompts: self.prompts.unwrap_or_else(|| current.prompts.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_models_persist_independently_and_empty_resets_to_default() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("preferences.json");
        let store = PreferenceStore::open(&path).unwrap();
        let original = store.snapshot().unwrap();
        assert_eq!(original.preferences.model_id(AssistantId::ClaudeCode), None);
        assert_eq!(original.preferences.model_id(AssistantId::Codex), None);
        store
            .update(
                0,
                PreferencePatch {
                    assistant_models: [
                        (AssistantId::ClaudeCode, "claude-example".to_owned()),
                        (AssistantId::Codex, "codex-example".to_owned()),
                    ]
                    .into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let reopened = PreferenceStore::open(&path).unwrap();
        reopened
            .update(
                1,
                PreferencePatch {
                    assistant_id: Some(AssistantId::Codex),
                    assistant_models: [(AssistantId::ClaudeCode, String::new())].into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let preferences = PreferenceStore::open(&path)
            .unwrap()
            .snapshot()
            .unwrap()
            .preferences;
        assert_eq!(preferences.model_id(AssistantId::ClaudeCode), None);
        assert_eq!(
            preferences.model_id(AssistantId::Codex),
            Some("codex-example")
        );
        assert_eq!(preferences.assistant_id, Some(AssistantId::Codex));
    }

    #[test]
    fn invalid_model_ids_leave_preferences_unchanged() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("preferences.json");
        let store = PreferenceStore::open(&path).unwrap();
        store
            .update(
                0,
                PreferencePatch {
                    mute_output: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for invalid in [
            "two words".to_owned(),
            "line\nbreak".to_owned(),
            "--flag".to_owned(),
            "x".repeat(201),
        ] {
            assert!(matches!(
                store.update(
                    1,
                    PreferencePatch {
                        assistant_models: [(AssistantId::Codex, invalid.clone())].into(),
                        ..Default::default()
                    }
                ),
                Err(PreferenceStoreError::InvalidModelId)
            ));
            assert_eq!(store.snapshot().unwrap().revision, 1);
            stored["preferences"]["assistant_models"]["codex"] = serde_json::Value::String(invalid);
            std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
            assert!(matches!(
                PreferenceStore::open(&path),
                Err(PreferenceStoreError::InvalidModelId)
            ));
        }
    }

    #[test]
    fn the_assistant_stays_inherited_until_explicitly_selected() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("preferences.json");
        let store = PreferenceStore::open(&path).unwrap();
        assert_eq!(store.snapshot().unwrap().preferences.assistant_id, None);
        store
            .update(
                0,
                PreferencePatch {
                    mute_output: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let reopened = PreferenceStore::open(&path).unwrap();
        assert_eq!(reopened.snapshot().unwrap().preferences.assistant_id, None);
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(stored["preferences"].get("assistant_id").is_none());

        reopened
            .update(
                1,
                PreferencePatch {
                    assistant_id: Some(AssistantId::Codex),
                    ..Default::default()
                },
            )
            .unwrap();
        let reopened = PreferenceStore::open(&path).unwrap();
        assert_eq!(
            reopened.snapshot().unwrap().preferences.assistant_id,
            Some(AssistantId::Codex)
        );
        reopened
            .update(
                2,
                PreferencePatch {
                    mute_output: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            reopened.snapshot().unwrap().preferences.assistant_id,
            Some(AssistantId::Codex)
        );
    }

    #[test]
    fn history_audio_retention_is_opt_in_and_snapshotted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences.json");
        let store = PreferenceStore::open(&path).unwrap();
        let original = store.snapshot().unwrap().preferences;
        assert!(!original.history_audio_retention);
        let settings = original.resolve_dictation(
            crate::dictation::DictationRequest::new(crate::dictation::StopMode::OnSilence),
            None,
        );
        assert!(!settings.history_audio_retention);

        for (revision, enabled) in [(0, true), (1, false)] {
            store
                .update(
                    revision,
                    PreferencePatch {
                        history_audio_retention: Some(enabled),
                        ..Default::default()
                    },
                )
                .unwrap();
            let preferences = PreferenceStore::open(&path)
                .unwrap()
                .snapshot()
                .unwrap()
                .preferences;
            assert_eq!(preferences.history_audio_retention, enabled);
            assert_eq!(
                preferences
                    .resolve_dictation(
                        crate::dictation::DictationRequest::new(
                            crate::dictation::StopMode::OnSilence
                        ),
                        None
                    )
                    .history_audio_retention,
                enabled
            );
            assert!(!settings.history_audio_retention);
        }
    }

    #[test]
    fn mute_output_persists_and_changes_only_future_recordings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences.json");
        let store = PreferenceStore::open(&path).unwrap();
        assert!(!store.snapshot().unwrap().preferences.mute_output);
        store
            .update(
                0,
                PreferencePatch {
                    mute_output: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let store = PreferenceStore::open(&path).unwrap();
        let snapshot = store.snapshot().unwrap();
        assert!(snapshot.preferences.mute_output);
        let recording = snapshot.preferences.resolve_dictation(
            crate::dictation::DictationRequest::new(crate::dictation::StopMode::OnSilence),
            None,
        );

        store.update(1, PreferencePatch::default()).unwrap();
        assert!(store.snapshot().unwrap().preferences.mute_output);
        store
            .update(
                1,
                PreferencePatch {
                    mute_output: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        let preferences = PreferenceStore::open(&path)
            .unwrap()
            .snapshot()
            .unwrap()
            .preferences;
        assert!(recording.mute_output);
        assert!(
            !preferences
                .resolve_dictation(
                    crate::dictation::DictationRequest::new(crate::dictation::StopMode::OnSilence),
                    None
                )
                .mute_output
        );
    }

    #[test]
    fn disabling_history_survives_restart_and_enters_dictation_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences.json");
        let store = PreferenceStore::open(&path).unwrap();
        store
            .update(
                0,
                PreferencePatch {
                    dictation_history: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        let preferences = PreferenceStore::open(&path)
            .unwrap()
            .snapshot()
            .unwrap()
            .preferences;
        assert!(
            !preferences
                .resolve_dictation(
                    crate::dictation::DictationRequest::new(crate::dictation::StopMode::OnSilence),
                    None
                )
                .dictation_history
        );
    }

    #[test]
    fn vocabulary_selection_persists_and_respects_request_overrides() {
        use crate::dictation::{DictationRequest, StopMode};
        use crate::vocabulary::{Context, Directory};

        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("preferences.json");
        let store = PreferenceStore::open(&path).expect("preferences");
        let selected = Directory::new("/projects/selected".into()).expect("absolute path");
        let explicit = Directory::new("/projects/explicit".into()).expect("absolute path");
        store
            .update(
                0,
                PreferencePatch {
                    project_vocabulary: Some(true),
                    vocabulary_directory: Some(Some(selected.clone())),
                    ..PreferencePatch::default()
                },
            )
            .expect("selection persists");
        let preferences = PreferenceStore::open(&path)
            .expect("reopen")
            .snapshot()
            .expect("snapshot")
            .preferences;
        for (context, expected) in [
            (Context::Default, Some(selected.clone())),
            (Context::Directory(explicit.clone()), Some(explicit.clone())),
            (Context::Disabled, None),
        ] {
            let settings = preferences.clone().resolve_dictation(
                DictationRequest {
                    project: context,
                    ..DictationRequest::new(StopMode::OnSilence)
                },
                None,
            );
            assert_eq!(settings.project, expected);
        }
        let disabled = PreferencePatch {
            project_vocabulary: Some(false),
            ..Default::default()
        }
        .apply_to(&preferences);
        assert_eq!(disabled.vocabulary_directory, Some(selected));
        assert_eq!(
            disabled
                .resolve_dictation(
                    DictationRequest {
                        project: Context::Directory(explicit),
                        ..DictationRequest::new(StopMode::OnSilence)
                    },
                    None
                )
                .project,
            None
        );
        let cleared = PreferencePatch {
            vocabulary_directory: Some(None),
            ..Default::default()
        }
        .apply_to(&preferences);
        assert!(cleared.vocabulary_directory.is_none());
    }

    #[test]
    fn an_app_value_matching_the_shared_one_is_dropped() {
        use super::super::{ApplicationId, ApplicationOverride, DictationOverrides};

        let slack = ApplicationId::new("com.tinyspeck.slackmacgap").expect("valid");
        let apps = ApplicationOverrides::new([(
            slack,
            ApplicationOverride {
                excluded: false,
                overrides: DictationOverrides {
                    output_mode: Some(OutputMode::Copy),
                    delivery_mode: Some(DeliveryMode::Complete),
                },
            },
        )])
        .expect("valid");
        let current = UserPreferences {
            output_mode: OutputMode::Insert,
            delivery_mode: DeliveryMode::Streaming,
            application_overrides: apps,
            ..UserPreferences::default()
        };

        let updated = PreferencePatch {
            output_mode: Some(OutputMode::Copy),
            ..Default::default()
        }
        .apply_to(&current);

        assert_eq!(
            updated
                .application_overrides
                .get("com.tinyspeck.slackmacgap"),
            Some(ApplicationOverride {
                excluded: false,
                overrides: DictationOverrides {
                    output_mode: None,
                    delivery_mode: Some(DeliveryMode::Complete),
                },
            })
        );
    }

    #[test]
    fn a_chord_the_ask_already_holds_is_refused_for_the_prompt_list() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = PreferenceStore::open(dir.path().join("preferences.json")).unwrap();

        let result = store.update(
            0,
            PreferencePatch {
                prompt_list_hotkey: Some(Some(Chord::ASK)),
                ..Default::default()
            },
        );

        assert!(matches!(result, Err(PreferenceStoreError::ChordInUse)));
    }

    #[test]
    fn a_cleared_shortcut_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences.json");
        let store = PreferenceStore::open(&path).unwrap();
        store
            .update(
                0,
                PreferencePatch {
                    ask_hotkey: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        let preferences = PreferenceStore::open(&path)
            .unwrap()
            .snapshot()
            .unwrap()
            .preferences;
        assert_eq!(preferences.ask_hotkey, None);
    }

    #[test]
    fn updating_the_dictation_hotkey_notifies_watchers() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = PreferenceStore::open(dir.path().join("preferences.json")).expect("opens");
        let mut watcher = store.dictation_hotkey();

        let unrelated = PreferencePatch {
            own_voice_only: Some(true),
            ..Default::default()
        };
        store.update(0, unrelated).expect("updates");
        assert!(!watcher.has_changed().expect("sender alive"));

        let key = PreferencePatch {
            dictation_hotkey: Some(HotkeyKey::Fn),
            ..Default::default()
        };
        store.update(1, key).expect("updates");
        assert!(watcher.has_changed().expect("sender alive"));
        assert_eq!(*watcher.borrow_and_update(), HotkeyKey::Fn);
    }
}
