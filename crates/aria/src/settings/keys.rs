//! The one list of settings keys. Every table built on the keys, from the
//! phone allowlist to the restart diff, derives from this declaration, so a
//! key that isn't here can't be read, written or shown.

use std::borrow::Cow;
use std::fmt;

use crate::config::{backend, tts_backend, wake_backend};
use crate::delivery::{OutputMode, PlatformTextDelivery, TextDelivery};

/// When a change takes effect: on the next operation, or after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    Live,
    Restart,
}

/// Who may write the key. The phone's allowlist is every `Phone` key; a key
/// that controls egress or runs code stays `Mac`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Mac,
    Phone,
}

/// The shape of a key's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    Integer,
    Number,
    Text,
    /// Text, with `~` for the home directory.
    Path,
    PathList,
    /// One of these words.
    Choice(&'static [&'static str]),
    /// off, shift, command, or a chord table.
    ShortcutEntry,
    /// `[assistant.models]`: a model ID per assistant.
    Models,
    /// `[prompts.<id>]` tables.
    Prompts,
    /// `[application_overrides."<bundle id>"]` tables.
    Overrides,
}

impl Kind {
    /// A key that holds a table of entries, which `set` never replaces whole.
    pub(crate) fn is_table(self) -> bool {
        match self {
            Self::Models | Self::Prompts | Self::Overrides => true,
            Self::Bool
            | Self::Integer
            | Self::Number
            | Self::Text
            | Self::Path
            | Self::PathList
            | Self::Choice(_)
            | Self::ShortcutEntry => false,
        }
    }
}

pub(super) const OUTPUT_MODES: &[&str] = &["insert", "insert_and_submit", "insert_or_copy", "copy"];
pub(super) const DELIVERY_MODES: &[&str] = &["streaming", "complete"];
const DEFAULT_OUTPUT_MODE: &str = match PlatformTextDelivery::DEFAULT_MODE {
    OutputMode::Insert => "insert",
    OutputMode::InsertAndSubmit => "insert_and_submit",
    OutputMode::InsertOrCopy => "insert_or_copy",
    OutputMode::Copy => "copy",
};
pub(super) const STT_BACKENDS: &[&str] = &[
    backend::WHISPER,
    backend::WHISPER_CANDLE,
    backend::PARAKEET,
    backend::MOONSHINE,
    backend::NEMOTRON,
    backend::PARAKEET_MLX,
    backend::SERVER,
];
pub(super) const WAKE_BACKENDS: &[&str] = &[wake_backend::OPEN_WAKE_WORD, wake_backend::LIVEKIT];
pub(super) const LISTEN_BACKENDS: &[&str] =
    &[backend::PARAKEET, backend::MOONSHINE, backend::NEMOTRON];

macro_rules! keys {
    ($($variant:ident: $path:literal, $apply:ident, $scope:ident, [$($alias:literal),*], $kind:expr, $default:expr, $description:literal;)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[allow(
            clippy::enum_variant_names,
            reason = "a variant is named for its path, and one path ends in aria_key"
        )]
        pub enum Key {
            $($variant,)+
        }

        impl Key {
            pub(crate) const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The dotted TOML path.
            pub(crate) fn path(self) -> &'static str {
                match self {
                    $(Self::$variant => $path,)+
                }
            }

            pub(crate) fn apply(self) -> Apply {
                match self {
                    $(Self::$variant => Apply::$apply,)+
                }
            }

            pub(crate) fn scope(self) -> Scope {
                match self {
                    $(Self::$variant => Scope::$scope,)+
                }
            }

            /// Older dotted paths that load as this key. Only plain renames; a
            /// change of shape can't alias.
            pub(crate) fn aliases(self) -> &'static [&'static str] {
                match self {
                    $(Self::$variant => &[$($alias),*],)+
                }
            }

            pub(crate) fn description(self) -> &'static str {
                match self {
                    $(Self::$variant => $description,)+
                }
            }

            /// What the value is, for the schema and for `aria settings set`.
            pub(crate) fn kind(self) -> Kind {
                match self {
                    $(Self::$variant => $kind,)+
                }
            }

            /// What an absent key takes: the bare word of a `Choice`, a TOML
            /// literal otherwise.
            fn default_text(self) -> Option<&'static str> {
                match self {
                    $(Self::$variant => $default,)+
                }
            }
        }
    };
}

keys! {
    DictationOutputMode: "dictation.output_mode", Live, Phone, [], Kind::Choice(OUTPUT_MODES), Some(DEFAULT_OUTPUT_MODE),
        "How text reaches the app in front: insert, insert_and_submit, insert_or_copy or copy.";
    DictationDeliveryMode: "dictation.delivery_mode", Live, Mac, [], Kind::Choice(DELIVERY_MODES), Some("streaming"),
        "streaming types as you speak. complete types once recording stops.";
    DictationSilenceDurationMs: "dictation.silence_duration_ms", Live, Mac, [], Kind::Integer, Some("1500"),
        "Silence, in ms, that ends a recording that stops on silence. 100 to 10000.";
    DictationAutoStopMs: "dictation.auto_stop_ms", Live, Mac, [], Kind::Integer, Some("10000"),
        "Silence, in ms, that ends a tapped recording. 3000 to 60000. 0 is off.";
    DictationMaxRecordingSecs: "dictation.max_recording_secs", Restart, Mac, [], Kind::Number, Some("600.0"),
        "The longest one recording may run.";
    DictationTimeoutSecs: "dictation.timeout_secs", Restart, Mac, [], Kind::Number, Some("660.0"),
        "The longest a whole dictation may take, recording included.";
    DictationMuteSound: "dictation.mute_sound", Live, Mac, [], Kind::Bool, Some("false"),
        "Mutes your Mac's sound while recording.";
    DictationCleanup: "dictation.cleanup", Live, Mac, [], Kind::Bool, Some("false"),
        "Drops \"um\", \"uh\" and repeated words before delivery.";
    DictationBacktrack: "dictation.backtrack", Live, Mac, [], Kind::Bool, Some("false"),
        "Applies a spoken self-correction. English only.";
    DictationOwnVoiceOnly: "dictation.own_voice_only", Live, Phone, [], Kind::Bool, Some("false"),
        "Types only what your enrolled voice said.";
    DictationCleanupLlmEndpoint: "dictation.cleanup_llm.endpoint", Restart, Mac, [], Kind::Text, None,
        "A loopback chat-completions endpoint that also cleans the transcript while cleanup is on.";
    DictationCleanupLlmModel: "dictation.cleanup_llm.model", Restart, Mac, [], Kind::Text, None,
        "The model that endpoint serves.";
    DictationVocabularyEnabled: "dictation.vocabulary.enabled", Live, Phone, [], Kind::Bool, Some("false"),
        "Adds terms from the project you're working in.";
    DictationVocabularyDir: "dictation.vocabulary.dir", Live, Phone, [], Kind::Path, None,
        "The project folder to read terms from. Absent means the folder the request names.";
    HistoryEnabled: "history.enabled", Live, Phone, [], Kind::Bool, Some("true"),
        "Saves dictation history.";
    HistoryKeepAudio: "history.keep_audio", Live, Phone, [], Kind::Bool, Some("false"),
        "Keeps each recording's audio for 24 hours.";
    ListenBackend: "listen.backend", Live, Mac, [], Kind::Choice(LISTEN_BACKENDS), None,
        "Transcribes system audio: parakeet, moonshine or nemotron.";
    ListenModel: "listen.model", Live, Mac, [], Kind::Path, None,
        "Absent means setup's download for that engine.";
    ListenDevice: "listen.device", Live, Mac, [], Kind::Choice(&["cpu", "gpu"]), None,
        "cpu or gpu. parakeet only.";
    ListenTranscriptsDir: "listen.transcripts_dir", Live, Mac, [], Kind::Path, Some("\"~/.aria/transcripts\""),
        "Where listen writes transcripts.";
    AssistantId: "assistant.id", Live, Mac, [], Kind::Choice(&["claude-code", "codex"]), Some("claude-code"),
        "Who answers `aria ask`: claude-code or codex.";
    AssistantModels: "assistant.models", Live, Mac, [], Kind::Models, None,
        "The model each assistant uses. A missing assistant uses its own default.";
    AssistantWorkdir: "assistant.workdir", Live, Mac, [], Kind::Path, None,
        "Where the assistant works. Absent means the caller's directory.";
    AssistantProjectRoots: "assistant.project_roots", Live, Mac, [], Kind::PathList, Some("[]"),
        "Folders whose git repos an ask may name.";
    AssistantTimeoutSecs: "assistant.timeout_secs", Live, Mac, [], Kind::Number, Some("300.0"),
        "The longest a reply may take, at most 570.";
    MemoryEnabled: "memory.enabled", Live, Mac, [], Kind::Bool, Some("false"),
        "Remembers facts you state while asking, and shows them to the assistant later.";
    MemoryExtractModel: "memory.extract_model", Live, Mac, [], Kind::Text, Some("\"sonnet\""),
        "The model that extracts facts after an ask.";
    MemoryMaxFactsPerSession: "memory.max_facts_per_session", Live, Mac, [], Kind::Integer, Some("10"),
        "The most facts kept from one ask session.";
    MemoryDir: "memory.dir", Restart, Mac, [], Kind::Path, Some("\"~/.aria/memory\""),
        "Holds the facts, and lexicon.txt, the terms that fix misheard words.";
    Prompts: "prompts", Live, Mac, [], Kind::Prompts, None,
        "What to make of a recording, keyed by id. At most 16, each chord once, listed in file order.";
    ApplicationOverrides: "application_overrides", Live, Mac, [], Kind::Overrides, None,
        "Settings for one app while it's in front, keyed by bundle id. At most 64.";
    ShortcutsAriaKey: "shortcuts.aria_key", Live, Mac, [], Kind::Choice(&["right_option", "fn", "off"]), Some("right_option"),
        "The Aria key: right_option, fn or off. Tap to dictate, hold to talk.";
    ShortcutsAsk: "shortcuts.ask", Live, Mac, [], Kind::ShortcutEntry, Some("\"shift\""),
        "Starts an ask: shift or command held with the Aria key, a chord, or off.";
    ShortcutsPromptList: "shortcuts.prompt_list", Live, Mac, [], Kind::ShortcutEntry, Some("\"command\""),
        "Lists the prompts that are on: shift, command, a chord, or off.";
    WakeEnabled: "wake.enabled", Restart, Mac, [], Kind::Bool, Some("false"),
        "Starts dictation when you say the wake word.";
    WakeBackend: "wake.backend", Restart, Mac, [], Kind::Choice(WAKE_BACKENDS), Some(wake_backend::DEFAULT),
        "openwakeword or livekit.";
    WakeModel: "wake.model", Restart, Mac, [], Kind::Path, Some("\"~/.aria/models/wakeword/aria.onnx\""),
        "The classifier. livekit has no default.";
    WakeThreshold: "wake.threshold", Restart, Mac, [], Kind::Number, Some("0.5"),
        "Detection score from 0 to 1. Higher means fewer false starts.";
    PhoneEnabled: "phone.enabled", Restart, Mac, [], Kind::Bool, Some("false"),
        "Takes recordings from your paired iPhone or Apple Watch on the local network.";
    PhonePort: "phone.port", Restart, Mac, [], Kind::Integer, Some("9850"),
        "The port it listens on, with mutual TLS.";
    DeliveryTimeoutSecs: "delivery.timeout_secs", Restart, Mac, [], Kind::Number, Some("5.0"),
        "The longest typing or pasting a result may take, for dictation, asks and the phone.";
    SttBackend: "stt.backend", Restart, Mac, [], Kind::Choice(STT_BACKENDS), Some(backend::DEFAULT),
        "whisper, whisper-candle, parakeet, moonshine, nemotron, parakeet-mlx or server.";
    SttLanguage: "stt.language", Live, Phone, [], Kind::Text, Some("\"en\""),
        "A language code such as \"en\" or \"en-us\", or \"auto\" to detect.";
    SttModel: "stt.model", Restart, Mac, [], Kind::Text, None,
        "A model file or folder. parakeet-mlx takes a Hugging Face repo ID.";
    SttDevice: "stt.device", Restart, Mac, [], Kind::Choice(&["cpu", "gpu"]), None,
        "cpu or gpu. parakeet only.";
    SttInterpreter: "stt.interpreter", Restart, Mac, [], Kind::Path, None,
        "A Python with parakeet-mlx installed. parakeet-mlx only, and required there.";
    SttServer: "stt.server", Restart, Mac, [], Kind::Text, None,
        "Your paired server's HTTPS origin. server only, and required there.";
    TtsBackend: "tts.backend", Restart, Mac, [], Kind::Choice(&[tts_backend::SYSTEM, tts_backend::KOKORO, tts_backend::QWEN]), Some(tts_backend::DEFAULT),
        "The engine that speaks `aria say` and replies: system, kokoro or qwen.";
    TtsRate: "tts.rate", Live, Mac, [], Kind::Number, Some("1.0"),
        "A multiplier on the voice's natural pace, 0.25 to 4.";
    TtsFallback: "tts.fallback", Restart, Mac, [], Kind::Choice(&["none", "system"]), None,
        "Speaks through the system voice if the model can't load: none or system.";
    TtsSystemVoice: "tts.system.voice", Live, Mac, [], Kind::Text, None,
        "One of `say`'s voices on macOS, or espeak-ng's on Linux.";
    TtsKokoroModel: "tts.kokoro.model", Restart, Mac, [], Kind::Path, Some("\"~/.aria/models/kokoro/kokoro.onnx\""),
        "The Kokoro model file.";
    TtsKokoroVoices: "tts.kokoro.voices", Restart, Mac, [], Kind::Path, Some("\"~/.aria/models/kokoro/voices.bin\""),
        "The archive of every Kokoro voice.";
    TtsKokoroVoice: "tts.kokoro.voice", Live, Mac, [], Kind::Text, None,
        "One voice from that archive.";
    TtsQwenModel: "tts.qwen.model", Restart, Mac, [], Kind::Path, Some("\"~/.aria/models/qwen/qwen3-tts-0.6b-customvoice\""),
        "The Qwen model folder.";
    TtsQwenTokenizer: "tts.qwen.tokenizer", Restart, Mac, [], Kind::Path, Some("\"~/.aria/models/qwen/qwen3-tts-tokenizer\""),
        "The 12 Hz speech tokenizer snapshot.";
    TtsQwenVoice: "tts.qwen.voice", Live, Mac, [], Kind::Text, None,
        "A preset voice name.";
    SpeakerModel: "speaker.model", Restart, Mac, [], Kind::Path, Some("\"~/.aria/models/wespeaker/en/model.onnx\""),
        "Tells voices apart. Without it, transcripts have no speaker labels.";
    SpeakerSegmentationModel: "speaker.segmentation_model", Restart, Mac, [], Kind::Path, Some("\"~/.aria/models/pyannote/segmentation-3.0.onnx\""),
        "Estimates who speaks in each audio frame, for Listen.";
    SpeakerProfilesDir: "speaker.profiles_dir", Restart, Mac, [], Kind::Path, Some("\"~/.aria/speakers\""),
        "Enrolled voice profiles.";
    CaptureBackend: "capture.backend", Restart, Mac, [], Kind::Choice(&["microphone", "wav_file"]), Some("microphone"),
        "Testing only. microphone or wav_file.";
    CapturePath: "capture.path", Restart, Mac, [], Kind::Path, None,
        "Testing only. The WAV file. wav_file only, and required there.";
    CapturePace: "capture.pace", Restart, Mac, [], Kind::Choice(&["immediate", "realtime"]), None,
        "Testing only. immediate or realtime. wav_file only.";
}

impl Key {
    /// The value an absent key takes, as a TOML literal. `None` when absent
    /// means something no single value says, such as "the caller's directory"
    /// or "the engine's own voice".
    pub(crate) fn default(self) -> Option<Cow<'static, str>> {
        let text = self.default_text()?;
        Some(match self.kind() {
            Kind::Choice(_) => format!("\"{text}\"").into(),
            Kind::Bool
            | Kind::Integer
            | Kind::Number
            | Kind::Text
            | Kind::Path
            | Kind::PathList
            | Kind::ShortcutEntry
            | Kind::Models
            | Kind::Prompts
            | Kind::Overrides => text.into(),
        })
    }

    /// The key a path names, by its own path or an alias.
    pub(crate) fn from_path(path: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|key| key.path() == path || key.aliases().contains(&path))
    }

    pub(crate) fn segments(self) -> std::str::Split<'static, char> {
        self.path().split('.')
    }
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_and_alias_names_one_key() {
        let mut seen = std::collections::HashSet::new();
        for key in Key::ALL {
            for path in std::iter::once(key.path()).chain(key.aliases().iter().copied()) {
                assert!(seen.insert(path), "{path} names two keys");
                assert_eq!(Key::from_path(path), Some(*key), "{path}");
            }
        }
        assert_eq!(Key::from_path("chat.nothing"), None);
    }

    #[test]
    fn every_word_of_a_choice_parses() {
        for key in Key::ALL {
            let Kind::Choice(words) = key.kind() else {
                continue;
            };
            let (table, name) = key.path().rsplit_once('.').expect("a key sits in a table");
            for word in words {
                let text = format!("[{table}]\n{name} = \"{word}\"\n");
                toml_edit::de::from_str::<super::super::SettingsFile>(&text)
                    .unwrap_or_else(|error| panic!("{key} = {word}: {error}"));
            }
        }
    }

    #[test]
    fn the_assistant_choices_are_every_assistant() {
        let names: Vec<&str> = crate::config::AssistantId::ALL
            .iter()
            .map(|id| id.name())
            .collect();
        let Kind::Choice(words) = Key::AssistantId.kind() else {
            panic!("assistant.id is a choice");
        };
        assert_eq!(words, names);
    }

    #[test]
    fn the_phone_writes_no_egress_or_code_key() {
        let phone: Vec<Key> = Key::ALL
            .iter()
            .copied()
            .filter(|key| key.scope() == Scope::Phone)
            .collect();
        assert_eq!(
            phone,
            [
                Key::DictationOutputMode,
                Key::DictationOwnVoiceOnly,
                Key::DictationVocabularyEnabled,
                Key::DictationVocabularyDir,
                Key::HistoryEnabled,
                Key::HistoryKeepAudio,
                Key::SttLanguage,
            ]
        );
    }
}
