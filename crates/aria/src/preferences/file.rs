use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    AskHotkey, AskHotkeyError, Modifiers, PreferenceSnapshot, PreferenceStoreError, UserPreferences,
};
use crate::audio::SilenceDuration;
use crate::delivery::OutputMode;
use crate::dictation::{DeliveryMode, StopMode};
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
        let recognition_language =
            self.preferences
                .recognition_language
                .parse()
                .map_err(|source| PreferenceStoreError::InvalidLanguage {
                    path: path.to_path_buf(),
                    source,
                })?;
        Ok(PreferenceSnapshot {
            revision: self.revision,
            preferences: UserPreferences {
                dictation_history: self.preferences.dictation_history,
                history_audio_retention: self.preferences.history_audio_retention,
                mute_output: self.preferences.mute_output,
                recognition_language,
                output_mode: self.preferences.output_mode.into(),
                stop_mode: self.preferences.stop_mode.into(),
                delivery_mode: self.preferences.delivery_mode.into(),
                silence_duration: SilenceDuration::new(Duration::from_millis(
                    self.preferences.silence_duration_ms,
                ))
                .map_err(|source| {
                    PreferenceStoreError::InvalidSilenceDuration {
                        path: path.to_path_buf(),
                        source,
                    }
                })?,
                own_voice_only: self.preferences.own_voice_only,
                project_vocabulary: self.preferences.project_vocabulary,
                vocabulary_directory: self
                    .preferences
                    .vocabulary_directory
                    .map(|path| crate::vocabulary::Directory::new(path.into()))
                    .transpose()?,
                dictation_hotkey: self.preferences.dictation_hotkey.into(),
                ask_hotkey: AskHotkey::try_from(self.preferences.ask_hotkey).map_err(|source| {
                    PreferenceStoreError::InvalidAskHotkey {
                        path: path.to_path_buf(),
                        source,
                    }
                })?,
            },
        })
    }
}

impl From<&PreferenceSnapshot> for StoredDocument {
    fn from(snapshot: &PreferenceSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            preferences: StoredPreferences {
                dictation_history: snapshot.preferences.dictation_history,
                history_audio_retention: snapshot.preferences.history_audio_retention,
                mute_output: snapshot.preferences.mute_output,
                recognition_language: snapshot
                    .preferences
                    .recognition_language
                    .as_str()
                    .to_owned(),
                output_mode: snapshot.preferences.output_mode.into(),
                stop_mode: snapshot.preferences.stop_mode.into(),
                delivery_mode: snapshot.preferences.delivery_mode.into(),
                silence_duration_ms: snapshot.preferences.silence_duration.get().as_millis() as u64,
                own_voice_only: snapshot.preferences.own_voice_only,
                project_vocabulary: snapshot.preferences.project_vocabulary,
                vocabulary_directory: snapshot
                    .preferences
                    .vocabulary_directory
                    .as_ref()
                    .map(|path| path.as_str().to_owned()),
                dictation_hotkey: snapshot.preferences.dictation_hotkey.into(),
                ask_hotkey: snapshot.preferences.ask_hotkey.into(),
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPreferences {
    #[serde(default = "history_enabled")]
    dictation_history: bool,
    #[serde(default)]
    history_audio_retention: bool,
    #[serde(default)]
    mute_output: bool,
    recognition_language: String,
    output_mode: StoredOutputMode,
    stop_mode: StoredStopMode,
    #[serde(default = "default_stored_delivery_mode")]
    delivery_mode: StoredDeliveryMode,
    silence_duration_ms: u64,
    // An older file has no own_voice_only; off matches its behavior.
    #[serde(default)]
    own_voice_only: bool,
    #[serde(default)]
    dictation_hotkey: StoredHotkeyKey,
    #[serde(default)]
    ask_hotkey: StoredAskHotkey,
    #[serde(default)]
    project_vocabulary: bool,
    #[serde(default)]
    vocabulary_directory: Option<String>,
}

fn history_enabled() -> bool {
    true
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
enum StoredStopMode {
    OnSilence,
    Manual,
}

impl From<StopMode> for StoredStopMode {
    fn from(value: StopMode) -> Self {
        match value {
            StopMode::OnSilence => Self::OnSilence,
            StopMode::Manual => Self::Manual,
        }
    }
}

impl From<StoredStopMode> for StopMode {
    fn from(value: StoredStopMode) -> Self {
        match value {
            StoredStopMode::OnSilence => Self::OnSilence,
            StoredStopMode::Manual => Self::Manual,
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

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAskHotkey {
    enabled: bool,
    key_code: u16,
    control: bool,
    option: bool,
    shift: bool,
    command: bool,
}

// An older file has no ask_hotkey; the default chord matches a fresh one.
impl Default for StoredAskHotkey {
    fn default() -> Self {
        AskHotkey::default().into()
    }
}

impl TryFrom<StoredAskHotkey> for AskHotkey {
    type Error = AskHotkeyError;

    fn try_from(stored: StoredAskHotkey) -> Result<Self, Self::Error> {
        Self::new(
            stored.key_code,
            Modifiers {
                control: stored.control,
                option: stored.option,
                shift: stored.shift,
                command: stored.command,
            },
            stored.enabled,
        )
    }
}

impl From<AskHotkey> for StoredAskHotkey {
    fn from(hotkey: AskHotkey) -> Self {
        let modifiers = hotkey.modifiers();
        Self {
            enabled: hotkey.enabled(),
            key_code: hotkey.key_code(),
            control: modifiers.control,
            option: modifiers.option,
            shift: modifiers.shift,
            command: modifiers.command,
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
    fn an_older_file_loads_with_defaults() {
        let document: StoredDocument = serde_json::from_str(
            r#"{"revision":2,"preferences":{"recognition_language":"en","output_mode":"insert",
                "stop_mode":"on_silence","delivery_mode":"streaming","silence_duration_ms":1500}}"#,
        )
        .expect("an older file still parses");

        assert!(document.preferences.dictation_history);
        assert!(!document.preferences.history_audio_retention);
        assert!(!document.preferences.mute_output);
        assert!(!document.preferences.own_voice_only);
        assert!(!document.preferences.project_vocabulary);
        assert!(document.preferences.vocabulary_directory.is_none());
        assert!(matches!(
            document.preferences.dictation_hotkey,
            StoredHotkeyKey::RightOption
        ));
        assert_eq!(
            AskHotkey::try_from(document.preferences.ask_hotkey).expect("a valid default"),
            AskHotkey::default()
        );
    }

    #[test]
    fn a_stored_hotkey_round_trips() {
        let hotkey = AskHotkey::new(
            49,
            Modifiers {
                control: false,
                option: true,
                shift: false,
                command: true,
            },
            false,
        )
        .expect("a disabled chord is valid");

        let decoded =
            AskHotkey::try_from(StoredAskHotkey::from(hotkey)).expect("the hotkey should decode");

        assert_eq!(decoded, hotkey);
    }
}
