//! Shared settings as keys of the settings file: read from the parsed
//! values, and written back as a patch of only the keys that changed.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{ApplicationId, ApplicationOverride, ApplicationOverrides, Prompt, PromptId, Prompts};
use super::{DictationOverrides, Settings};
use aria_kit::synthesis::SpeakingRate;

use super::{
    AriaKeyName, DeliveryModeName, InvalidValue, Key, Keyed, OutputModeName, Patch, SettingsError,
    SettingsFile, ShortcutEntry, invalid,
};
use crate::audio::{AutoStop, SilenceDuration};
use crate::config::AssistantId;

impl Settings {
    pub(crate) fn from_file(settings: &SettingsFile) -> Result<Self, SettingsError> {
        let defaults = Self::default();
        let SettingsFile {
            dictation,
            history,
            stt,
            assistant,
            shortcuts,
            prompts,
            application_overrides,
            ..
        } = settings;
        Ok(Self {
            output_mode: dictation
                .output_mode
                .map_or(defaults.output_mode, Into::into),
            delivery_mode: dictation
                .delivery_mode
                .map_or(defaults.delivery_mode, Into::into),
            silence_duration: match dictation.silence_duration_ms {
                Some(ms) => SilenceDuration::new(Duration::from_millis(ms)).map_err(|source| {
                    invalid(
                        Key::DictationSilenceDurationMs,
                        InvalidValue::SilenceDuration(source),
                    )
                })?,
                None => defaults.silence_duration,
            },
            auto_stop: match dictation.auto_stop_ms {
                Some(AUTO_STOP_OFF) => None,
                Some(ms) => Some(AutoStop::new(Duration::from_millis(ms)).map_err(|source| {
                    invalid(Key::DictationAutoStopMs, InvalidValue::AutoStop(source))
                })?),
                None => defaults.auto_stop,
            },
            mute_sound: dictation.mute_sound.unwrap_or(defaults.mute_sound),
            cleanup: dictation.cleanup.unwrap_or(defaults.cleanup),
            backtrack: dictation.backtrack.unwrap_or(defaults.backtrack),
            own_voice_only: dictation.own_voice_only.unwrap_or(defaults.own_voice_only),
            vocabulary_enabled: dictation
                .vocabulary
                .enabled
                .unwrap_or(defaults.vocabulary_enabled),
            vocabulary_dir: dictation
                .vocabulary
                .dir
                .clone()
                .map(super::Directory::new)
                .transpose()
                .map_err(|source| {
                    invalid(Key::DictationVocabularyDir, InvalidValue::Directory(source))
                })?,
            history_enabled: history.enabled.unwrap_or(defaults.history_enabled),
            history_keep_audio: history.keep_audio.unwrap_or(defaults.history_keep_audio),
            assistant_id: assistant
                .id
                .as_deref()
                .map(|name| assistant_id(name, Key::AssistantId))
                .transpose()?,
            assistant_models: assistant
                .models
                .iter()
                .map(|(name, model)| Ok((assistant_id(name, Key::AssistantModels)?, model.clone())))
                .collect::<Result<_, SettingsError>>()?,
            memory_enabled: settings.memory.enabled.unwrap_or(defaults.memory_enabled),
            prompts: prompt_list(prompts).map_err(|source| invalid(Key::Prompts, source))?,
            application_overrides: overrides(application_overrides)
                .map_err(|source| invalid(Key::ApplicationOverrides, source))?,
            aria_key: settings.aria_key(),
            ask_shortcut: shortcut(shortcuts.ask, defaults.ask_shortcut, Key::ShortcutsAsk)?,
            prompt_list_shortcut: shortcut(
                shortcuts.prompt_list,
                defaults.prompt_list_shortcut,
                Key::ShortcutsPromptList,
            )?,
            stt_language: match &stt.language {
                Some(language) => language
                    .parse()
                    .map_err(|source| invalid(Key::SttLanguage, InvalidValue::Language(source)))?,
                None => defaults.stt_language,
            },
            tts_rate: match settings.tts.rate {
                Some(rate) => SpeakingRate::new(rate as f32)
                    .map_err(|source| invalid(Key::TtsRate, InvalidValue::SpeakingRate(source)))?,
                None => defaults.tts_rate,
            },
        })
    }
}

/// The keys whose values differ between `current` and `next`. Destructured
/// so a new setting fails to compile until it has a key.
pub(super) fn patch(current: &Settings, next: &Settings) -> Result<Patch, SettingsError> {
    let Settings {
        output_mode,
        delivery_mode,
        silence_duration,
        auto_stop,
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
    } = next;
    let mut patch = Patch::new();
    if *output_mode != current.output_mode {
        patch.set_encoded(
            Key::DictationOutputMode,
            &OutputModeName::from(*output_mode),
        )?;
    }
    if *delivery_mode != current.delivery_mode {
        patch.set_encoded(
            Key::DictationDeliveryMode,
            &DeliveryModeName::from(*delivery_mode),
        )?;
    }
    if *silence_duration != current.silence_duration {
        patch.set(
            Key::DictationSilenceDurationMs,
            milliseconds(silence_duration.get()),
        );
    }
    if *auto_stop != current.auto_stop {
        let ms = auto_stop.map_or(0, |auto_stop| milliseconds(auto_stop.get()));
        patch.set(Key::DictationAutoStopMs, ms);
    }
    if *mute_sound != current.mute_sound {
        patch.set(Key::DictationMuteSound, *mute_sound);
    }
    if *cleanup != current.cleanup {
        patch.set(Key::DictationCleanup, *cleanup);
    }
    if *backtrack != current.backtrack {
        patch.set(Key::DictationBacktrack, *backtrack);
    }
    if *own_voice_only != current.own_voice_only {
        patch.set(Key::DictationOwnVoiceOnly, *own_voice_only);
    }
    if *vocabulary_enabled != current.vocabulary_enabled {
        patch.set(Key::DictationVocabularyEnabled, *vocabulary_enabled);
    }
    if *vocabulary_dir != current.vocabulary_dir {
        match vocabulary_dir {
            Some(directory) => patch.set(Key::DictationVocabularyDir, directory.as_str()),
            None => patch.reset(Key::DictationVocabularyDir),
        };
    }
    if *history_enabled != current.history_enabled {
        patch.set(Key::HistoryEnabled, *history_enabled);
    }
    if *history_keep_audio != current.history_keep_audio {
        patch.set(Key::HistoryKeepAudio, *history_keep_audio);
    }
    if *assistant_id != current.assistant_id {
        match assistant_id {
            Some(id) => patch.set(Key::AssistantId, id.name()),
            None => patch.reset(Key::AssistantId),
        };
    }
    if *assistant_models != current.assistant_models {
        patch.set_entries(
            Key::AssistantModels,
            &models(&current.assistant_models),
            &models(assistant_models),
        )?;
    }
    if *memory_enabled != current.memory_enabled {
        patch.set(Key::MemoryEnabled, *memory_enabled);
    }
    if *prompts != current.prompts {
        patch.set_entries(
            Key::Prompts,
            &Keyed::from(&current.prompts),
            &Keyed::from(prompts),
        )?;
    }
    if *application_overrides != current.application_overrides {
        patch.set_entries(
            Key::ApplicationOverrides,
            &Keyed::from(&current.application_overrides),
            &Keyed::from(application_overrides),
        )?;
    }
    if *aria_key != current.aria_key {
        patch.set_encoded(Key::ShortcutsAriaKey, &AriaKeyName::from(*aria_key))?;
    }
    if *ask_shortcut != current.ask_shortcut {
        patch.set_encoded(Key::ShortcutsAsk, &ShortcutEntry::from(*ask_shortcut))?;
    }
    if *prompt_list_shortcut != current.prompt_list_shortcut {
        patch.set_encoded(
            Key::ShortcutsPromptList,
            &ShortcutEntry::from(*prompt_list_shortcut),
        )?;
    }
    if *stt_language != current.stt_language {
        patch.set(Key::SttLanguage, stt_language.as_str());
    }
    if *tts_rate != current.tts_rate {
        patch.set(Key::TtsRate, rate_for_the_file(*tts_rate));
    }
    Ok(patch)
}

/// The rate holds a single-precision value, which would write 1.15 as
/// 1.149999976158142.
fn rate_for_the_file(rate: SpeakingRate) -> f64 {
    (f64::from(rate.get()) * RATE_STEPS_PER_UNIT).round() / RATE_STEPS_PER_UNIT
}

const RATE_STEPS_PER_UNIT: f64 = 100.0;

fn models(models: &BTreeMap<AssistantId, String>) -> Keyed<String> {
    models
        .iter()
        .map(|(id, model)| (id.name().to_owned(), model.clone()))
        .collect()
}

/// `auto_stop_ms = 0` turns auto-stop off.
const AUTO_STOP_OFF: u64 = 0;

fn milliseconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn assistant_id(name: &str, key: Key) -> Result<AssistantId, SettingsError> {
    name.parse().map_err(|_unknown| {
        invalid(
            key,
            InvalidValue::Unsupported {
                value: name.to_owned(),
                supported: AssistantId::ALL
                    .iter()
                    .map(|id| id.name())
                    .collect::<Vec<_>>()
                    .join(" and "),
            },
        )
    })
}

fn shortcut(
    entry: Option<ShortcutEntry>,
    default: Option<super::Shortcut>,
    key: Key,
) -> Result<Option<super::Shortcut>, SettingsError> {
    match entry {
        Some(entry) => entry.shortcut().map_err(|source| invalid(key, source)),
        None => Ok(default),
    }
}

fn overrides(
    entries: &Keyed<super::ApplicationOverrideEntry>,
) -> Result<ApplicationOverrides, InvalidValue> {
    let entries = entries
        .iter()
        .map(|(id, entry)| {
            let id = ApplicationId::new(id).map_err(InvalidValue::Override)?;
            let entry = ApplicationOverride {
                excluded: entry.excluded,
                overrides: DictationOverrides {
                    output_mode: entry.output_mode.map(Into::into),
                    delivery_mode: entry.delivery_mode.map(Into::into),
                },
            };
            Ok((id, entry))
        })
        .collect::<Result<Vec<_>, InvalidValue>>()?;
    ApplicationOverrides::new(entries).map_err(InvalidValue::Override)
}

fn prompt_list(entries: &Keyed<super::PromptEntry>) -> Result<Prompts, InvalidValue> {
    let prompts = entries
        .iter()
        .map(|(id, entry)| {
            let chord = entry.chord.map(|chord| chord.chord()).transpose()?;
            Prompt::new(
                PromptId::new(id).map_err(InvalidValue::Prompt)?,
                entry.name.clone(),
                entry.kind.into(),
                entry.file.clone(),
                entry.enabled,
                chord,
            )
            .map_err(InvalidValue::Prompt)
        })
        .collect::<Result<Vec<_>, InvalidValue>>()?;
    Prompts::new(prompts).map_err(InvalidValue::Prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::OutputMode;
    use crate::hotkey::AriaKey;
    use crate::settings::{Chord, Modifiers, Shortcut};

    fn settings(text: &str) -> SettingsFile {
        crate::settings::Loaded::parse(text, std::path::Path::new("/Users/you"))
            .expect("the text loads")
            .settings
    }

    #[test]
    fn an_empty_file_reads_as_the_defaults() {
        assert_eq!(
            Settings::from_file(&SettingsFile::default()).expect("valid"),
            Settings::default()
        );
    }

    #[test]
    fn every_setting_round_trips_through_its_key() {
        let chord = Chord::new(
            15,
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        )
        .expect("a chord");
        let next = Settings {
            assistant_id: Some(AssistantId::Codex),
            assistant_models: [(AssistantId::Codex, "gpt-x".to_owned())].into(),
            history_enabled: false,
            history_keep_audio: true,
            memory_enabled: true,
            backtrack: true,
            cleanup: true,
            tts_rate: SpeakingRate::new(1.5).expect("valid"),
            mute_sound: true,
            stt_language: "de".parse().expect("a language"),
            output_mode: OutputMode::Copy,
            delivery_mode: crate::settings::DeliveryMode::Complete,
            silence_duration: SilenceDuration::new(Duration::from_millis(2000)).expect("valid"),
            auto_stop: None,
            own_voice_only: true,
            aria_key: Some(AriaKey::Fn),
            ask_shortcut: Some(Shortcut::Chord(chord)),
            prompt_list_shortcut: None,
            vocabulary_enabled: true,
            vocabulary_dir: Some(
                crate::settings::Directory::new("/src/writ".into()).expect("valid"),
            ),
            application_overrides: ApplicationOverrides::new([(
                ApplicationId::new("com.tinyspeck.slackmacgap").expect("valid"),
                ApplicationOverride {
                    excluded: true,
                    overrides: DictationOverrides::default(),
                },
            )])
            .expect("valid"),
            prompts: Prompts::new(vec![
                Prompt::new(
                    PromptId::new("reply").expect("id"),
                    "Reply".to_owned(),
                    crate::settings::PromptKind::Compose,
                    "/p/reply.md".into(),
                    true,
                    None,
                )
                .expect("prompt"),
            ])
            .expect("prompts"),
        };
        let patch = patch(&Settings::default(), &next).expect("encodes");
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        let written = crate::settings::modify(&path, dir.path(), None, |_current| {
            Ok::<_, SettingsError>(patch.clone())
        })
        .expect("writes");
        let crate::settings::Written::Applied(loaded) = written else {
            panic!("{written:?}");
        };

        assert_eq!(Settings::from_file(&loaded.settings).expect("valid"), next);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("[prompts.reply]"), "{text}");
        assert!(
            text.contains("[application_overrides.\"com.tinyspeck.slackmacgap\"]"),
            "{text}"
        );
        assert!(text.contains("auto_stop_ms = 0"), "{text}");
    }

    #[test]
    fn an_unchanged_setting_writes_nothing() {
        let current =
            Settings::from_file(&settings("[dictation]\nmute_sound = true\n")).expect("valid");
        assert!(patch(&current, &current).expect("encodes").is_empty());
    }

    #[test]
    fn an_aria_key_that_is_off_survives_an_unrelated_write() {
        let current =
            Settings::from_file(&settings("[shortcuts]\naria_key = \"off\"\n")).expect("valid");
        let next = Settings {
            mute_sound: true,
            ..current.clone()
        };
        let keys: Vec<Key> = patch(&current, &next).expect("encodes").keys().collect();
        assert_eq!(keys, [Key::DictationMuteSound]);
    }
}
