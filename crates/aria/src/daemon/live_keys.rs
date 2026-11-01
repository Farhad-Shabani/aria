//! Every live key, proved live. Each case edits the file as a person would,
//! polls as the daemon does, and reads the value through the call the next
//! operation makes. A new key fails to compile here until it is classified.
//!
//! The reads stop short of running an operation, which would take a model, a
//! microphone or an assistant. Bugs of this class come from a value captured
//! once, and every read here goes through the per-operation entry point:
//! `DictationSettings::resolve` for a press, `next_utterance` for
//! speech, and the config load an ask, a memory run and a listen session each
//! make on start.

use super::speech::next_utterance;
use crate::config::{ChatConfig, ListenConfig, MemoryConfig, TtsEngine};
use crate::delivery::{Application, PlatformTextDelivery, TextDelivery};
use crate::dictation::{DictationRequest, StopMode};
use crate::settings::{Apply, Key, Patch, SettingsCache, SettingsError, SettingsStore, modify};
use crate::speech::SpeechRequest;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use toml_edit::Value;

/// Names the output mode because its default differs by platform, and each
/// case must change what this file holds.
const BASE: &str = "\
[dictation]
output_mode = \"insert\"

[dictation.vocabulary]
enabled = true
dir = \"/project\"

[listen]
backend = \"parakeet\"
";

struct LiveHarness {
    path: std::path::PathBuf,
    home: std::path::PathBuf,
    cache: Arc<SettingsCache>,
    settings: SettingsStore,
}

impl LiveHarness {
    fn start(home: &Path) -> Self {
        let path = home.join("settings.toml");
        fs::write(&path, BASE).unwrap();
        age(&path);
        let cache = Arc::new(SettingsCache::open(path.clone(), home.to_path_buf()).unwrap());
        let settings = SettingsStore::new(Arc::clone(&cache));
        Self {
            path,
            home: home.to_path_buf(),
            cache,
            settings,
        }
    }

    /// A writer other than the daemon, then the poll that finds it.
    fn edit_file(&self, patch: Patch) {
        modify::<SettingsError>(&self.path, &self.home, None, |_current| Ok(patch)).unwrap();
        age(&self.path);
        self.cache.poll().unwrap();
    }

    fn next_press(&self) -> String {
        let application = Application {
            name: "App".into(),
            bundle_id: Some("com.app".into()),
        };
        let settings = crate::dictation::DictationSettings::resolve(
            self.settings.snapshot().unwrap().settings,
            DictationRequest::new(StopMode::OnSilence),
            Some(application),
        );
        format!("{settings:?}")
    }

    fn next_setting_read(&self) -> String {
        format!("{:?}", self.settings.snapshot().unwrap().settings)
    }

    fn next_ask(&self) -> String {
        let settings = self.cache.effective();
        format!("{:?}", ChatConfig::load(&settings))
    }

    fn next_memory_run(&self) -> String {
        let settings = self.cache.effective();
        format!("{:?}", MemoryConfig::load(&settings.memory, &self.home))
    }

    fn next_listen(&self) -> String {
        let settings = self.cache.effective();
        format!("{:?}", ListenConfig::load(&settings, &self.home))
    }

    fn next_utterance(&self, engine: TtsEngine) -> String {
        let request = SpeechRequest {
            text: aria_kit::synthesis::SpeechText::new("hello").unwrap(),
            voice: None,
            rate: None,
        };
        let utterance = next_utterance(&self.cache, request, engine).unwrap();
        format!("{:?} {:?}", utterance.voice, utterance.rate)
    }

    /// What the operation that reads `key` sees next.
    fn read(&self, key: Key) -> String {
        match key {
            Key::DictationOutputMode
            | Key::DictationDeliveryMode
            | Key::DictationSilenceDurationMs
            | Key::DictationAutoStopMs
            | Key::DictationMuteSound
            | Key::DictationCleanup
            | Key::DictationBacktrack
            | Key::SttLanguage
            | Key::DictationOwnVoiceOnly
            | Key::DictationVocabularyEnabled
            | Key::DictationVocabularyDir
            | Key::HistoryEnabled
            | Key::HistoryKeepAudio
            | Key::ApplicationOverrides => self.next_press(),
            // The app and an ask read these from the settings.
            Key::AssistantId
            | Key::AssistantModels
            | Key::ShortcutsAsk
            | Key::ShortcutsPromptList
            | Key::Prompts => self.next_setting_read(),
            Key::ShortcutsAriaKey => format!("{:?}", *self.cache.aria_key().borrow()),
            Key::AssistantWorkdir | Key::AssistantProjectRoots | Key::AssistantTimeoutSecs => {
                self.next_ask()
            }
            Key::MemoryEnabled | Key::MemoryExtractModel | Key::MemoryMaxFactsPerSession => {
                self.next_memory_run()
            }
            Key::TtsRate | Key::TtsSystemVoice => self.next_utterance(TtsEngine::System),
            Key::TtsKokoroVoice => self.next_utterance(TtsEngine::Kokoro),
            Key::TtsQwenVoice => self.next_utterance(TtsEngine::Qwen),
            Key::ListenBackend
            | Key::ListenModel
            | Key::ListenDevice
            | Key::ListenTranscriptsDir => self.next_listen(),
            Key::DictationCleanupLlmEndpoint
            | Key::DictationCleanupLlmModel
            | Key::MemoryDir
            | Key::PhoneEnabled
            | Key::PhonePort
            | Key::WakeEnabled
            | Key::WakeBackend
            | Key::WakeModel
            | Key::WakeThreshold
            | Key::TtsBackend
            | Key::TtsFallback
            | Key::TtsKokoroModel
            | Key::TtsKokoroVoices
            | Key::TtsQwenModel
            | Key::TtsQwenTokenizer
            | Key::SttBackend
            | Key::SttModel
            | Key::SttDevice
            | Key::SttInterpreter
            | Key::SttServer
            | Key::SpeakerModel
            | Key::SpeakerSegmentationModel
            | Key::SpeakerProfilesDir
            | Key::DictationMaxRecordingSecs
            | Key::DeliveryTimeoutSecs
            | Key::DictationTimeoutSecs
            | Key::CaptureBackend
            | Key::CapturePath
            | Key::CapturePace => panic!("{key} is a restart key"),
        }
    }
}

/// Past the settling time, so a case doesn't wait it out.
fn age(path: &Path) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(1))
        .unwrap();
}

fn set(key: Key, value: impl Into<Value>) -> Patch {
    let mut patch = Patch::new();
    patch.set(key, value);
    patch
}

fn entry(key: Key, id: &str, literal: &str) -> Patch {
    let mut patch = Patch::new();
    patch.set_below(key, vec![id.to_owned()], literal.parse().unwrap());
    patch
}

/// An edit that changes what `key` holds from `BASE`.
fn change(key: Key) -> Patch {
    match key {
        Key::DictationOutputMode => set(key, "copy"),
        Key::DictationDeliveryMode => set(key, "complete"),
        Key::DictationSilenceDurationMs => set(key, 2000),
        Key::DictationAutoStopMs => set(key, 20000),
        Key::SttLanguage => set(key, "de"),
        Key::DictationVocabularyEnabled | Key::HistoryEnabled => set(key, false),
        Key::DictationVocabularyDir => set(key, "/other"),
        Key::AssistantId => set(key, "codex"),
        Key::AssistantModels => entry(key, "codex", "\"a-model\""),
        Key::AssistantWorkdir => set(key, "/work"),
        Key::AssistantProjectRoots => {
            let roots: Value = "[\"/roots\"]".parse().unwrap();
            set(key, roots)
        }
        Key::AssistantTimeoutSecs => set(key, 60.0),
        Key::MemoryExtractModel => set(key, "opus"),
        Key::MemoryMaxFactsPerSession => set(key, 3),
        Key::ShortcutsAriaKey | Key::ShortcutsAsk | Key::ShortcutsPromptList => set(key, "off"),
        Key::Prompts => entry(
            key,
            "reply",
            "{ name = \"Reply\", kind = \"compose\", file = \"/reply.md\" }",
        ),
        Key::ApplicationOverrides => entry(key, "com.app", "{ output_mode = \"copy\" }"),
        Key::TtsRate => set(key, 1.5),
        Key::TtsSystemVoice | Key::TtsKokoroVoice | Key::TtsQwenVoice => set(key, "a-voice"),
        Key::ListenBackend => set(key, "moonshine"),
        Key::ListenModel => set(key, "/model"),
        Key::ListenDevice => set(key, "gpu"),
        Key::ListenTranscriptsDir => set(key, "/transcripts"),
        Key::DictationMuteSound
        | Key::DictationCleanup
        | Key::DictationBacktrack
        | Key::DictationOwnVoiceOnly
        | Key::HistoryKeepAudio
        | Key::MemoryEnabled => set(key, true),
        Key::DictationCleanupLlmEndpoint
        | Key::DictationCleanupLlmModel
        | Key::MemoryDir
        | Key::PhoneEnabled
        | Key::PhonePort
        | Key::WakeEnabled
        | Key::WakeBackend
        | Key::WakeModel
        | Key::WakeThreshold
        | Key::TtsBackend
        | Key::TtsFallback
        | Key::TtsKokoroModel
        | Key::TtsKokoroVoices
        | Key::TtsQwenModel
        | Key::TtsQwenTokenizer
        | Key::SttBackend
        | Key::SttModel
        | Key::SttDevice
        | Key::SttInterpreter
        | Key::SttServer
        | Key::SpeakerModel
        | Key::SpeakerSegmentationModel
        | Key::SpeakerProfilesDir
        | Key::DictationMaxRecordingSecs
        | Key::DeliveryTimeoutSecs
        | Key::DictationTimeoutSecs
        | Key::CaptureBackend
        | Key::CapturePath
        | Key::CapturePace => panic!("{key} is frozen at startup, so it has no live change"),
    }
}

#[test]
fn the_next_operation_sees_every_live_key_change() {
    let live = Key::ALL
        .iter()
        .copied()
        .filter(|key| key.apply() == Apply::Live)
        // Pasting forces complete delivery regardless of this setting.
        .filter(|key| PlatformTextDelivery::STREAMS || *key != Key::DictationDeliveryMode);
    for key in live {
        let home = tempfile::tempdir().unwrap();
        let harness = LiveHarness::start(home.path());
        let before = harness.read(key);

        harness.edit_file(change(key));

        let after = harness.read(key);
        assert_ne!(before, after, "{key} did not reach the next operation");
    }
}
