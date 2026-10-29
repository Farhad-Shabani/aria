use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    ApplicationId, ApplicationOverride, ApplicationOverrides, Chord, ChordError,
    DictationOverrides, Modifiers, OverrideError, PreferenceSnapshot, PreferenceStoreError, Prompt,
    PromptError, PromptId, Prompts, UserPreferences,
};
use crate::audio::{AutoStop, SilenceDuration};
use crate::chat::PromptKind;
use crate::config::AssistantId;
use crate::delivery::OutputMode;
use crate::dictation::DeliveryMode;
use crate::hotkey::HotkeyKey;
use crate::primitives::private_document::{self, DocumentError, PreparedWrite, PrivateDocument};

const MAX_FILE_BYTES: u64 = 64 * 1024;

pub(super) fn load(path: &Path) -> Result<PreferenceSnapshot, PreferenceStoreError> {
    let document: Option<StoredDocument> = document(path).read().map_err(wrap(path))?;
    match document {
        Some(document) => document.into_snapshot(path),
        None => Ok(PreferenceSnapshot::default()),
    }
}

pub(super) fn prepare(
    path: &Path,
    snapshot: &PreferenceSnapshot,
) -> Result<PreparedWrite, PreferenceStoreError> {
    document(path)
        .prepare(&StoredDocument::from(snapshot))
        .map_err(wrap(path))
}

pub(super) struct CommittedWrite {
    path: PathBuf,
    committed: private_document::CommittedWrite,
}

impl CommittedWrite {
    /// A rename that landed but may not survive a power cut keeps its own
    /// category: the update is live, and the client is told to re-read rather
    /// than to retry.
    pub(super) fn confirm_durability(self) -> Result<(), PreferenceStoreError> {
        self.committed.confirm_durability().map_err(|source| {
            PreferenceStoreError::DurabilityUnconfirmed {
                path: self.path,
                source,
            }
        })
    }
}

pub(super) fn commit(
    prepared: PreparedWrite,
    path: &Path,
) -> Result<CommittedWrite, PreferenceStoreError> {
    Ok(CommittedWrite {
        path: path.to_path_buf(),
        committed: prepared.commit().map_err(wrap(path))?,
    })
}

fn document(path: &Path) -> PrivateDocument {
    PrivateDocument::new(path, MAX_FILE_BYTES)
}

/// The primitive names the fault; this file knows which document it was.
fn wrap(path: &Path) -> impl Fn(DocumentError) -> PreferenceStoreError + use<'_> {
    move |source| PreferenceStoreError::Document {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDocument {
    revision: u64,
    preferences: StoredPreferences,
}

impl StoredDocument {
    fn into_snapshot(self, path: &Path) -> Result<PreferenceSnapshot, PreferenceStoreError> {
        let invalid_chord = |source| PreferenceStoreError::InvalidChord {
            path: path.to_path_buf(),
            source,
        };
        let invalid_prompt = |source| PreferenceStoreError::InvalidPrompt {
            path: path.to_path_buf(),
            source,
        };
        let prompts = self
            .preferences
            .prompts
            .into_iter()
            .map(|stored| {
                let chord = stored
                    .chord
                    .map(Chord::try_from)
                    .transpose()
                    .map_err(invalid_chord)?;
                stored.into_prompt(chord).map_err(invalid_prompt)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let prompts = Prompts::new(prompts).map_err(invalid_prompt)?;
        let recognition_language =
            self.preferences
                .recognition_language
                .parse()
                .map_err(|source| PreferenceStoreError::InvalidLanguage {
                    path: path.to_path_buf(),
                    source,
                })?;
        let preferences = UserPreferences {
            assistant_id: self.preferences.assistant_id.map(Into::into),
            assistant_models: self
                .preferences
                .assistant_models
                .into_iter()
                .map(|(assistant_id, model_id)| (assistant_id.into(), model_id))
                .collect(),
            dictation_history: self.preferences.dictation_history,
            history_audio_retention: self.preferences.history_audio_retention,
            mute_output: self.preferences.mute_output,
            recognition_language,
            output_mode: self.preferences.output_mode.into(),
            delivery_mode: self.preferences.delivery_mode.into(),
            silence_duration: SilenceDuration::new(Duration::from_millis(
                self.preferences.silence_duration_ms,
            ))
            .map_err(|source| PreferenceStoreError::InvalidSilenceDuration {
                path: path.to_path_buf(),
                source,
            })?,
            auto_stop: self
                .preferences
                .auto_stop_ms
                .map(|ms| AutoStop::new(Duration::from_millis(ms)))
                .transpose()
                .map_err(|source| PreferenceStoreError::InvalidAutoStop {
                    path: path.to_path_buf(),
                    source,
                })?,
            own_voice_only: self.preferences.own_voice_only,
            project_vocabulary: self.preferences.project_vocabulary,
            vocabulary_directory: self
                .preferences
                .vocabulary_directory
                .map(|path| crate::vocabulary::Directory::new(path.into()))
                .transpose()?,
            dictation_hotkey: self.preferences.dictation_hotkey.into(),
            ask_hotkey: self.preferences.ask_hotkey.chord().map_err(invalid_chord)?,
            prompt_list_hotkey: self
                .preferences
                .prompt_list_hotkey
                .chord()
                .map_err(invalid_chord)?,
            application_overrides: ApplicationOverrides::try_from(
                self.preferences.application_overrides,
            )
            .map_err(|source| PreferenceStoreError::InvalidApplicationOverrides {
                path: path.to_path_buf(),
                source,
            })?,
            prompts,
        };
        preferences.validate_assistant_models()?;
        if preferences.shares_a_chord() {
            return Err(PreferenceStoreError::SharedChord {
                path: path.to_path_buf(),
            });
        }
        Ok(PreferenceSnapshot {
            revision: self.revision,
            preferences,
        })
    }
}

impl From<&PreferenceSnapshot> for StoredDocument {
    fn from(snapshot: &PreferenceSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            preferences: StoredPreferences {
                assistant_id: snapshot.preferences.assistant_id.map(Into::into),
                assistant_models: snapshot
                    .preferences
                    .assistant_models
                    .iter()
                    .map(|(assistant_id, model_id)| ((*assistant_id).into(), model_id.clone()))
                    .collect(),
                dictation_history: snapshot.preferences.dictation_history,
                history_audio_retention: snapshot.preferences.history_audio_retention,
                mute_output: snapshot.preferences.mute_output,
                recognition_language: snapshot
                    .preferences
                    .recognition_language
                    .as_str()
                    .to_owned(),
                output_mode: snapshot.preferences.output_mode.into(),
                delivery_mode: snapshot.preferences.delivery_mode.into(),
                silence_duration_ms: snapshot.preferences.silence_duration.get().as_millis() as u64,
                auto_stop_ms: snapshot
                    .preferences
                    .auto_stop
                    .map(|auto_stop| auto_stop.get().as_millis() as u64),
                own_voice_only: snapshot.preferences.own_voice_only,
                project_vocabulary: snapshot.preferences.project_vocabulary,
                vocabulary_directory: snapshot
                    .preferences
                    .vocabulary_directory
                    .as_ref()
                    .map(|path| path.as_str().to_owned()),
                dictation_hotkey: snapshot.preferences.dictation_hotkey.into(),
                ask_hotkey: snapshot.preferences.ask_hotkey.into(),
                prompt_list_hotkey: snapshot.preferences.prompt_list_hotkey.into(),
                application_overrides: snapshot
                    .preferences
                    .application_overrides
                    .iter()
                    .map(StoredApplicationOverride::from)
                    .collect(),
                prompts: snapshot
                    .preferences
                    .prompts
                    .entries()
                    .iter()
                    .map(StoredPrompt::from)
                    .collect(),
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assistant_id: Option<StoredAssistantId>,
    #[serde(default)]
    assistant_models: BTreeMap<StoredAssistantId, String>,
    #[serde(default = "history_enabled")]
    dictation_history: bool,
    #[serde(default)]
    history_audio_retention: bool,
    #[serde(default)]
    mute_output: bool,
    recognition_language: String,
    output_mode: StoredOutputMode,
    #[serde(default = "default_stored_delivery_mode")]
    delivery_mode: StoredDeliveryMode,
    silence_duration_ms: u64,
    // Missing in an older file, which gets the default; `null` is off.
    #[serde(default = "auto_stop_on")]
    auto_stop_ms: Option<u64>,
    // An older file has no own_voice_only; off matches its behavior.
    #[serde(default)]
    own_voice_only: bool,
    #[serde(default)]
    dictation_hotkey: StoredHotkeyKey,
    #[serde(default)]
    ask_hotkey: StoredHotkey,
    #[serde(default)]
    prompt_list_hotkey: StoredHotkey,
    #[serde(default)]
    project_vocabulary: bool,
    #[serde(default)]
    vocabulary_directory: Option<String>,
    #[serde(default)]
    application_overrides: Vec<StoredApplicationOverride>,
    #[serde(default)]
    prompts: Vec<StoredPrompt>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StoredAssistantId {
    ClaudeCode,
    Codex,
}

impl From<AssistantId> for StoredAssistantId {
    fn from(assistant_id: AssistantId) -> Self {
        match assistant_id {
            AssistantId::ClaudeCode => Self::ClaudeCode,
            AssistantId::Codex => Self::Codex,
        }
    }
}

impl From<StoredAssistantId> for AssistantId {
    fn from(assistant_id: StoredAssistantId) -> Self {
        match assistant_id {
            StoredAssistantId::ClaudeCode => Self::ClaudeCode,
            StoredAssistantId::Codex => Self::Codex,
        }
    }
}

fn history_enabled() -> bool {
    true
}

fn auto_stop_on() -> Option<u64> {
    Some(AutoStop::default().get().as_millis() as u64)
}

fn default_stored_delivery_mode() -> StoredDeliveryMode {
    StoredDeliveryMode::Streaming
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredOutputMode {
    Insert,
    InsertAndSubmit,
    InsertOrCopy,
    Copy,
}

impl From<OutputMode> for StoredOutputMode {
    fn from(value: OutputMode) -> Self {
        match value {
            OutputMode::Insert => Self::Insert,
            OutputMode::InsertAndSubmit => Self::InsertAndSubmit,
            OutputMode::InsertOrCopy => Self::InsertOrCopy,
            OutputMode::Copy => Self::Copy,
        }
    }
}

impl From<StoredOutputMode> for OutputMode {
    fn from(value: StoredOutputMode) -> Self {
        match value {
            StoredOutputMode::Insert => Self::Insert,
            StoredOutputMode::InsertAndSubmit => Self::InsertAndSubmit,
            StoredOutputMode::InsertOrCopy => Self::InsertOrCopy,
            StoredOutputMode::Copy => Self::Copy,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredDeliveryMode {
    Complete,
    Streaming,
}

impl From<DeliveryMode> for StoredDeliveryMode {
    fn from(value: DeliveryMode) -> Self {
        match value {
            DeliveryMode::Complete => Self::Complete,
            DeliveryMode::Streaming => Self::Streaming,
        }
    }
}

impl From<StoredDeliveryMode> for DeliveryMode {
    fn from(value: StoredDeliveryMode) -> Self {
        match value {
            StoredDeliveryMode::Complete => Self::Complete,
            StoredDeliveryMode::Streaming => Self::Streaming,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredApplicationOverride {
    application_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_mode: Option<StoredOutputMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivery_mode: Option<StoredDeliveryMode>,
    #[serde(default, skip_serializing_if = "is_false")]
    excluded: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl From<(&ApplicationId, &ApplicationOverride)> for StoredApplicationOverride {
    fn from((id, entry): (&ApplicationId, &ApplicationOverride)) -> Self {
        let ApplicationOverride {
            excluded,
            overrides:
                DictationOverrides {
                    output_mode,
                    delivery_mode,
                },
        } = *entry;
        Self {
            application_id: id.as_str().to_owned(),
            output_mode: output_mode.map(Into::into),
            delivery_mode: delivery_mode.map(Into::into),
            excluded,
        }
    }
}

impl TryFrom<Vec<StoredApplicationOverride>> for ApplicationOverrides {
    type Error = OverrideError;

    fn try_from(stored: Vec<StoredApplicationOverride>) -> Result<Self, Self::Error> {
        let entries = stored
            .into_iter()
            .map(|entry| {
                let overrides = ApplicationOverride {
                    excluded: entry.excluded,
                    overrides: DictationOverrides {
                        output_mode: entry.output_mode.map(Into::into),
                        delivery_mode: entry.delivery_mode.map(Into::into),
                    },
                };
                Ok((ApplicationId::new(entry.application_id)?, overrides))
            })
            .collect::<Result<Vec<_>, OverrideError>>()?;
        Self::new(entries)
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredChord {
    key_code: u16,
    control: bool,
    option: bool,
    shift: bool,
    command: bool,
}

impl TryFrom<StoredChord> for Chord {
    type Error = ChordError;

    fn try_from(stored: StoredChord) -> Result<Self, Self::Error> {
        Self::new(
            stored.key_code,
            Modifiers {
                control: stored.control,
                option: stored.option,
                shift: stored.shift,
                command: stored.command,
            },
        )
    }
}

impl From<Chord> for StoredChord {
    fn from(chord: Chord) -> Self {
        let modifiers = chord.modifiers();
        Self {
            key_code: chord.key_code(),
            control: modifiers.control,
            option: modifiers.option,
            shift: modifiers.shift,
            command: modifiers.command,
        }
    }
}

/// Flat, with a switch beside the chord. A chord that is off, or a switch
/// with no chord, reads as none.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredHotkey {
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_code: Option<u16>,
    control: bool,
    option: bool,
    shift: bool,
    command: bool,
}

impl Default for StoredHotkey {
    fn default() -> Self {
        Self::from(None)
    }
}

impl StoredHotkey {
    fn chord(self) -> Result<Option<Chord>, ChordError> {
        let (true, Some(key_code)) = (self.enabled, self.key_code) else {
            return Ok(None);
        };
        Chord::try_from(StoredChord {
            key_code,
            control: self.control,
            option: self.option,
            shift: self.shift,
            command: self.command,
        })
        .map(Some)
    }
}

impl From<Option<Chord>> for StoredHotkey {
    fn from(chord: Option<Chord>) -> Self {
        let chord = chord.map(StoredChord::from);
        Self {
            enabled: chord.is_some(),
            key_code: chord.map(|chord| chord.key_code),
            control: chord.is_some_and(|chord| chord.control),
            option: chord.is_some_and(|chord| chord.option),
            shift: chord.is_some_and(|chord| chord.shift),
            command: chord.is_some_and(|chord| chord.command),
        }
    }
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredHotkeyKey {
    #[default]
    RightOption,
    Fn,
}

impl From<HotkeyKey> for StoredHotkeyKey {
    fn from(value: HotkeyKey) -> Self {
        match value {
            HotkeyKey::RightOption => Self::RightOption,
            HotkeyKey::Fn => Self::Fn,
        }
    }
}

impl From<StoredHotkeyKey> for HotkeyKey {
    fn from(value: StoredHotkeyKey) -> Self {
        match value {
            StoredHotkeyKey::RightOption => Self::RightOption,
            StoredHotkeyKey::Fn => Self::Fn,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPrompt {
    id: String,
    name: String,
    kind: StoredPromptKind,
    file: PathBuf,
    enabled: bool,
    chord: Option<StoredChord>,
}

impl StoredPrompt {
    fn into_prompt(self, chord: Option<Chord>) -> Result<Prompt, PromptError> {
        Prompt::new(
            PromptId::new(self.id)?,
            self.name,
            self.kind.into(),
            self.file,
            self.enabled,
            chord,
        )
    }
}

impl From<&Prompt> for StoredPrompt {
    fn from(prompt: &Prompt) -> Self {
        Self {
            id: prompt.id().as_str().to_owned(),
            name: prompt.name().to_owned(),
            kind: prompt.kind().into(),
            file: prompt.file().to_path_buf(),
            enabled: prompt.enabled(),
            chord: prompt.chord().map(StoredChord::from),
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredPromptKind {
    Rewrite,
    Compose,
}

impl From<PromptKind> for StoredPromptKind {
    fn from(kind: PromptKind) -> Self {
        match kind {
            PromptKind::Rewrite => Self::Rewrite,
            PromptKind::Compose => Self::Compose,
        }
    }
}

impl From<StoredPromptKind> for PromptKind {
    fn from(kind: StoredPromptKind) -> Self {
        match kind {
            StoredPromptKind::Rewrite => Self::Rewrite,
            StoredPromptKind::Compose => Self::Compose,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_modes_round_trip_through_storage() {
        for (mode, name) in [
            (OutputMode::Insert, "insert"),
            (OutputMode::InsertAndSubmit, "insert_and_submit"),
            (OutputMode::InsertOrCopy, "insert_or_copy"),
            (OutputMode::Copy, "copy"),
        ] {
            let encoded = serde_json::to_string(&StoredOutputMode::from(mode)).unwrap();
            assert_eq!(encoded, format!("\"{name}\""));
            let decoded: StoredOutputMode = serde_json::from_str(&encoded).unwrap();
            assert_eq!(OutputMode::from(decoded), mode);
        }
    }

    #[test]
    fn a_file_with_a_stop_mode_is_refused() {
        let error = serde_json::from_str::<StoredDocument>(
            r#"{"revision":2,"preferences":{"recognition_language":"en","output_mode":"insert",
                "stop_mode":"on_silence","delivery_mode":"streaming","silence_duration_ms":1500}}"#,
        )
        .err()
        .expect("a stored stop mode is refused");

        assert!(error.to_string().contains("unknown field `stop_mode`"));
    }

    #[test]
    fn a_file_without_optional_fields_loads_with_defaults() {
        let document: StoredDocument = serde_json::from_str(
            r#"{"revision":2,"preferences":{"recognition_language":"en","output_mode":"insert",
                "delivery_mode":"streaming","silence_duration_ms":1500}}"#,
        )
        .expect("a file without optional fields still parses");

        assert!(document.preferences.dictation_history);
        assert!(!document.preferences.history_audio_retention);
        assert!(!document.preferences.mute_output);
        assert_eq!(document.preferences.auto_stop_ms, Some(10_000));
        assert!(!document.preferences.own_voice_only);
        assert!(!document.preferences.project_vocabulary);
        assert!(document.preferences.vocabulary_directory.is_none());
        assert!(document.preferences.application_overrides.is_empty());
        assert!(document.preferences.prompts.is_empty());
        assert!(matches!(
            document.preferences.dictation_hotkey,
            StoredHotkeyKey::RightOption
        ));
        assert_eq!(
            document
                .preferences
                .ask_hotkey
                .chord()
                .expect("a valid default"),
            None
        );
        assert_eq!(
            document
                .preferences
                .prompt_list_hotkey
                .chord()
                .expect("valid"),
            None
        );
    }

    #[test]
    fn a_stored_hotkey_round_trips_and_keeps_the_flat_layout() {
        let modifiers = Modifiers {
            option: true,
            command: true,
            ..Modifiers::default()
        };
        let chord = Chord::new(49, modifiers).expect("a chord");
        for hotkey in [Some(chord), None] {
            let decoded = StoredHotkey::from(hotkey).chord().expect("decodes");
            assert_eq!(decoded, hotkey);
        }

        let released: StoredHotkey = serde_json::from_str(
            r#"{"enabled":true,"key_code":0,"control":true,"option":true,"shift":false,"command":false}"#,
        )
        .expect("a released file's ask hotkey parses");
        let control_option_a = Chord::new(
            0,
            Modifiers {
                control: true,
                option: true,
                ..Modifiers::default()
            },
        )
        .expect("a chord");
        assert_eq!(released.chord().expect("valid"), Some(control_option_a));

        let switched_off: StoredHotkey = serde_json::from_str(
            r#"{"enabled":false,"key_code":0,"control":true,"option":true,"shift":false,"command":false}"#,
        )
        .expect("a released file's switched-off ask hotkey parses");
        assert_eq!(switched_off.chord().expect("valid"), None);

        let on_without_a_key: StoredHotkey = serde_json::from_str(
            r#"{"enabled":true,"control":false,"option":false,"shift":false,"command":false}"#,
        )
        .expect("a switch with no chord parses");
        assert_eq!(on_without_a_key.chord().expect("valid"), None);
    }

    #[test]
    fn a_file_that_gives_one_chord_two_jobs_is_refused() {
        let document: StoredDocument = serde_json::from_str(
            r#"{"revision":2,"preferences":{"recognition_language":"en","output_mode":"insert",
                "silence_duration_ms":1500,"ask_hotkey":{"enabled":true,"key_code":0,
                "control":true,"option":true,"shift":false,"command":false},"prompts":[{"id":"p","name":"P","kind":"rewrite",
                "file":"/p.md","enabled":true,"chord":{"key_code":0,"control":true,
                "option":true,"shift":false,"command":false}}]}}"#,
        )
        .expect("the document parses");

        assert!(matches!(
            document.into_snapshot(Path::new("/p.json")),
            Err(PreferenceStoreError::SharedChord { .. })
        ));
    }

    #[test]
    fn an_app_entry_without_an_exclusion_stays_on_and_writes_none() {
        let stored: StoredApplicationOverride =
            serde_json::from_str(r#"{"application_id":"com.app","output_mode":"copy"}"#)
                .expect("an older entry parses");
        assert!(!stored.excluded);

        let id = ApplicationId::new("com.app").expect("valid");
        let encoded = serde_json::to_string(&StoredApplicationOverride::from((
            &id,
            &ApplicationOverride::default(),
        )))
        .expect("encodes");
        assert!(!encoded.contains("excluded"));
    }

    #[test]
    fn application_overrides_round_trip_and_stay_validated() {
        let overrides = ApplicationOverrides::new([(
            ApplicationId::new("com.tinyspeck.slackmacgap").expect("valid"),
            ApplicationOverride {
                excluded: true,
                overrides: DictationOverrides {
                    output_mode: Some(OutputMode::Copy),
                    delivery_mode: None,
                },
            },
        )])
        .expect("valid");
        let snapshot = PreferenceSnapshot {
            revision: 3,
            preferences: UserPreferences {
                application_overrides: overrides.clone(),
                ..UserPreferences::default()
            },
        };
        let encoded = serde_json::to_string(&StoredDocument::from(&snapshot)).expect("encodes");
        assert!(encoded.contains(
            r#""application_overrides":[{"application_id":"com.tinyspeck.slackmacgap","output_mode":"copy","excluded":true}]"#
        ));

        let decoded: StoredDocument = serde_json::from_str(&encoded).expect("decodes");
        let path = Path::new("preferences.json");
        assert_eq!(
            decoded
                .into_snapshot(path)
                .expect("loads")
                .preferences
                .application_overrides,
            overrides
        );

        let invalid: StoredDocument =
            serde_json::from_str(&encoded.replace("slackmacgap", "slack mac")).expect("parses");
        assert!(matches!(
            invalid.into_snapshot(path),
            Err(PreferenceStoreError::InvalidApplicationOverrides { .. })
        ));
    }

    #[test]
    fn auto_stop_off_persists_as_null_and_stays_validated() {
        let snapshot = PreferenceSnapshot {
            revision: 3,
            preferences: UserPreferences {
                auto_stop: None,
                ..UserPreferences::default()
            },
        };
        let encoded = serde_json::to_string(&StoredDocument::from(&snapshot)).expect("encodes");
        assert!(encoded.contains(r#""auto_stop_ms":null"#));

        let decoded: StoredDocument = serde_json::from_str(&encoded).expect("decodes");
        let path = Path::new("preferences.json");
        assert_eq!(
            decoded
                .into_snapshot(path)
                .expect("loads")
                .preferences
                .auto_stop,
            None
        );

        let invalid: StoredDocument = serde_json::from_str(
            &encoded.replace(r#""auto_stop_ms":null"#, r#""auto_stop_ms":2999"#),
        )
        .expect("parses");
        assert!(matches!(
            invalid.into_snapshot(path),
            Err(PreferenceStoreError::InvalidAutoStop { .. })
        ));
    }

    #[test]
    fn stored_prompts_round_trip() {
        let prompt = Prompt::new(
            PromptId::new("7f3c2a90-1b2d").expect("id"),
            "Polish".to_owned(),
            PromptKind::Compose,
            PathBuf::from("/prompts/polish.md"),
            false,
            Some(
                Chord::new(
                    35,
                    Modifiers {
                        control: true,
                        ..Modifiers::default()
                    },
                )
                .expect("chord"),
            ),
        )
        .expect("prompt");
        let snapshot = PreferenceSnapshot {
            revision: 3,
            preferences: UserPreferences {
                prompts: Prompts::new(vec![prompt]).expect("prompts"),
                ..UserPreferences::default()
            },
        };

        let encoded = serde_json::to_string(&StoredDocument::from(&snapshot)).expect("encode");
        let decoded: StoredDocument = serde_json::from_str(&encoded).expect("decode");

        assert_eq!(
            decoded.into_snapshot(Path::new("/p.json")).expect("valid"),
            snapshot
        );
    }

    #[test]
    fn a_stored_prompt_with_a_relative_file_is_refused() {
        let document: StoredDocument = serde_json::from_str(
            r#"{"revision":2,"preferences":{"recognition_language":"en","output_mode":"insert",
                "silence_duration_ms":1500,"prompts":[{"id":"p","name":"P","kind":"rewrite",
                "file":"p.md","enabled":true,"chord":null}]}}"#,
        )
        .expect("the document parses");

        assert!(matches!(
            document.into_snapshot(Path::new("/p.json")),
            Err(PreferenceStoreError::InvalidPrompt { .. })
        ));
    }
}
