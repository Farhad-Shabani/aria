use std::collections::BTreeMap;
use std::sync::Arc;

use aria_kit::synthesis::SpeakingRate;
use aria_kit::transcription::Language;
use tokio::sync::watch;

use super::DeliveryMode;
use super::{ApplicationOverrides, Prompts, Settings, Shortcut, StoreError, view};
use super::{Loaded, Patch, Refusal, Revision, SettingsCache, SettingsFile, Written};
use crate::audio::{AutoStop, SilenceDuration};
use crate::config::AssistantId;
use crate::delivery::OutputMode;
use crate::hotkey::AriaKey;

/// `revision` is the hash of the file's values, so a comment edit keeps it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsSnapshot {
    pub revision: u64,
    pub settings: Settings,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsPatch {
    pub output_mode: Option<OutputMode>,
    pub delivery_mode: Option<DeliveryMode>,
    pub silence_duration: Option<SilenceDuration>,
    pub auto_stop: Option<Option<AutoStop>>,
    pub mute_sound: Option<bool>,
    pub cleanup: Option<bool>,
    pub backtrack: Option<bool>,
    pub own_voice_only: Option<bool>,
    pub vocabulary_enabled: Option<bool>,
    pub vocabulary_dir: Option<Option<super::Directory>>,
    pub history_enabled: Option<bool>,
    pub history_keep_audio: Option<bool>,
    pub assistant_id: Option<AssistantId>,
    pub assistant_models: BTreeMap<AssistantId, String>,
    pub memory_enabled: Option<bool>,
    pub prompts: Option<Prompts>,
    /// Replaces every entry.
    pub application_overrides: Option<ApplicationOverrides>,
    pub aria_key: Option<Option<AriaKey>>,
    pub ask_shortcut: Option<Option<Shortcut>>,
    pub prompt_list_shortcut: Option<Option<Shortcut>>,
    pub stt_language: Option<Language>,
    pub tts_rate: Option<SpeakingRate>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateResult {
    Updated {
        snapshot: SettingsSnapshot,
        /// Dotted keys that apply after a restart, the written ones or not.
        restart_required: Vec<String>,
    },
    Conflict(SettingsSnapshot),
}

/// The shared settings as a view over the settings file. Every write goes
/// through the settings write path; the watches belong to the settings cache.
pub(crate) struct SettingsStore {
    cache: Arc<SettingsCache>,
}

impl SettingsStore {
    pub(crate) fn new(cache: Arc<SettingsCache>) -> Self {
        Self { cache }
    }

    /// A store over its own settings file at `path`, for tests.
    #[cfg(test)]
    pub(crate) fn open(path: impl Into<std::path::PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        let home = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        Ok(Self::new(Arc::new(SettingsCache::open(path, home)?)))
    }

    /// The Aria key, `None` while it is off.
    pub(crate) fn aria_key(&self) -> watch::Receiver<Option<AriaKey>> {
        self.cache.aria_key()
    }

    pub(crate) fn snapshot(&self) -> Result<SettingsSnapshot, StoreError> {
        let loaded = self.cache.snapshot();
        snapshot_of(&loaded)
    }

    /// A client's patch against the revision it read.
    pub(crate) fn update(
        &self,
        expected_revision: u64,
        patch: SettingsPatch,
    ) -> Result<UpdateResult, StoreError> {
        self.write(Some(Revision::from(expected_revision)), patch)
    }

    /// The daemon's own write, made against the file as it stands.
    pub(crate) fn modify(&self, patch: SettingsPatch) -> Result<SettingsSnapshot, StoreError> {
        match self.write(None, patch)? {
            UpdateResult::Updated { snapshot, .. } | UpdateResult::Conflict(snapshot) => {
                Ok(snapshot)
            }
        }
    }

    /// Tells whether an update would accept `patch`. Takes no lock and no
    /// revision, so the update can still conflict or find the file busy.
    pub(crate) fn check(&self, patch: SettingsPatch) -> Result<(), StoreError> {
        let current = self.cache.snapshot();
        let patch = Self::file_patch(patch, &current.settings)?;
        match self.cache.check_patch(&patch)? {
            // `check_patch` takes no revision, so it never answers a conflict.
            // The match still has to name that arm.
            Written::Applied(_)
            | Written::AppliedRestartKey(_)
            | Written::Refused(Refusal::Conflict { .. }) => Ok(()),
            Written::Refused(Refusal::FileInvalid(error)) => Err(StoreError::FileInvalid(error)),
            Written::Refused(Refusal::Invalid(error)) => Err(StoreError::Settings(error)),
            Written::Refused(Refusal::ChangedRecently) => Err(StoreError::ChangedRecently),
        }
    }

    fn write(
        &self,
        expected: Option<Revision>,
        patch: SettingsPatch,
    ) -> Result<UpdateResult, StoreError> {
        let written = self
            .cache
            .modify(expected, |settings| Self::file_patch(patch, settings))?;
        match written {
            Written::Applied(loaded) | Written::AppliedRestartKey(loaded) => {
                Ok(UpdateResult::Updated {
                    snapshot: snapshot_of(&loaded)?,
                    restart_required: self.cache.restart_required(),
                })
            }
            Written::Refused(Refusal::Conflict { current }) => {
                Ok(UpdateResult::Conflict(snapshot_of(&current)?))
            }
            Written::Refused(Refusal::FileInvalid(error)) => Err(StoreError::FileInvalid(error)),
            Written::Refused(Refusal::Invalid(error)) => Err(StoreError::Settings(error)),
            Written::Refused(Refusal::ChangedRecently) => Err(StoreError::ChangedRecently),
        }
    }
}

impl SettingsStore {
    /// The keys of the settings file that `patch` changes. Fails if the result
    /// reuses a shortcut or names an invalid model.
    fn file_patch(patch: SettingsPatch, settings: &SettingsFile) -> Result<Patch, StoreError> {
        let current = Settings::from_file(settings)?;
        let next = patch.apply_to(&current);
        next.validate_assistant_models()?;
        if next.shares_a_binding() {
            return Err(StoreError::BindingInUse);
        }
        Ok(view::patch(&current, &next)?)
    }
}

fn snapshot_of(loaded: &Loaded) -> Result<SettingsSnapshot, StoreError> {
    Ok(SettingsSnapshot {
        revision: loaded.revision.get(),
        settings: Settings::from_file(&loaded.settings)?,
    })
}

impl SettingsPatch {
    fn apply_to(self, current: &Settings) -> Settings {
        let output_mode = self.output_mode.unwrap_or(current.output_mode);
        let delivery_mode = self.delivery_mode.unwrap_or(current.delivery_mode);
        let mut assistant_models = current.assistant_models.clone();
        assistant_models.extend(self.assistant_models);
        assistant_models.retain(|_, model_id| !model_id.is_empty());
        Settings {
            output_mode,
            delivery_mode,
            silence_duration: self.silence_duration.unwrap_or(current.silence_duration),
            auto_stop: self.auto_stop.unwrap_or(current.auto_stop),
            mute_sound: self.mute_sound.unwrap_or(current.mute_sound),
            cleanup: self.cleanup.unwrap_or(current.cleanup),
            backtrack: self.backtrack.unwrap_or(current.backtrack),
            own_voice_only: self.own_voice_only.unwrap_or(current.own_voice_only),
            vocabulary_enabled: self
                .vocabulary_enabled
                .unwrap_or(current.vocabulary_enabled),
            vocabulary_dir: self
                .vocabulary_dir
                .unwrap_or_else(|| current.vocabulary_dir.clone()),
            history_enabled: self.history_enabled.unwrap_or(current.history_enabled),
            history_keep_audio: self
                .history_keep_audio
                .unwrap_or(current.history_keep_audio),
            assistant_id: self.assistant_id.or(current.assistant_id),
            assistant_models,
            memory_enabled: self.memory_enabled.unwrap_or(current.memory_enabled),
            prompts: self.prompts.unwrap_or_else(|| current.prompts.clone()),
            application_overrides: match self.application_overrides {
                Some(overrides) => overrides.without_inert(output_mode, delivery_mode),
                None => current.application_overrides.clone(),
            },
            aria_key: self.aria_key.unwrap_or(current.aria_key),
            ask_shortcut: self.ask_shortcut.unwrap_or(current.ask_shortcut),
            prompt_list_shortcut: self
                .prompt_list_shortcut
                .unwrap_or(current.prompt_list_shortcut),
            stt_language: self
                .stt_language
                .unwrap_or_else(|| current.stt_language.clone()),
            tts_rate: self.tts_rate.unwrap_or(current.tts_rate),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Chord, Modifiers, Prompt, PromptId};
    use super::*;
    use crate::hotkey::Combo;

    fn revision(store: &SettingsStore) -> u64 {
        store.snapshot().expect("a snapshot").revision
    }

    fn new_store() -> (tempfile::TempDir, std::path::PathBuf, SettingsStore) {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("settings.toml");
        let store = SettingsStore::open(&path).unwrap();
        (home, path, store)
    }

    fn update(store: &SettingsStore, patch: SettingsPatch) {
        store.update(revision(store), patch).unwrap();
    }

    /// A toggle changes its own line and nothing else.
    #[test]
    fn an_unrelated_toggle_keeps_a_hand_written_override_line() {
        let written = "[dictation]\noutput_mode = \"insert\"\n\n\
            [application_overrides.\"com.app\"]\n\
            output_mode = \"insert\" # the same on purpose\n";
        let (_home, path, store) = store_with(written);

        update(
            &store,
            SettingsPatch {
                mute_sound: Some(true),
                ..Default::default()
            },
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("output_mode = \"insert\" # the same on purpose"),
            "{text}"
        );
    }

    #[test]
    fn assistant_models_persist_independently_and_empty_resets_to_default() {
        let (_home, path, store) = new_store();
        let original = store.snapshot().unwrap();
        assert_eq!(original.settings.model_id(AssistantId::ClaudeCode), None);
        assert_eq!(original.settings.model_id(AssistantId::Codex), None);
        update(
            &store,
            SettingsPatch {
                assistant_models: [
                    (AssistantId::ClaudeCode, "claude-example".to_owned()),
                    (AssistantId::Codex, "codex-example".to_owned()),
                ]
                .into(),
                ..Default::default()
            },
        );
        let reopened = SettingsStore::open(&path).unwrap();
        update(
            &reopened,
            SettingsPatch {
                assistant_id: Some(AssistantId::Codex),
                assistant_models: [(AssistantId::ClaudeCode, String::new())].into(),
                ..Default::default()
            },
        );
        let settings = SettingsStore::open(&path)
            .unwrap()
            .snapshot()
            .unwrap()
            .settings;
        assert_eq!(settings.model_id(AssistantId::ClaudeCode), None);
        assert_eq!(settings.model_id(AssistantId::Codex), Some("codex-example"));
        assert_eq!(settings.assistant_id, Some(AssistantId::Codex));
    }

    #[test]
    fn invalid_model_ids_leave_settings_unchanged() {
        let (_home, path, store) = new_store();
        update(
            &store,
            SettingsPatch {
                mute_sound: Some(true),
                ..Default::default()
            },
        );
        let before = revision(&store);
        let invalid_ids = [
            "two words".to_owned(),
            "line\nbreak".to_owned(),
            "--flag".to_owned(),
            "x".repeat(201),
        ];
        for invalid in &invalid_ids {
            assert!(matches!(
                store.update(
                    before,
                    SettingsPatch {
                        assistant_models: [(AssistantId::Codex, invalid.clone())].into(),
                        ..Default::default()
                    }
                ),
                Err(StoreError::InvalidModelId)
            ));
            assert_eq!(revision(&store), before);
        }
        for invalid in &invalid_ids {
            let text = format!("[assistant.models]\ncodex = {invalid:?}\n");
            std::fs::write(&path, text).unwrap();
            assert!(matches!(
                SettingsStore::open(&path),
                Err(StoreError::Settings(
                    crate::settings::SettingsError::Invalid {
                        key: crate::settings::Key::AssistantModels,
                        ..
                    }
                ))
            ));
        }
    }

    #[test]
    fn the_assistant_stays_unsaved_until_explicitly_selected() {
        let (_home, path, store) = new_store();
        assert_eq!(store.snapshot().unwrap().settings.assistant_id, None);
        update(
            &store,
            SettingsPatch {
                mute_sound: Some(true),
                ..Default::default()
            },
        );
        let reopened = SettingsStore::open(&path).unwrap();
        assert_eq!(reopened.snapshot().unwrap().settings.assistant_id, None);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[assistant]"), "{text}");

        update(
            &reopened,
            SettingsPatch {
                assistant_id: Some(AssistantId::Codex),
                ..Default::default()
            },
        );
        let reopened = SettingsStore::open(&path).unwrap();
        assert_eq!(
            reopened.snapshot().unwrap().settings.assistant_id,
            Some(AssistantId::Codex)
        );
        update(
            &reopened,
            SettingsPatch {
                mute_sound: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(
            reopened.snapshot().unwrap().settings.assistant_id,
            Some(AssistantId::Codex)
        );
    }

    #[test]
    fn history_keep_audio_is_opt_in_and_snapshotted() {
        let (_home, path, store) = new_store();
        let original = store.snapshot().unwrap().settings;
        assert!(!original.history_keep_audio);
        let settings = crate::dictation::DictationSettings::resolve(
            original,
            crate::dictation::DictationRequest::new(crate::dictation::StopMode::OnSilence),
            None,
        );
        assert!(!settings.history_keep_audio);

        for enabled in [true, false] {
            update(
                &store,
                SettingsPatch {
                    history_keep_audio: Some(enabled),
                    ..Default::default()
                },
            );
            let loaded = SettingsStore::open(&path)
                .unwrap()
                .snapshot()
                .unwrap()
                .settings;
            assert_eq!(loaded.history_keep_audio, enabled);
            assert_eq!(
                crate::dictation::DictationSettings::resolve(
                    loaded,
                    crate::dictation::DictationRequest::new(crate::dictation::StopMode::OnSilence),
                    None
                )
                .history_keep_audio,
                enabled
            );
            assert!(!settings.history_keep_audio);
        }
    }

    #[test]
    fn vocabulary_selection_persists_and_respects_request_overrides() {
        use crate::dictation::{DictationRequest, StopMode};
        use crate::settings::Directory;
        use crate::vocabulary::Context;

        let (_home, path, store) = new_store();
        let selected = Directory::new("/projects/selected".into()).expect("absolute path");
        let explicit = Directory::new("/projects/explicit".into()).expect("absolute path");
        store
            .update(
                revision(&store),
                SettingsPatch {
                    vocabulary_enabled: Some(true),
                    vocabulary_dir: Some(Some(selected.clone())),
                    ..SettingsPatch::default()
                },
            )
            .expect("selection persists");
        let settings = SettingsStore::open(&path)
            .expect("reopen")
            .snapshot()
            .expect("snapshot")
            .settings;
        for (context, expected) in [
            (Context::Default, Some(selected.clone())),
            (Context::Directory(explicit.clone()), Some(explicit.clone())),
            (Context::Disabled, None),
        ] {
            let settings = crate::dictation::DictationSettings::resolve(
                settings.clone(),
                DictationRequest {
                    project: context,
                    ..DictationRequest::new(StopMode::OnSilence)
                },
                None,
            );
            assert_eq!(settings.project, expected);
        }
        let disabled = SettingsPatch {
            vocabulary_enabled: Some(false),
            ..Default::default()
        }
        .apply_to(&settings);
        assert_eq!(disabled.vocabulary_dir, Some(selected));
        assert_eq!(
            crate::dictation::DictationSettings::resolve(
                disabled,
                DictationRequest {
                    project: Context::Directory(explicit),
                    ..DictationRequest::new(StopMode::OnSilence)
                },
                None
            )
            .project,
            None
        );
        let cleared = SettingsPatch {
            vocabulary_dir: Some(None),
            ..Default::default()
        }
        .apply_to(&settings);
        assert!(cleared.vocabulary_dir.is_none());
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
        let current = Settings {
            output_mode: OutputMode::Insert,
            delivery_mode: DeliveryMode::Streaming,
            application_overrides: apps.clone(),
            ..Settings::default()
        };

        let unnamed = SettingsPatch {
            output_mode: Some(OutputMode::Copy),
            ..Default::default()
        }
        .apply_to(&current);
        assert_eq!(unnamed.application_overrides, apps);

        let updated = SettingsPatch {
            output_mode: Some(OutputMode::Copy),
            application_overrides: Some(apps),
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
    fn one_binding_for_the_ask_and_the_prompt_list_is_refused() {
        let (_home, _path, store) = new_store();
        let control_option_a = Chord::new(
            0,
            Modifiers {
                control: true,
                option: true,
                ..Modifiers::default()
            },
        )
        .expect("a chord");

        for shared in [
            Shortcut::Chord(control_option_a),
            Shortcut::Combo(Combo::Shift),
        ] {
            let result = store.update(
                revision(&store),
                SettingsPatch {
                    ask_shortcut: Some(Some(shared)),
                    prompt_list_shortcut: Some(Some(shared)),
                    ..Default::default()
                },
            );
            assert!(
                matches!(result, Err(StoreError::BindingInUse)),
                "{shared:?}"
            );
        }
    }

    #[test]
    fn a_check_answers_like_an_update_and_leaves_the_file_as_it_was() {
        let (_home, path, store) = new_store();
        let control_option_a = Shortcut::Chord(
            Chord::new(
                0,
                Modifiers {
                    control: true,
                    option: true,
                    ..Modifiers::default()
                },
            )
            .expect("a chord"),
        );
        update(
            &store,
            SettingsPatch {
                ask_shortcut: Some(Some(control_option_a)),
                ..Default::default()
            },
        );
        let before = std::fs::read_to_string(&path).unwrap();

        let refused = store.check(SettingsPatch {
            prompt_list_shortcut: Some(Some(control_option_a)),
            ..Default::default()
        });
        let taken = store.check(SettingsPatch {
            mute_sound: Some(true),
            ..Default::default()
        });

        assert!(
            matches!(refused, Err(StoreError::BindingInUse)),
            "{refused:?}"
        );
        assert!(taken.is_ok(), "{taken:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// Operations read these through `loaded`, so a restart mark would report
    /// a restart while applying live.
    #[test]
    fn every_key_behind_these_settings_is_live() {
        use crate::settings::{Apply, Key};
        for key in [
            Key::DictationOutputMode,
            Key::DictationDeliveryMode,
            Key::DictationSilenceDurationMs,
            Key::DictationAutoStopMs,
            Key::DictationMuteSound,
            Key::DictationCleanup,
            Key::DictationBacktrack,
            Key::DictationOwnVoiceOnly,
            Key::DictationVocabularyEnabled,
            Key::DictationVocabularyDir,
            Key::HistoryEnabled,
            Key::HistoryKeepAudio,
            Key::AssistantId,
            Key::AssistantModels,
            Key::MemoryEnabled,
            Key::Prompts,
            Key::ApplicationOverrides,
            Key::ShortcutsAriaKey,
            Key::ShortcutsAsk,
            Key::ShortcutsPromptList,
            Key::SttLanguage,
            Key::TtsRate,
        ] {
            assert_eq!(key.apply(), Apply::Live, "{key}");
        }
    }

    #[test]
    fn updating_the_aria_key_notifies_watchers() {
        let (_home, _path, store) = new_store();
        let mut watcher = store.aria_key();

        let unrelated = SettingsPatch {
            own_voice_only: Some(true),
            ..Default::default()
        };
        store.update(revision(&store), unrelated).expect("updates");
        assert!(!watcher.has_changed().expect("sender alive"));

        let key = SettingsPatch {
            aria_key: Some(Some(AriaKey::Fn)),
            ..Default::default()
        };
        store.update(revision(&store), key).expect("updates");
        assert!(watcher.has_changed().expect("sender alive"));
        assert_eq!(*watcher.borrow_and_update(), Some(AriaKey::Fn));
    }

    const PROMPTS: &str = "[prompts.reply]\n# keep me\nname = \"Reply\"\nkind = \"compose\"\nfile = \"~/p/reply.md\" # mine\n\n# the second\n[prompts.polish]\nname = \"Polish\"\nkind = \"rewrite\"\nfile = \"/p/polish.md\"\n";

    fn store_with(text: &str) -> (tempfile::TempDir, std::path::PathBuf, SettingsStore) {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("settings.toml");
        std::fs::write(&path, text).unwrap();
        let store = SettingsStore::open(&path).unwrap();
        (home, path, store)
    }

    fn renamed(prompt: &Prompt, name: &str) -> Prompt {
        Prompt::new(
            PromptId::new(prompt.id().as_str()).unwrap(),
            name.to_owned(),
            prompt.kind(),
            prompt.file().to_path_buf(),
            prompt.enabled(),
            prompt.chord(),
        )
        .unwrap()
    }

    fn write_prompts(store: &SettingsStore, prompts: Vec<Prompt>) {
        store
            .modify(SettingsPatch {
                prompts: Some(Prompts::new(prompts).unwrap()),
                ..Default::default()
            })
            .unwrap();
    }

    #[test]
    fn a_prompt_edit_keeps_the_comments_inside_its_table() {
        let (_home, path, store) = store_with(PROMPTS);
        let current = store.snapshot().unwrap().settings.prompts;
        let [reply, polish] = current.entries() else {
            panic!("two prompts");
        };

        write_prompts(&store, vec![renamed(reply, "Answer"), polish.clone()]);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            PROMPTS.replace("name = \"Reply\"", "name = \"Answer\"")
        );
    }

    #[test]
    fn removing_a_prompt_removes_only_its_table() {
        let (_home, path, store) = store_with(PROMPTS);
        let current = store.snapshot().unwrap().settings.prompts;

        write_prompts(&store, vec![current.entries()[0].clone()]);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("[prompts.reply]\n# keep me\nname = \"Reply\""),
            "{text}"
        );
        assert!(
            !text.contains("polish") && !text.contains("the second"),
            "{text}"
        );
    }

    #[test]
    fn reordering_prompts_moves_each_table_with_its_comments() {
        let (_home, path, store) = store_with(PROMPTS);
        let current = store.snapshot().unwrap().settings.prompts;
        let [reply, polish] = current.entries() else {
            panic!("two prompts");
        };

        write_prompts(&store, vec![polish.clone(), reply.clone()]);

        let text = std::fs::read_to_string(&path).unwrap();
        let polish_at = text.find("[prompts.polish]").expect("polish stays");
        let reply_at = text.find("[prompts.reply]").expect("reply stays");
        assert!(polish_at < reply_at, "{text}");
        assert!(text.contains("# the second\n[prompts.polish]"), "{text}");
        assert!(text.contains("[prompts.reply]\n# keep me\n"), "{text}");
        let ids: Vec<String> = SettingsStore::open(&path)
            .unwrap()
            .snapshot()
            .unwrap()
            .settings
            .prompts
            .entries()
            .iter()
            .map(|prompt| prompt.id().as_str().to_owned())
            .collect();
        assert_eq!(ids, ["polish", "reply"]);
    }

    #[test]
    fn an_app_override_edit_keeps_its_other_lines() {
        let text = "[dictation]\noutput_mode = \"insert\"\n\n[application_overrides.\"com.tinyspeck.slackmacgap\"]\n# chats\noutput_mode = \"copy\"\nexcluded = false\n";
        let (_home, path, store) = store_with(text);
        let mut overrides: Vec<_> = store
            .snapshot()
            .unwrap()
            .settings
            .application_overrides
            .iter()
            .map(|(id, entry)| (id.clone(), *entry))
            .collect();
        overrides[0].1.excluded = true;

        store
            .modify(SettingsPatch {
                application_overrides: Some(ApplicationOverrides::new(overrides).unwrap()),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text.replace("excluded = false", "excluded = true")
        );
    }
}
