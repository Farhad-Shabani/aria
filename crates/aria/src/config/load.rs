//! Resolution of configuration-file values, environment overrides, and defaults.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::audio::ReplayPace;
use crate::dictation::MAX_DICTATION_TIMEOUT;
use crate::primitives::user_home;

use aria_kit::synthesis::{SpeakingRate, Voice};

use super::WakeWordOverride;
use super::file::{self, FileConfig};
use super::{
    CaptureConfig, ConfigError, DaemonConfig, DictationConfig, DictationTimeouts, HotkeyConfig,
    ListenConfig, ListenTranscription, MemoryConfig, RelayConfig, SpeakerConfig,
    TranscriptionConfig, TranscriptionDevice, TtsBackend, TtsConfig, TtsFallback, WakeBackend,
    WakeConfig, backend, tts_backend, wake_backend,
};
use super::{ChatAgent, ChatConfig, chat_agent};

pub(super) const DEFAULT_CONFIG_RELATIVE_PATH: &str = ".aria/config.yaml";

const DEFAULT_BACKEND: &str = backend::WHISPER;
const DEFAULT_WHISPER_MODEL: &str = "~/models/whisper/ggml-large-v3-turbo-q8_0.bin";
const DEFAULT_WHISPER_CANDLE_MODEL: &str = "~/models/whisper-candle/tiny.en";
const DEFAULT_PARAKEET_MODEL: &str = "~/models/parakeet/parakeet-tdt-0.6b-v3";
const DEFAULT_MOONSHINE_MODEL: &str = "~/models/moonshine/base";
const DEFAULT_NEMOTRON_MODEL: &str = "~/models/nemotron/nemotron-speech-streaming-en-0.6b";
const DEFAULT_WAKE_BACKEND: &str = wake_backend::OPEN_WAKE_WORD;
const DEFAULT_WAKE_MODEL: &str = "~/models/wakeword/aria.onnx";
const DEFAULT_WAKE_THRESHOLD: f32 = 0.5;
/// A HuggingFace repo id, resolved by the sidecar against its own cache.
const DEFAULT_PARAKEET_MLX_MODEL: &str = "mlx-community/parakeet-tdt-0.6b-v3";
const DEFAULT_RECORDING_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_OVERALL_TIMEOUT: Duration = Duration::from_secs(320);

pub(super) const DEFAULT_CAPTURE_BACKEND: &str = "microphone";
pub(super) const WAV_FILE_CAPTURE_BACKEND: &str = "wav-file";

const DEFAULT_TTS_BACKEND: &str = tts_backend::SYSTEM;
const DEFAULT_CHAT_AGENT: &str = chat_agent::CLAUDE_CODE;
/// Long enough for an agent to read a repo and think.
const DEFAULT_CHAT_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_KOKORO_MODEL: &str = "~/models/kokoro/kokoro.onnx";
const DEFAULT_KOKORO_VOICES: &str = "~/models/kokoro/voices.bin";
const DEFAULT_QWEN_MODEL_DIR: &str = "~/models/qwen/qwen3-tts-0.6b-customvoice";
const DEFAULT_QWEN_TOKENIZER_DIR: &str = "~/models/qwen/qwen3-tts-tokenizer";

const DEFAULT_LISTEN_TRANSCRIPTS_DIR: &str = "~/.aria/transcripts";
const DEFAULT_SPEAKER_MODEL: &str = "~/.wespeaker/en/model.onnx";
const DEFAULT_SEGMENTATION_MODEL: &str = "~/models/pyannote/segmentation-3.0.onnx";
const DEFAULT_SPEAKERS_DIR: &str = "~/.aria/speakers";
const DEFAULT_MEMORY_DIR: &str = "~/.aria/memory";
const DEFAULT_MEMORY_EXTRACT_MODEL: &str = "sonnet";
const DEFAULT_MEMORY_MAX_FACTS: u32 = 10;

pub(super) const CAPTURE_BACKEND_ENV: &str = "ARIA_CAPTURE_BACKEND";
pub(super) const CAPTURE_FILE_ENV: &str = "ARIA_CAPTURE_FILE";
pub(super) const CAPTURE_PACE_ENV: &str = "ARIA_CAPTURE_PACE";
pub(super) const BACKEND_ENV: &str = "ARIA_TRANSCRIPTION_BACKEND";
pub(super) const MODEL_ENV: &str = "ARIA_TRANSCRIPTION_MODEL";
pub(super) const DEVICE_ENV: &str = "ARIA_TRANSCRIPTION_DEVICE";
pub(super) const INTERPRETER_ENV: &str = "ARIA_TRANSCRIPTION_INTERPRETER";
pub(super) const LISTEN_BACKEND_ENV: &str = "ARIA_LISTEN_BACKEND";
pub(super) const LISTEN_MODEL_ENV: &str = "ARIA_LISTEN_MODEL";
pub(super) const LISTEN_DEVICE_ENV: &str = "ARIA_LISTEN_DEVICE";
pub(super) const LISTEN_TRANSCRIPTS_DIR_ENV: &str = "ARIA_LISTEN_TRANSCRIPTS_DIR";
pub(super) const SPEAKER_MODEL_ENV: &str = "ARIA_SPEAKER_MODEL";
pub(super) const SEGMENTATION_MODEL_ENV: &str = "ARIA_SEGMENTATION_MODEL";
pub(super) const SPEAKERS_DIR_ENV: &str = "ARIA_SPEAKERS_DIR";
pub(super) const MEMORY_ENABLED_ENV: &str = "ARIA_MEMORY_ENABLED";
pub(super) const MEMORY_EXTRACT_MODEL_ENV: &str = "ARIA_MEMORY_EXTRACT_MODEL";
pub(super) const MEMORY_MAX_FACTS_ENV: &str = "ARIA_MEMORY_MAX_FACTS";
pub(super) const MEMORY_DIR_ENV: &str = "ARIA_MEMORY_DIR";
pub(super) const WAKE_ENABLED_ENV: &str = "ARIA_WAKE_ENABLED";
pub(super) const HOTKEY_ENABLED_ENV: &str = "ARIA_HOTKEY_ENABLED";
pub(super) const DICTATION_CLEANUP_ENV: &str = "ARIA_DICTATION_CLEANUP";
pub(super) const DICTATION_BACKTRACK_ENV: &str = "ARIA_DICTATION_BACKTRACK";
pub(super) const WAKE_BACKEND_ENV: &str = "ARIA_WAKE_BACKEND";
pub(super) const WAKE_MODEL_ENV: &str = "ARIA_WAKE_MODEL";
pub(super) const WAKE_THRESHOLD_ENV: &str = "ARIA_WAKE_THRESHOLD";
pub(super) const TTS_BACKEND_ENV: &str = "ARIA_TTS_BACKEND";
pub(super) const TTS_MODEL_ENV: &str = "ARIA_TTS_MODEL";
pub(super) const TTS_VOICES_ENV: &str = "ARIA_TTS_VOICES";
pub(super) const TTS_TOKENIZER_ENV: &str = "ARIA_TTS_TOKENIZER";
pub(super) const TTS_VOICE_ENV: &str = "ARIA_TTS_VOICE";
pub(super) const TTS_RATE_ENV: &str = "ARIA_TTS_RATE";
pub(super) const TTS_FALLBACK_ENV: &str = "ARIA_TTS_FALLBACK";
pub(super) const CHAT_AGENT_ENV: &str = "ARIA_CHAT_AGENT";
pub(super) const CHAT_TIMEOUT_ENV: &str = "ARIA_CHAT_TIMEOUT";
pub(super) const RECORDING_TIMEOUT_ENV: &str = "ARIA_DICTATION_RECORDING_TIMEOUT";
pub(super) const DELIVERY_TIMEOUT_ENV: &str = "ARIA_DICTATION_DELIVERY_TIMEOUT";
pub(super) const OVERALL_TIMEOUT_ENV: &str = "ARIA_DICTATION_OVERALL_TIMEOUT";
pub(crate) const UNATTENDED_ENV: &str = "ARIA_UNATTENDED";

/// The environment variables config resolution reads, captured up front
/// so resolution takes plain values.
#[derive(Clone)]
pub(crate) struct Environment {
    pub(super) home: PathBuf,
    pub(super) capture_backend: Option<String>,
    pub(super) capture_file: Option<String>,
    pub(super) capture_pace: Option<String>,
    pub(super) transcription_backend: Option<String>,
    pub(super) transcription_model: Option<String>,
    pub(super) transcription_device: Option<String>,
    pub(super) transcription_interpreter: Option<String>,
    pub(super) listen_backend: Option<String>,
    pub(super) listen_model: Option<String>,
    pub(super) listen_device: Option<String>,
    pub(super) listen_transcripts_dir: Option<String>,
    pub(super) speaker_model: Option<String>,
    pub(super) segmentation_model: Option<String>,
    pub(super) speakers_dir: Option<String>,
    pub(super) memory_enabled: Option<String>,
    pub(super) memory_extract_model: Option<String>,
    pub(super) memory_max_facts: Option<String>,
    pub(super) memory_dir: Option<String>,
    pub(super) tts_backend: Option<String>,
    pub(super) tts_model: Option<String>,
    pub(super) tts_voices: Option<String>,
    pub(super) tts_tokenizer: Option<String>,
    pub(super) tts_voice: Option<String>,
    pub(super) tts_rate: Option<String>,
    pub(super) tts_fallback: Option<String>,
    pub(super) chat_agent: Option<String>,
    pub(super) chat_timeout: Option<String>,
    pub(super) wake_enabled: Option<String>,
    pub(super) wake_backend: Option<String>,
    pub(super) wake_model: Option<String>,
    pub(super) wake_threshold: Option<String>,
    pub(super) hotkey_enabled: Option<String>,
    pub(super) dictation_cleanup: Option<String>,
    pub(super) dictation_backtrack: Option<String>,
    pub(super) recording_timeout: Option<String>,
    pub(super) delivery_timeout: Option<String>,
    pub(super) overall_timeout: Option<String>,
    pub(super) unattended: Option<String>,
}

impl Environment {
    pub(crate) fn capture() -> Result<Self, ConfigError> {
        Ok(Self {
            home: home_directory()?,
            capture_backend: environment(CAPTURE_BACKEND_ENV)?,
            capture_file: environment(CAPTURE_FILE_ENV)?,
            capture_pace: environment(CAPTURE_PACE_ENV)?,
            transcription_backend: environment(BACKEND_ENV)?,
            transcription_model: environment(MODEL_ENV)?,
            transcription_device: environment(DEVICE_ENV)?,
            transcription_interpreter: environment(INTERPRETER_ENV)?,
            listen_backend: environment(LISTEN_BACKEND_ENV)?,
            listen_model: environment(LISTEN_MODEL_ENV)?,
            listen_device: environment(LISTEN_DEVICE_ENV)?,
            listen_transcripts_dir: environment(LISTEN_TRANSCRIPTS_DIR_ENV)?,
            speaker_model: environment(SPEAKER_MODEL_ENV)?,
            segmentation_model: environment(SEGMENTATION_MODEL_ENV)?,
            speakers_dir: environment(SPEAKERS_DIR_ENV)?,
            memory_enabled: environment(MEMORY_ENABLED_ENV)?,
            memory_extract_model: environment(MEMORY_EXTRACT_MODEL_ENV)?,
            memory_max_facts: environment(MEMORY_MAX_FACTS_ENV)?,
            memory_dir: environment(MEMORY_DIR_ENV)?,
            tts_backend: environment(TTS_BACKEND_ENV)?,
            tts_model: environment(TTS_MODEL_ENV)?,
            tts_voices: environment(TTS_VOICES_ENV)?,
            tts_tokenizer: environment(TTS_TOKENIZER_ENV)?,
            tts_voice: environment(TTS_VOICE_ENV)?,
            tts_rate: environment(TTS_RATE_ENV)?,
            tts_fallback: environment(TTS_FALLBACK_ENV)?,
            chat_agent: environment(CHAT_AGENT_ENV)?,
            chat_timeout: environment(CHAT_TIMEOUT_ENV)?,
            wake_enabled: environment(WAKE_ENABLED_ENV)?,
            wake_backend: environment(WAKE_BACKEND_ENV)?,
            wake_model: environment(WAKE_MODEL_ENV)?,
            wake_threshold: environment(WAKE_THRESHOLD_ENV)?,
            hotkey_enabled: environment(HOTKEY_ENABLED_ENV)?,
            dictation_cleanup: environment(DICTATION_CLEANUP_ENV)?,
            dictation_backtrack: environment(DICTATION_BACKTRACK_ENV)?,
            recording_timeout: environment(RECORDING_TIMEOUT_ENV)?,
            delivery_timeout: environment(DELIVERY_TIMEOUT_ENV)?,
            overall_timeout: environment(OVERALL_TIMEOUT_ENV)?,
            unattended: environment(UNATTENDED_ENV)?,
        })
    }

    /// Nothing set but the home, for tests that resolve from a file alone.
    #[cfg(test)]
    pub(crate) fn in_home(home: PathBuf) -> Self {
        Self {
            home,
            capture_backend: None,
            capture_file: None,
            capture_pace: None,
            transcription_backend: None,
            transcription_model: None,
            transcription_device: None,
            transcription_interpreter: None,
            listen_backend: None,
            listen_model: None,
            listen_device: None,
            listen_transcripts_dir: None,
            speaker_model: None,
            segmentation_model: None,
            speakers_dir: None,
            memory_enabled: None,
            memory_extract_model: None,
            memory_max_facts: None,
            memory_dir: None,
            tts_backend: None,
            tts_model: None,
            tts_voices: None,
            tts_tokenizer: None,
            tts_voice: None,
            tts_rate: None,
            tts_fallback: None,
            chat_agent: None,
            chat_timeout: None,
            wake_enabled: None,
            wake_backend: None,
            wake_model: None,
            wake_threshold: None,
            hotkey_enabled: None,
            dictation_cleanup: None,
            dictation_backtrack: None,
            recording_timeout: None,
            delivery_timeout: None,
            overall_timeout: None,
            unattended: None,
        }
    }

    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    /// Raw, because what counts as unattended is the daemon's call.
    pub(crate) fn unattended(&self) -> Option<&str> {
        self.unattended.as_deref()
    }
}

pub(super) fn load(environment: &Environment) -> Result<DictationConfig, ConfigError> {
    let file_config = file::read(&environment.home.join(DEFAULT_CONFIG_RELATIVE_PATH))?;

    dictation(&file_config, environment)
}

pub(super) fn load_daemon(
    environment: &Environment,
    wake_word: WakeWordOverride,
) -> Result<DaemonConfig, ConfigError> {
    let file_config = file::read(&environment.home.join(DEFAULT_CONFIG_RELATIVE_PATH))?;
    let dictation = dictation(&file_config, environment)?;
    let wake = wake(&file_config, environment, &dictation.capture, wake_word)?;
    let hotkey = hotkey(&file_config, environment)?;
    let tts = tts(&file_config, environment)?;
    Ok(DaemonConfig {
        dictation,
        wake,
        hotkey,
        relay: relay(&file_config),
        tts,
    })
}

/// Relay clients are unauthenticated and can deliver text to the focused app,
/// so enabling the LAN listener requires explicit configuration.
const DEFAULT_RELAY_ENABLED: bool = false;
const DEFAULT_RELAY_PORT: u16 = 9850;

pub(super) fn relay(file: &FileConfig) -> RelayConfig {
    if file.relay.enabled.unwrap_or(DEFAULT_RELAY_ENABLED) {
        RelayConfig::Enabled {
            port: file.relay.port.unwrap_or(DEFAULT_RELAY_PORT),
        }
    } else {
        RelayConfig::Disabled
    }
}

/// The hotkey listener costs nothing while its permission is missing, so it
/// defaults on where a listener exists at all.
const DEFAULT_HOTKEY_ENABLED: bool = cfg!(target_os = "macos");

pub(super) fn hotkey(
    file: &FileConfig,
    environment: &Environment,
) -> Result<HotkeyConfig, ConfigError> {
    let enabled = env_bool(
        environment.hotkey_enabled.as_deref(),
        HOTKEY_ENABLED_ENV,
        file.hotkey.enabled,
        DEFAULT_HOTKEY_ENABLED,
    )?;
    Ok(if enabled {
        HotkeyConfig::Enabled
    } else {
        HotkeyConfig::Disabled
    })
}

pub(super) fn load_listen(environment: &Environment) -> Result<ListenConfig, ConfigError> {
    let file_config = file::read(&environment.home.join(DEFAULT_CONFIG_RELATIVE_PATH))?;
    listen(&file_config, environment)
}

pub(super) fn listen(
    file: &FileConfig,
    environment: &Environment,
) -> Result<ListenConfig, ConfigError> {
    let backend = optional_string_value(
        environment.listen_backend.as_deref(),
        file.listen.backend.as_deref(),
    );
    // Without its own backend, listen inherits the resolved transcription
    // section — when that engine can stream.
    let transcription = match backend {
        Some(backend) => TranscriptionSettings {
            backend,
            model: optional_string_value(
                environment.listen_model.as_deref(),
                file.listen.model.as_deref(),
            ),
            device: optional_string_value(
                environment.listen_device.as_deref(),
                file.listen.device.as_deref(),
            ),
            interpreter: None,
        }
        .resolve(&environment.home)?,
        None => transcription(file, environment)?,
    };
    let transcription = match transcription {
        TranscriptionConfig::Parakeet {
            model_directory,
            device,
        } => ListenTranscription::Parakeet {
            model_directory,
            device,
        },
        TranscriptionConfig::Moonshine { model_directory } => {
            ListenTranscription::Moonshine { model_directory }
        }
        TranscriptionConfig::Nemotron { model_directory } => {
            ListenTranscription::Nemotron { model_directory }
        }
        other => {
            return Err(ConfigError::UnsupportedValue {
                field: "listen.backend",
                value: other.backend_name().to_owned(),
                supported: "parakeet, moonshine, and nemotron",
            });
        }
    };
    let transcripts_dir = optional_string_value(
        environment.listen_transcripts_dir.as_deref(),
        file.listen.transcripts_dir.as_deref(),
    )
    .unwrap_or_else(|| DEFAULT_LISTEN_TRANSCRIPTS_DIR.to_owned());
    Ok(ListenConfig {
        transcription,
        transcripts_dir: expand_home(
            &non_empty(transcripts_dir, "listen.transcripts_dir")?,
            &environment.home,
        )?,
    })
}

pub(super) fn load_speaker(environment: &Environment) -> Result<SpeakerConfig, ConfigError> {
    let file_config = file::read(&environment.home.join(DEFAULT_CONFIG_RELATIVE_PATH))?;
    speaker(&file_config, environment)
}

pub(super) fn speaker(
    file: &FileConfig,
    environment: &Environment,
) -> Result<SpeakerConfig, ConfigError> {
    let model = string_value(
        environment.speaker_model.as_deref(),
        file.speaker.model.as_deref(),
        DEFAULT_SPEAKER_MODEL,
    );
    let segmentation_model = string_value(
        environment.segmentation_model.as_deref(),
        file.speaker.segmentation_model.as_deref(),
        DEFAULT_SEGMENTATION_MODEL,
    );
    let speakers_dir = string_value(
        environment.speakers_dir.as_deref(),
        file.speaker.speakers_dir.as_deref(),
        DEFAULT_SPEAKERS_DIR,
    );
    Ok(SpeakerConfig {
        model: expand_home(&non_empty(model, "speaker.model")?, &environment.home)?,
        segmentation_model: expand_home(
            &non_empty(segmentation_model, "speaker.segmentation_model")?,
            &environment.home,
        )?,
        speakers_dir: expand_home(
            &non_empty(speakers_dir, "speaker.speakers_dir")?,
            &environment.home,
        )?,
    })
}

pub(super) fn load_memory(environment: &Environment) -> Result<MemoryConfig, ConfigError> {
    let file_config = file::read(&environment.home.join(DEFAULT_CONFIG_RELATIVE_PATH))?;
    memory(&file_config, environment)
}

pub(crate) fn memory_directory(environment: &Environment) -> Result<PathBuf, ConfigError> {
    let file_config = file::read(&environment.home.join(DEFAULT_CONFIG_RELATIVE_PATH))?;
    resolve_memory_directory(&file_config, environment)
}

fn resolve_memory_directory(
    file: &FileConfig,
    environment: &Environment,
) -> Result<PathBuf, ConfigError> {
    let dir = string_value(
        environment.memory_dir.as_deref(),
        file.memory.dir.as_deref(),
        DEFAULT_MEMORY_DIR,
    );
    expand_home(&non_empty(dir, "memory.dir")?, &environment.home)
}

pub(super) fn memory(
    file: &FileConfig,
    environment: &Environment,
) -> Result<MemoryConfig, ConfigError> {
    let enabled = env_bool(
        environment.memory_enabled.as_deref(),
        MEMORY_ENABLED_ENV,
        file.memory.enabled,
        false,
    )?;
    let extract_model = string_value(
        environment.memory_extract_model.as_deref(),
        file.memory.extract_model.as_deref(),
        DEFAULT_MEMORY_EXTRACT_MODEL,
    );
    let max_facts_per_session = match environment.memory_max_facts.as_deref() {
        Some(value) => value
            .parse()
            .map_err(|_error| ConfigError::InvalidEnvironmentNumber {
                variable: MEMORY_MAX_FACTS_ENV,
            })?,
        None => file
            .memory
            .max_facts_per_session
            .unwrap_or(DEFAULT_MEMORY_MAX_FACTS),
    };
    Ok(MemoryConfig {
        enabled,
        extract_model: non_empty(extract_model, "memory.extract_model")?,
        max_facts_per_session,
        dir: resolve_memory_directory(file, environment)?,
    })
}

pub(super) fn load_chat(environment: &Environment) -> Result<ChatConfig, ConfigError> {
    let file_config = file::read(&environment.home.join(DEFAULT_CONFIG_RELATIVE_PATH))?;
    chat(&file_config, environment)
}

pub(super) fn chat(
    file: &FileConfig,
    environment: &Environment,
) -> Result<ChatConfig, ConfigError> {
    let agent = string_value(
        environment.chat_agent.as_deref(),
        file.chat.agent.as_deref(),
        DEFAULT_CHAT_AGENT,
    );
    let agent = match agent.as_str() {
        chat_agent::CLAUDE_CODE => ChatAgent::ClaudeCode,
        chat_agent::CODEX => ChatAgent::Codex,
        _ => {
            return Err(ConfigError::UnsupportedValue {
                field: "chat.agent",
                value: agent,
                supported: "claude-code and codex",
            });
        }
    };
    let workdir = file
        .chat
        .workdir
        .clone()
        .map(|workdir| non_empty(workdir, "chat.workdir"))
        .transpose()?
        .as_deref()
        .map(|workdir| expand_home(workdir, &environment.home))
        .transpose()?;
    let project_roots = file
        .chat
        .project_roots
        .iter()
        .map(|root| expand_home(root, &environment.home))
        .collect::<Result<_, _>>()?;
    let timeout = timeout(
        environment.chat_timeout.as_deref(),
        CHAT_TIMEOUT_ENV,
        file.chat.timeout,
        "chat.timeout",
        DEFAULT_CHAT_TIMEOUT,
    )?;
    if timeout > crate::chat::MAX_TIMEOUT {
        return Err(ConfigError::ChatTimeoutTooLong {
            actual: timeout,
            maximum: crate::chat::MAX_TIMEOUT,
        });
    }
    Ok(ChatConfig {
        agent,
        workdir,
        project_roots,
        timeout,
        delivery_timeout: timeouts(file, environment)?.delivery,
    })
}

pub(super) fn load_tts(environment: &Environment) -> Result<TtsConfig, ConfigError> {
    let file_config = file::read(&environment.home.join(DEFAULT_CONFIG_RELATIVE_PATH))?;
    tts(&file_config, environment)
}

pub(super) fn tts(file: &FileConfig, environment: &Environment) -> Result<TtsConfig, ConfigError> {
    let backend = string_value(
        environment.tts_backend.as_deref(),
        file.tts.backend.as_deref(),
        DEFAULT_TTS_BACKEND,
    );
    let model = optional_string_value(environment.tts_model.as_deref(), file.tts.model.as_deref());
    let voices = optional_string_value(
        environment.tts_voices.as_deref(),
        file.tts.voices.as_deref(),
    );
    let tokenizer = optional_string_value(
        environment.tts_tokenizer.as_deref(),
        file.tts.tokenizer.as_deref(),
    );
    let backend = match backend.as_str() {
        tts_backend::KOKORO => {
            reject_tts_setting(tokenizer.is_some(), "tts.tokenizer", tts_backend::KOKORO)?;
            TtsBackend::Kokoro {
                model: tts_path(model, DEFAULT_KOKORO_MODEL, "tts.model", &environment.home)?,
                voices: tts_path(
                    voices,
                    DEFAULT_KOKORO_VOICES,
                    "tts.voices",
                    &environment.home,
                )?,
            }
        }
        tts_backend::QWEN => {
            reject_tts_setting(voices.is_some(), "tts.voices", tts_backend::QWEN)?;
            TtsBackend::Qwen {
                model_dir: tts_path(
                    model,
                    DEFAULT_QWEN_MODEL_DIR,
                    "tts.model",
                    &environment.home,
                )?,
                tokenizer_dir: tts_path(
                    tokenizer,
                    DEFAULT_QWEN_TOKENIZER_DIR,
                    "tts.tokenizer",
                    &environment.home,
                )?,
            }
        }
        tts_backend::SYSTEM => {
            reject_tts_setting(model.is_some(), "tts.model", tts_backend::SYSTEM)?;
            reject_tts_setting(voices.is_some(), "tts.voices", tts_backend::SYSTEM)?;
            reject_tts_setting(tokenizer.is_some(), "tts.tokenizer", tts_backend::SYSTEM)?;
            TtsBackend::System
        }
        _ => {
            return Err(ConfigError::UnsupportedValue {
                field: "tts.backend",
                value: backend,
                supported: "kokoro, qwen, and system",
            });
        }
    };
    let fallback = tts_fallback(
        environment.tts_fallback.as_deref(),
        file.tts.fallback.as_deref(),
        &backend,
    )?;
    let voice = optional_string_value(environment.tts_voice.as_deref(), file.tts.voice.as_deref())
        .map(|name| {
            Voice::new(name).map_err(|source| ConfigError::InvalidVoice {
                field: "tts.voice",
                source,
            })
        })
        .transpose()?;
    let rate = speaking_rate(environment.tts_rate.as_deref(), file.tts.rate)?;
    Ok(TtsConfig {
        backend,
        voice,
        rate,
        fallback,
    })
}

fn tts_path(
    value: Option<String>,
    default: &'static str,
    field: &'static str,
    home: &Path,
) -> Result<PathBuf, ConfigError> {
    let value = value.unwrap_or_else(|| default.to_owned());
    let value = non_empty(value, field)?;
    expand_home(&value, home)
}

fn reject_tts_setting(
    present: bool,
    field: &'static str,
    backend: &'static str,
) -> Result<(), ConfigError> {
    if present {
        return Err(ConfigError::NotApplicableToBackend { field, backend });
    }
    Ok(())
}

fn tts_fallback(
    environment_value: Option<&str>,
    file_value: Option<&str>,
    backend: &TtsBackend,
) -> Result<TtsFallback, ConfigError> {
    let Some(value) = environment_value.or(file_value) else {
        return Ok(TtsFallback::None);
    };
    if matches!(backend, TtsBackend::System) {
        return Err(ConfigError::NotApplicableToBackend {
            field: "tts.fallback",
            backend: tts_backend::SYSTEM,
        });
    }
    match value {
        "none" => Ok(TtsFallback::None),
        "system" => Ok(TtsFallback::System),
        _ => Err(ConfigError::UnsupportedValue {
            field: "tts.fallback",
            value: value.to_owned(),
            supported: "none and system",
        }),
    }
}

fn speaking_rate(
    environment_value: Option<&str>,
    file_value: Option<f64>,
) -> Result<SpeakingRate, ConfigError> {
    let rate = match environment_value {
        Some(value) => {
            Some(
                value
                    .parse()
                    .map_err(|_error| ConfigError::InvalidEnvironmentNumber {
                        variable: TTS_RATE_ENV,
                    })?,
            )
        }
        None => file_value,
    };
    let Some(rate) = rate else {
        return Ok(SpeakingRate::standard());
    };
    SpeakingRate::new(rate as f32).map_err(|source| ConfigError::InvalidSpeakingRate {
        field: "tts.rate",
        source,
    })
}

fn dictation(
    file_config: &FileConfig,
    environment: &Environment,
) -> Result<DictationConfig, ConfigError> {
    Ok(DictationConfig {
        capture: capture(file_config, environment)?,
        transcription: transcription(file_config, environment)?,
        dictation_timeouts: timeouts(file_config, environment)?,
        cleanup: cleanup(file_config, environment)?,
        backtrack: backtrack(file_config, environment)?,
        cleanup_llm: cleanup_llm(file_config, environment)?,
    })
}

pub(super) fn cleanup(file: &FileConfig, environment: &Environment) -> Result<bool, ConfigError> {
    env_bool(
        environment.dictation_cleanup.as_deref(),
        DICTATION_CLEANUP_ENV,
        file.dictation_cleanup,
        false,
    )
}

pub(super) fn backtrack(file: &FileConfig, environment: &Environment) -> Result<bool, ConfigError> {
    env_bool(
        environment.dictation_backtrack.as_deref(),
        DICTATION_BACKTRACK_ENV,
        file.dictation_backtrack,
        false,
    )
}

pub(super) fn cleanup_llm(
    file: &FileConfig,
    environment: &Environment,
) -> Result<Option<super::LlmCleanupConfig>, ConfigError> {
    let Some(config) = &file.dictation_cleanup_llm else {
        return Ok(None);
    };
    if !cleanup(file, environment)? {
        return Err(ConfigError::CleanupLlmRequiresCleanup);
    }
    crate::dictation::cleanup::validate_config(&config.endpoint, &config.model)
        .map_err(ConfigError::CleanupLlm)?;
    Ok(Some(config.clone()))
}

/// Where a backend's model lives unless configured otherwise; None for a
/// backend that needs no file on disk.
pub(crate) fn default_model_path(backend_name: &str, home: &Path) -> Option<PathBuf> {
    let value = match backend_name {
        backend::WHISPER => DEFAULT_WHISPER_MODEL,
        backend::WHISPER_CANDLE => DEFAULT_WHISPER_CANDLE_MODEL,
        backend::PARAKEET => DEFAULT_PARAKEET_MODEL,
        backend::MOONSHINE => DEFAULT_MOONSHINE_MODEL,
        backend::NEMOTRON => DEFAULT_NEMOTRON_MODEL,
        tts_backend::KOKORO => DEFAULT_KOKORO_MODEL,
        tts_backend::QWEN => DEFAULT_QWEN_MODEL_DIR,
        _ => return None,
    };
    expand_home(value, home).ok()
}

/// The file `load` reads. It need not exist; a missing one leaves
/// every setting at its default.
pub(crate) fn default_path() -> Result<PathBuf, ConfigError> {
    Ok(home_directory()?.join(DEFAULT_CONFIG_RELATIVE_PATH))
}

pub(super) fn capture(
    file: &FileConfig,
    environment: &Environment,
) -> Result<CaptureConfig, ConfigError> {
    let backend = string_value(
        environment.capture_backend.as_deref(),
        file.capture.backend.as_deref(),
        DEFAULT_CAPTURE_BACKEND,
    );
    let path = optional_string_value(
        environment.capture_file.as_deref(),
        file.capture.path.as_deref(),
    );
    let pace = optional_string_value(
        environment.capture_pace.as_deref(),
        file.capture.pace.as_deref(),
    );
    resolve_capture(backend, path, pace, &environment.home)
}

fn resolve_capture(
    backend: String,
    path: Option<String>,
    pace: Option<String>,
    home: &Path,
) -> Result<CaptureConfig, ConfigError> {
    if backend == DEFAULT_CAPTURE_BACKEND {
        if pace.is_some() {
            return Err(ConfigError::NotApplicableToBackend {
                field: "capture.pace",
                backend: WAV_FILE_CAPTURE_BACKEND,
            });
        }
        return Ok(CaptureConfig::Microphone);
    }
    if backend != WAV_FILE_CAPTURE_BACKEND {
        return Err(ConfigError::UnsupportedValue {
            field: "capture.backend",
            value: backend,
            supported: "microphone and wav-file",
        });
    }

    let path = path.ok_or(ConfigError::MissingValue {
        field: "capture.path",
        backend: WAV_FILE_CAPTURE_BACKEND,
    })?;
    let path = non_empty(path, "capture.path")?;
    let path = expand_home(&path, home)?;
    Ok(CaptureConfig::WavFile {
        path,
        pace: replay_pace(pace)?,
    })
}

fn replay_pace(pace: Option<String>) -> Result<ReplayPace, ConfigError> {
    match pace.as_deref() {
        None | Some("immediate") => Ok(ReplayPace::Immediate),
        Some("realtime") => Ok(ReplayPace::Realtime),
        Some(_) => Err(ConfigError::UnsupportedValue {
            field: "capture.pace",
            value: pace.unwrap_or_default(),
            supported: "immediate and realtime",
        }),
    }
}

pub(super) fn wake(
    file: &FileConfig,
    environment: &Environment,
    capture: &CaptureConfig,
    wake_word: WakeWordOverride,
) -> Result<WakeConfig, ConfigError> {
    let enabled = wake_enabled(
        wake_word,
        environment.wake_enabled.as_deref(),
        file.wake.enabled,
    )?;
    if !enabled {
        return Ok(WakeConfig::Disabled);
    }

    let backend = parse_wake_backend(&string_value(
        environment.wake_backend.as_deref(),
        file.wake.backend.as_deref(),
        DEFAULT_WAKE_BACKEND,
    ))?;
    let model = optional_string_value(
        environment.wake_model.as_deref(),
        file.wake.model.as_deref(),
    );
    let threshold = wake_threshold(environment.wake_threshold.as_deref(), file.wake.threshold)?;

    if !matches!(capture, CaptureConfig::Microphone) {
        return Err(ConfigError::WakeRequiresMicrophone);
    }

    let model = match (model, backend) {
        (Some(model), _) => model,
        (None, WakeBackend::OpenWakeWord) => DEFAULT_WAKE_MODEL.to_owned(),
        // No aria-trained livekit head ships with Aria, and the openWakeWord
        // default would load into the wrong backend.
        (None, WakeBackend::LiveKit) => {
            return Err(ConfigError::MissingValue {
                field: "wake.model",
                backend: wake_backend::LIVEKIT,
            });
        }
    };
    let model = non_empty(model, "wake.model")?;
    let classifier = expand_home(&model, &environment.home)?;
    Ok(WakeConfig::Enabled {
        backend,
        classifier,
        threshold: threshold.unwrap_or(DEFAULT_WAKE_THRESHOLD),
    })
}

fn parse_wake_backend(value: &str) -> Result<WakeBackend, ConfigError> {
    match value {
        wake_backend::OPEN_WAKE_WORD => Ok(WakeBackend::OpenWakeWord),
        wake_backend::LIVEKIT => Ok(WakeBackend::LiveKit),
        _ => Err(ConfigError::UnsupportedValue {
            field: "wake.backend",
            value: value.to_owned(),
            supported: "openwakeword and livekit",
        }),
    }
}

/// Three sources, most explicit first.
fn wake_enabled(
    command_value: WakeWordOverride,
    environment_value: Option<&str>,
    file_value: Option<bool>,
) -> Result<bool, ConfigError> {
    if matches!(command_value, WakeWordOverride::Enable) {
        return Ok(true);
    }
    env_bool(environment_value, WAKE_ENABLED_ENV, file_value, false)
}

/// An on/off switch: the variable when set, else the file, else the default.
fn env_bool(
    environment_value: Option<&str>,
    variable: &'static str,
    file_value: Option<bool>,
    default: bool,
) -> Result<bool, ConfigError> {
    match environment_value {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(ConfigError::UnsupportedValue {
            field: variable,
            value: value.to_owned(),
            supported: "true and false",
        }),
        None => Ok(file_value.unwrap_or(default)),
    }
}

fn wake_threshold(
    environment_value: Option<&str>,
    file_value: Option<f32>,
) -> Result<Option<f32>, ConfigError> {
    let threshold = match environment_value {
        Some(value) => {
            Some(
                value
                    .parse()
                    .map_err(|_error| ConfigError::InvalidEnvironmentNumber {
                        variable: WAKE_THRESHOLD_ENV,
                    })?,
            )
        }
        None => file_value,
    };
    if let Some(value) = threshold
        && (!value.is_finite() || value <= 0.0 || value > 1.0)
    {
        return Err(ConfigError::InvalidThreshold {
            field: "wake.threshold",
            value,
        });
    }
    Ok(threshold)
}

pub(super) fn transcription(
    file: &FileConfig,
    environment: &Environment,
) -> Result<TranscriptionConfig, ConfigError> {
    TranscriptionSettings::read(file, environment).resolve(&environment.home)
}

pub(super) fn transcription_defaults(
    backend: &str,
    home: &Path,
) -> Result<TranscriptionConfig, ConfigError> {
    TranscriptionSettings {
        backend: backend.to_owned(),
        model: None,
        device: None,
        interpreter: None,
    }
    .resolve(home)
}

/// The transcription settings as written, before a backend claims them. Only
/// `backend` applies to every backend; the rest belong to some of them.
struct TranscriptionSettings {
    backend: String,
    model: Option<String>,
    device: Option<String>,
    interpreter: Option<String>,
}

impl TranscriptionSettings {
    fn read(file: &FileConfig, environment: &Environment) -> Self {
        Self {
            backend: string_value(
                environment.transcription_backend.as_deref(),
                file.transcription.backend.as_deref(),
                DEFAULT_BACKEND,
            ),
            model: optional_string_value(
                environment.transcription_model.as_deref(),
                file.transcription.model.as_deref(),
            ),
            device: optional_string_value(
                environment.transcription_device.as_deref(),
                file.transcription.device.as_deref(),
            ),
            interpreter: optional_string_value(
                environment.transcription_interpreter.as_deref(),
                file.transcription.interpreter.as_deref(),
            ),
        }
    }

    fn resolve(self, home: &Path) -> Result<TranscriptionConfig, ConfigError> {
        self.reject_settings_the_backend_does_not_have()?;

        match self.backend.as_str() {
            backend::WHISPER => Ok(TranscriptionConfig::Whisper {
                model: model_path(self.model, DEFAULT_WHISPER_MODEL, home)?,
            }),
            backend::WHISPER_CANDLE => Ok(TranscriptionConfig::WhisperCandle {
                model_directory: model_path(self.model, DEFAULT_WHISPER_CANDLE_MODEL, home)?,
            }),
            backend::PARAKEET => Ok(TranscriptionConfig::Parakeet {
                model_directory: model_path(self.model, DEFAULT_PARAKEET_MODEL, home)?,
                device: transcription_device(self.device)?,
            }),
            backend::MOONSHINE => Ok(TranscriptionConfig::Moonshine {
                model_directory: model_path(self.model, DEFAULT_MOONSHINE_MODEL, home)?,
            }),
            backend::NEMOTRON => Ok(TranscriptionConfig::Nemotron {
                model_directory: model_path(self.model, DEFAULT_NEMOTRON_MODEL, home)?,
            }),
            backend::PARAKEET_MLX => {
                let interpreter = self.interpreter.ok_or(ConfigError::MissingValue {
                    field: "transcription.interpreter",
                    backend: backend::PARAKEET_MLX,
                })?;
                let interpreter = non_empty(interpreter, "transcription.interpreter")?;
                let model = self
                    .model
                    .unwrap_or_else(|| DEFAULT_PARAKEET_MLX_MODEL.to_owned());
                Ok(TranscriptionConfig::ParakeetMlx {
                    model: non_empty(model, "transcription.model")?,
                    interpreter: expand_home(&interpreter, home)?,
                })
            }
            _ => Err(ConfigError::UnsupportedValue {
                field: "transcription.backend",
                value: self.backend,
                supported: "whisper, whisper-candle, parakeet, moonshine, nemotron, and parakeet-mlx",
            }),
        }
    }

    fn reject_settings_the_backend_does_not_have(&self) -> Result<(), ConfigError> {
        if self.interpreter.is_some() && self.backend != backend::PARAKEET_MLX {
            return Err(ConfigError::NotApplicableToBackend {
                field: "transcription.interpreter",
                backend: backend::PARAKEET_MLX,
            });
        }
        if self.device.is_some() && self.backend != backend::PARAKEET {
            return Err(ConfigError::NotApplicableToBackend {
                field: "transcription.device",
                backend: backend::PARAKEET,
            });
        }
        Ok(())
    }
}

fn transcription_device(device: Option<String>) -> Result<TranscriptionDevice, ConfigError> {
    match device.as_deref() {
        None | Some("cpu") => Ok(TranscriptionDevice::Cpu),
        Some("gpu") => Ok(TranscriptionDevice::Gpu),
        Some(_) => Err(ConfigError::UnsupportedValue {
            field: "transcription.device",
            value: device.unwrap_or_default(),
            supported: "cpu and gpu",
        }),
    }
}

fn model_path(
    model: Option<String>,
    default: &'static str,
    home: &Path,
) -> Result<PathBuf, ConfigError> {
    let model = model.unwrap_or_else(|| default.to_owned());
    let model = non_empty(model, "transcription.model")?;
    expand_home(&model, home)
}

pub(super) fn timeouts(
    file: &FileConfig,
    environment: &Environment,
) -> Result<DictationTimeouts, ConfigError> {
    let recording = timeout(
        environment.recording_timeout.as_deref(),
        RECORDING_TIMEOUT_ENV,
        file.dictation_timeouts.recording,
        "dictation_timeouts.recording",
        DEFAULT_RECORDING_TIMEOUT,
    )?;
    let delivery = timeout(
        environment.delivery_timeout.as_deref(),
        DELIVERY_TIMEOUT_ENV,
        file.dictation_timeouts.delivery,
        "dictation_timeouts.delivery",
        DEFAULT_DELIVERY_TIMEOUT,
    )?;
    let overall = timeout(
        environment.overall_timeout.as_deref(),
        OVERALL_TIMEOUT_ENV,
        file.dictation_timeouts.overall,
        "dictation_timeouts.overall",
        DEFAULT_OVERALL_TIMEOUT,
    )?;

    validate_timeouts(recording, overall)?;
    Ok(DictationTimeouts {
        recording,
        delivery,
        overall,
    })
}

fn validate_timeouts(recording: Duration, overall: Duration) -> Result<(), ConfigError> {
    if overall > MAX_DICTATION_TIMEOUT {
        return Err(ConfigError::OverallTimeoutTooLong {
            actual: overall,
            maximum: MAX_DICTATION_TIMEOUT,
        });
    }
    if recording >= overall {
        return Err(ConfigError::RecordingTimeoutTooLong { recording, overall });
    }
    Ok(())
}

fn string_value(
    environment_value: Option<&str>,
    file_value: Option<&str>,
    default: &'static str,
) -> String {
    optional_string_value(environment_value, file_value).unwrap_or_else(|| default.to_owned())
}

fn optional_string_value(
    environment_value: Option<&str>,
    file_value: Option<&str>,
) -> Option<String> {
    environment_value.or(file_value).map(str::to_owned)
}

fn non_empty(value: String, field: &'static str) -> Result<String, ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::EmptyValue { field });
    }
    Ok(value)
}

fn timeout(
    environment_value: Option<&str>,
    variable: &'static str,
    file_value: Option<f64>,
    file_name: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    if let Some(value) = environment_value {
        let seconds = value
            .parse()
            .map_err(|_error| ConfigError::InvalidEnvironmentNumber { variable })?;
        return duration(seconds, variable);
    }

    match file_value {
        Some(seconds) => duration(seconds, file_name),
        None => Ok(default),
    }
}

fn environment(variable: &'static str) -> Result<Option<String>, ConfigError> {
    let Some(value) = env::var_os(variable) else {
        return Ok(None);
    };
    value
        .into_string()
        .map(Some)
        .map_err(|_value| ConfigError::InvalidEnvironmentEncoding { variable })
}

fn duration(seconds: f64, field: &'static str) -> Result<Duration, ConfigError> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(ConfigError::InvalidDuration { field, seconds });
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_error| ConfigError::InvalidDuration { field, seconds })
}

fn home_directory() -> Result<PathBuf, ConfigError> {
    user_home::resolve().map_err(ConfigError::Home)
}

fn expand_home(value: &str, home: &Path) -> Result<PathBuf, ConfigError> {
    if value == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(relative) = value.strip_prefix("~/") {
        return Ok(home.join(relative));
    }
    if value.starts_with('~') {
        return Err(ConfigError::UnsupportedHomeSyntax(value.to_owned()));
    }
    Ok(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_environment() -> Environment {
        Environment::in_home(PathBuf::from("/home/user"))
    }

    #[test]
    fn lexicon_ignores_extraction_settings() {
        let home = tempfile::tempdir().expect("home");
        let dir = home.path().join(".aria/memory");
        std::fs::create_dir_all(&dir).expect("memory directory");
        std::fs::write(dir.join("lexicon.txt"), "db2\n").expect("lexicon");
        std::fs::write(
            home.path().join(DEFAULT_CONFIG_RELATIVE_PATH),
            "memory:\n  enabled: false\n  extract_model: ''\n",
        )
        .expect("config");
        let mut environment = Environment::in_home(home.path().to_path_buf());
        environment.memory_max_facts = Some("bad".to_owned());

        assert!(MemoryConfig::load(&environment).is_err());
        let lexicon = crate::memory::Lexicon::configured(&environment)
            .expect("correction needs only the memory directory");
        assert_eq!(lexicon.correct("d b two").as_deref(), Some("db2"));
    }

    #[test]
    fn cleanup_is_off_until_the_file_or_environment_turns_it_on() {
        let mut file = FileConfig::default();
        let mut environment = empty_environment();
        assert!(!cleanup(&file, &environment).expect("default"));

        file.dictation_cleanup = Some(true);
        assert!(cleanup(&file, &environment).expect("file"));

        environment.dictation_cleanup = Some("false".to_owned());
        assert!(!cleanup(&file, &environment).expect("environment wins"));
    }

    #[test]
    fn backtrack_is_independent_and_environment_overrides_the_file() {
        let mut environment = empty_environment();
        let mut file = FileConfig::default();
        let config = dictation(&file, &environment).unwrap();
        assert!(!config.backtrack);

        file = serde_saphyr::from_str("dictation_backtrack: true\n").unwrap();
        let config = dictation(&file, &environment).unwrap();
        assert!(config.backtrack);
        assert!(!config.cleanup);
        assert!(config.cleanup_llm.is_none());

        environment.dictation_backtrack = Some("false".to_owned());
        assert!(!dictation(&file, &environment).unwrap().backtrack);
        file.dictation_backtrack = Some(false);
        environment.dictation_backtrack = Some("true".to_owned());
        assert!(dictation(&file, &environment).unwrap().backtrack);

        for value in ["yes", "1", "TRUE", ""] {
            environment.dictation_backtrack = Some(value.to_owned());
            assert!(matches!(
                dictation(&file, &environment),
                Err(ConfigError::UnsupportedValue {
                    field: DICTATION_BACKTRACK_ENV,
                    ..
                })
            ));
        }
        assert!(serde_saphyr::from_str::<FileConfig>("dictation_backtrack: invalid\n").is_err());
        assert!(serde_saphyr::from_str::<FileConfig>("dictation_backtrak: true\n").is_err());
    }

    #[test]
    fn llm_cleanup_requires_effective_cleanup_and_preserves_settings() {
        let mut environment = empty_environment();
        assert!(
            dictation(&FileConfig::default(), &environment)
                .unwrap()
                .cleanup_llm
                .is_none()
        );
        let file: FileConfig = serde_saphyr::from_str(
            "dictation_cleanup: true\ndictation_cleanup_llm:\n  endpoint: http://127.0.0.1:8080/v1/chat/completions\n  model: local-model\n",
        ).unwrap();
        let config = dictation(&file, &environment).unwrap();
        assert!(config.cleanup);
        assert_eq!(config.cleanup_llm, file.dictation_cleanup_llm);
        assert!(!config.backtrack);
        environment.dictation_backtrack = Some("true".to_owned());
        assert!(dictation(&file, &environment).unwrap().backtrack);

        environment.dictation_cleanup = Some("false".to_owned());
        assert!(matches!(
            dictation(&file, &environment),
            Err(ConfigError::CleanupLlmRequiresCleanup)
        ));
        environment.dictation_cleanup = Some("true".to_owned());
        let mut file = file;
        file.dictation_cleanup = Some(false);
        assert!(dictation(&file, &environment).is_ok());
    }

    #[test]
    fn llm_cleanup_rejects_incomplete_unknown_and_invalid_settings() {
        for section in [
            "{}",
            "{endpoint: http://127.0.0.1:8080/v1/chat/completions}",
            "{model: local-model}",
            "{endpoint: http://127.0.0.1:8080/v1/chat/completions, model: local-model, token: secret}",
        ] {
            assert!(
                serde_saphyr::from_str::<FileConfig>(&format!(
                    "dictation_cleanup_llm: {section}\n"
                ))
                .is_err()
            );
        }
        for (endpoint, model) in [
            ("https://example.com/v1/chat/completions", "local-model"),
            ("http://127.0.0.1:8080/v1/chat/completions", ""),
        ] {
            let file = FileConfig {
                dictation_cleanup: Some(true),
                dictation_cleanup_llm: Some(super::super::LlmCleanupConfig {
                    endpoint: endpoint.to_owned(),
                    model: model.to_owned(),
                }),
                ..FileConfig::default()
            };
            assert!(matches!(
                dictation(&file, &empty_environment()),
                Err(ConfigError::CleanupLlm(_))
            ));
        }
    }

    fn resolve(
        backend: &str,
        path: Option<&str>,
        pace: Option<&str>,
    ) -> Result<CaptureConfig, ConfigError> {
        resolve_capture(
            backend.to_owned(),
            path.map(str::to_owned),
            pace.map(str::to_owned),
            Path::new("/home/user"),
        )
    }

    #[test]
    fn capture_defaults_to_the_microphone() {
        let capture = resolve("microphone", None, None).expect("default must resolve");
        assert_eq!(capture, CaptureConfig::Microphone);
    }

    #[test]
    fn capture_resolves_a_wav_file_backend() {
        let capture =
            resolve("wav-file", Some("~/fixture.wav"), None).expect("wav-file must resolve");
        assert_eq!(
            capture,
            CaptureConfig::WavFile {
                path: PathBuf::from("/home/user/fixture.wav"),
                pace: ReplayPace::Immediate,
            }
        );
    }

    #[test]
    fn capture_resolves_a_paced_replay() {
        let capture = resolve("wav-file", Some("/fixture.wav"), Some("realtime"))
            .expect("a paced replay must resolve");
        assert!(matches!(
            capture,
            CaptureConfig::WavFile {
                pace: ReplayPace::Realtime,
                ..
            }
        ));
    }

    #[test]
    fn capture_rejects_an_unknown_pace() {
        let error = resolve("wav-file", Some("/fixture.wav"), Some("double"))
            .expect_err("an unknown pace must be rejected");
        assert!(matches!(
            error,
            ConfigError::UnsupportedValue {
                field: "capture.pace",
                ..
            }
        ));
    }

    #[test]
    fn the_microphone_has_no_pace() {
        let error = resolve("microphone", None, Some("realtime"))
            .expect_err("a paced microphone must be rejected");
        assert!(matches!(
            error,
            ConfigError::NotApplicableToBackend {
                field: "capture.pace",
                ..
            }
        ));
    }

    #[test]
    fn capture_rejects_an_unknown_backend() {
        let error =
            resolve("pulseaudio", None, None).expect_err("an unknown backend must be rejected");
        assert!(matches!(
            error,
            ConfigError::UnsupportedValue {
                field: "capture.backend",
                ..
            }
        ));
    }

    #[test]
    fn disabled_wake_ignores_its_dormant_settings() {
        let mut file = FileConfig::default();
        file.wake.enabled = Some(true);
        file.wake.model = Some("/missing/model.onnx".to_owned());
        file.wake.threshold = Some(0.75);
        let mut environment = empty_environment();
        environment.wake_enabled = Some("false".to_owned());
        environment.wake_threshold = Some("not-a-number".to_owned());

        let wake = wake(
            &file,
            &environment,
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect("the disable override should win");

        assert_eq!(wake, WakeConfig::Disabled);
    }

    #[test]
    fn a_wake_word_run_outranks_a_disabled_environment_and_file() {
        let mut file = FileConfig::default();
        file.wake.enabled = Some(false);
        let mut environment = empty_environment();
        environment.wake_enabled = Some("false".to_owned());

        let wake = wake(
            &file,
            &environment,
            &CaptureConfig::Microphone,
            WakeWordOverride::Enable,
        )
        .expect("the command should enable monitoring");

        assert!(matches!(wake, WakeConfig::Enabled { .. }));
    }

    fn enabled_wake(backend: Option<&str>) -> FileConfig {
        let mut file = FileConfig::default();
        file.wake.enabled = Some(true);
        file.wake.backend = backend.map(str::to_owned);
        file
    }

    #[test]
    fn wake_defaults_to_openwakeword_and_its_own_model() {
        let wake = wake(
            &enabled_wake(None),
            &empty_environment(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect("the default backend must resolve");

        assert_eq!(
            wake,
            WakeConfig::Enabled {
                backend: WakeBackend::OpenWakeWord,
                classifier: PathBuf::from("/home/user/models/wakeword/aria.onnx"),
                threshold: DEFAULT_WAKE_THRESHOLD,
            }
        );
    }

    #[test]
    fn wake_resolves_the_livekit_backend() {
        let mut file = enabled_wake(Some("livekit"));
        file.wake.model = Some("~/models/hey_livekit.onnx".to_owned());

        let wake = wake(
            &file,
            &empty_environment(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect("livekit with a model must resolve");

        assert_eq!(
            wake,
            WakeConfig::Enabled {
                backend: WakeBackend::LiveKit,
                classifier: PathBuf::from("/home/user/models/hey_livekit.onnx"),
                threshold: DEFAULT_WAKE_THRESHOLD,
            }
        );
    }

    #[test]
    fn livekit_without_a_model_is_refused() {
        let error = wake(
            &enabled_wake(Some("livekit")),
            &empty_environment(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect_err("livekit has no default model");

        assert!(matches!(
            error,
            ConfigError::MissingValue {
                field: "wake.model",
                backend: "livekit",
            }
        ));
    }

    #[test]
    fn the_wake_backend_environment_override_wins() {
        let mut file = enabled_wake(Some("openwakeword"));
        file.wake.model = Some("~/models/hey_livekit.onnx".to_owned());
        let mut environment = empty_environment();
        environment.wake_backend = Some("livekit".to_owned());

        let wake = wake(
            &file,
            &environment,
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect("the environment override should win");

        assert!(matches!(
            wake,
            WakeConfig::Enabled {
                backend: WakeBackend::LiveKit,
                ..
            }
        ));
    }

    #[test]
    fn wake_rejects_an_unknown_backend() {
        let error = wake(
            &enabled_wake(Some("porcupine")),
            &empty_environment(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect_err("an unknown backend must be rejected");

        assert!(matches!(
            error,
            ConfigError::UnsupportedValue {
                field: "wake.backend",
                ..
            }
        ));
    }

    /// Every setting unset but the backend, so each test names only what it
    /// is about.
    fn settings(backend: &str) -> TranscriptionSettings {
        TranscriptionSettings {
            backend: backend.to_owned(),
            model: None,
            device: None,
            interpreter: None,
        }
    }

    #[test]
    fn each_backend_falls_back_to_its_own_model() {
        let defaults = [
            (
                "whisper",
                TranscriptionConfig::Whisper {
                    model: PathBuf::from("/home/user/models/whisper/ggml-large-v3-turbo-q8_0.bin"),
                },
            ),
            (
                "parakeet",
                TranscriptionConfig::Parakeet {
                    model_directory: PathBuf::from(
                        "/home/user/models/parakeet/parakeet-tdt-0.6b-v3",
                    ),
                    device: TranscriptionDevice::Cpu,
                },
            ),
            (
                "moonshine",
                TranscriptionConfig::Moonshine {
                    model_directory: PathBuf::from("/home/user/models/moonshine/base"),
                },
            ),
            (
                "nemotron",
                TranscriptionConfig::Nemotron {
                    model_directory: PathBuf::from(
                        "/home/user/models/nemotron/nemotron-speech-streaming-en-0.6b",
                    ),
                },
            ),
        ];

        for (backend, expected) in defaults {
            let transcription = settings(backend)
                .resolve(Path::new("/home/user"))
                .expect("a default must resolve");
            assert_eq!(transcription, expected, "{backend}");
        }
    }

    #[test]
    fn retry_backend_keeps_matching_config_and_uses_other_engine_defaults() {
        let home = Path::new("/tmp/retry-config");
        let configured = TranscriptionConfig::Parakeet {
            model_directory: home.join("custom"),
            device: TranscriptionDevice::Cpu,
        };
        assert_eq!(
            configured.for_backend(backend::PARAKEET, home).unwrap(),
            configured
        );
        assert_eq!(
            configured.for_backend(backend::WHISPER, home).unwrap(),
            transcription_defaults(backend::WHISPER, home).unwrap()
        );
        assert!(configured.for_backend(backend::PARAKEET_MLX, home).is_err());
    }

    #[test]
    fn transcription_resolves_a_parakeet_model_directory() {
        let transcription = TranscriptionSettings {
            model: Some("~/models/tdt".to_owned()),
            ..settings("parakeet")
        }
        .resolve(Path::new("/home/user"))
        .expect("parakeet must resolve");
        assert_eq!(
            transcription,
            TranscriptionConfig::Parakeet {
                model_directory: PathBuf::from("/home/user/models/tdt"),
                device: TranscriptionDevice::Cpu,
            }
        );
    }

    #[test]
    fn transcription_rejects_an_unknown_backend() {
        let error = settings("vosk")
            .resolve(Path::new("/home/user"))
            .expect_err("an unknown backend must be rejected");
        assert!(matches!(
            error,
            ConfigError::UnsupportedValue {
                field: "transcription.backend",
                ..
            }
        ));
    }

    #[test]
    fn a_setting_the_backend_does_not_have_is_rejected() {
        let error = TranscriptionSettings {
            interpreter: Some("/usr/bin/python3".to_owned()),
            ..settings("whisper")
        }
        .resolve(Path::new("/home/user"))
        .expect_err("whisper has no interpreter");
        assert!(matches!(
            error,
            ConfigError::NotApplicableToBackend {
                field: "transcription.interpreter",
                ..
            }
        ));

        let error = TranscriptionSettings {
            device: Some("gpu".to_owned()),
            ..settings("whisper")
        }
        .resolve(Path::new("/home/user"))
        .expect_err("whisper has no device");
        assert!(matches!(
            error,
            ConfigError::NotApplicableToBackend {
                field: "transcription.device",
                ..
            }
        ));
    }

    #[test]
    fn chat_defaults_to_claude_code_on_the_path() {
        let chat = chat(&FileConfig::default(), &empty_environment()).expect("defaults resolve");

        assert_eq!(
            chat,
            ChatConfig {
                agent: ChatAgent::ClaudeCode,
                workdir: None,
                project_roots: Vec::new(),
                timeout: DEFAULT_CHAT_TIMEOUT,
                delivery_timeout: DEFAULT_DELIVERY_TIMEOUT,
            }
        );
    }

    #[test]
    fn chat_resolves_codex_with_its_workdir_and_timeouts() {
        let mut file = FileConfig::default();
        file.chat.agent = Some("codex".to_owned());
        file.chat.workdir = Some("~/src".to_owned());
        file.chat.project_roots = vec!["~".to_owned(), "/srv/code".to_owned()];
        file.chat.timeout = Some(30.0);
        file.dictation_timeouts.delivery = Some(2.0);
        let environment = empty_environment();

        let chat = chat(&file, &environment).expect("codex resolves");

        assert_eq!(chat.agent, ChatAgent::Codex);
        assert_eq!(chat.workdir, Some(environment.home.join("src")));
        assert_eq!(
            chat.project_roots,
            vec![environment.home, PathBuf::from("/srv/code")]
        );
        assert_eq!(chat.timeout, Duration::from_secs(30));
        assert_eq!(chat.delivery_timeout, Duration::from_secs(2));
    }

    #[test]
    fn a_chat_timeout_the_client_would_not_wait_out_is_refused() {
        let mut file = FileConfig::default();
        file.chat.timeout = Some(900.0);

        assert!(matches!(
            chat(&file, &empty_environment()),
            Err(ConfigError::ChatTimeoutTooLong { .. })
        ));
    }

    #[test]
    fn the_chat_agent_environment_override_wins_and_unknown_agents_are_refused() {
        let mut file = FileConfig::default();
        file.chat.agent = Some("codex".to_owned());
        let mut environment = empty_environment();
        environment.chat_agent = Some("claude-code".to_owned());

        let resolved = chat(&file, &environment).expect("the override resolves");
        assert_eq!(resolved.agent.name(), "claude-code");

        environment.chat_agent = Some("gemini".to_owned());
        assert!(matches!(
            chat(&file, &environment),
            Err(ConfigError::UnsupportedValue {
                field: "chat.agent",
                ..
            })
        ));
    }

    #[test]
    fn tts_defaults_to_the_system_backend_at_the_natural_pace() {
        let tts = tts(&FileConfig::default(), &empty_environment()).expect("defaults must resolve");

        assert_eq!(
            tts,
            TtsConfig {
                backend: TtsBackend::System,
                voice: None,
                rate: SpeakingRate::standard(),
                fallback: TtsFallback::None,
            }
        );
    }

    #[test]
    fn tts_resolves_a_voice_and_rate() {
        let mut file = FileConfig::default();
        file.tts.voice = Some("Samantha".to_owned());
        file.tts.rate = Some(1.5);

        let tts = tts(&file, &empty_environment()).expect("a voice and rate must resolve");

        assert_eq!(tts.voice, Some(Voice::new("Samantha").expect("valid")));
        assert_eq!(tts.rate, SpeakingRate::new(1.5).expect("valid"));
    }

    #[test]
    fn the_tts_rate_environment_override_wins() {
        let mut file = FileConfig::default();
        file.tts.rate = Some(1.5);
        let mut environment = empty_environment();
        environment.tts_rate = Some("2.0".to_owned());

        let tts = tts(&file, &environment).expect("the override must resolve");

        assert_eq!(tts.rate, SpeakingRate::new(2.0).expect("valid"));
    }

    #[test]
    fn tts_resolves_kokoro_with_its_own_model_defaults() {
        let mut file = FileConfig::default();
        file.tts.backend = Some("kokoro".to_owned());
        file.tts.fallback = Some("system".to_owned());

        let tts = tts(&file, &empty_environment()).expect("kokoro must resolve");

        assert_eq!(
            tts.backend,
            TtsBackend::Kokoro {
                model: PathBuf::from("/home/user/models/kokoro/kokoro.onnx"),
                voices: PathBuf::from("/home/user/models/kokoro/voices.bin"),
            }
        );
        assert_eq!(tts.fallback, TtsFallback::System);
    }

    #[test]
    fn tts_settings_the_backend_does_not_have_are_rejected() {
        let mut file = FileConfig::default();
        file.tts.model = Some("~/models/kokoro/kokoro.onnx".to_owned());
        let error = tts(&file, &empty_environment()).expect_err("the system backend has no model");
        assert!(matches!(
            error,
            ConfigError::NotApplicableToBackend {
                field: "tts.model",
                ..
            }
        ));

        let mut file = FileConfig::default();
        file.tts.fallback = Some("system".to_owned());
        let error = tts(&file, &empty_environment())
            .expect_err("the system backend has nothing to fall back to");
        assert!(matches!(
            error,
            ConfigError::NotApplicableToBackend {
                field: "tts.fallback",
                ..
            }
        ));
    }

    #[test]
    fn tts_resolves_qwen_with_its_own_directory_defaults() {
        let mut file = FileConfig::default();
        file.tts.backend = Some("qwen".to_owned());

        let tts = tts(&file, &empty_environment()).expect("qwen must resolve");

        assert_eq!(
            tts.backend,
            TtsBackend::Qwen {
                model_dir: PathBuf::from("/home/user/models/qwen/qwen3-tts-0.6b-customvoice"),
                tokenizer_dir: PathBuf::from("/home/user/models/qwen/qwen3-tts-tokenizer"),
            }
        );
    }

    #[test]
    fn tts_rejects_the_qwen_tokenizer_setting_on_other_backends() {
        let mut file = FileConfig::default();
        file.tts.tokenizer = Some("~/models/qwen/qwen3-tts-tokenizer".to_owned());
        let error = tts(&file, &empty_environment())
            .expect_err("the system backend has no speech tokenizer");
        assert!(matches!(
            error,
            ConfigError::NotApplicableToBackend {
                field: "tts.tokenizer",
                ..
            }
        ));

        let mut file = FileConfig::default();
        file.tts.backend = Some("kokoro".to_owned());
        file.tts.voices = Some("~/models/kokoro/voices.bin".to_owned());
        file.tts.tokenizer = Some("~/models/qwen/qwen3-tts-tokenizer".to_owned());
        let error =
            tts(&file, &empty_environment()).expect_err("kokoro has no use for a speech tokenizer");
        assert!(matches!(
            error,
            ConfigError::NotApplicableToBackend {
                field: "tts.tokenizer",
                ..
            }
        ));
    }

    #[test]
    fn tts_rejects_kokoro_voices_for_qwen() {
        let mut file = FileConfig::default();
        file.tts.backend = Some("qwen".to_owned());
        file.tts.voices = Some("~/models/kokoro/voices.bin".to_owned());

        let error = tts(&file, &empty_environment()).expect_err("qwen has no voices file");

        assert_eq!(
            error.to_string(),
            "tts.voices does not apply to the qwen backend"
        );
    }

    #[test]
    fn tts_rejects_an_unknown_backend_and_a_wild_rate() {
        let mut file = FileConfig::default();
        file.tts.backend = Some("elevenlabs".to_owned());
        let error =
            tts(&file, &empty_environment()).expect_err("an unknown backend must be rejected");
        assert!(matches!(
            error,
            ConfigError::UnsupportedValue {
                field: "tts.backend",
                ..
            }
        ));

        let mut file = FileConfig::default();
        file.tts.rate = Some(9.0);
        let error = tts(&file, &empty_environment()).expect_err("a wild rate must be rejected");
        assert!(matches!(
            error,
            ConfigError::InvalidSpeakingRate {
                field: "tts.rate",
                ..
            }
        ));
    }

    #[test]
    fn listen_inherits_a_streaming_transcription_backend() {
        let mut file = FileConfig::default();
        file.transcription.backend = Some("moonshine".to_owned());

        let listen = listen(&file, &empty_environment()).expect("moonshine must be inherited");

        assert_eq!(
            listen,
            ListenConfig {
                transcription: ListenTranscription::Moonshine {
                    model_directory: PathBuf::from("/home/user/models/moonshine/base"),
                },
                transcripts_dir: PathBuf::from("/home/user/.aria/transcripts"),
            }
        );
    }

    #[test]
    fn listen_refuses_a_backend_that_cannot_stream() {
        // The default transcription backend is whisper, which listen does
        // not offer.
        let error = listen(&FileConfig::default(), &empty_environment())
            .expect_err("a whole-mode inheritance must be refused");
        assert!(matches!(
            error,
            ConfigError::UnsupportedValue {
                field: "listen.backend",
                ..
            }
        ));
    }

    #[test]
    fn an_own_listen_backend_outranks_the_transcription_section() {
        let mut file = FileConfig::default();
        file.transcription.backend = Some("whisper".to_owned());
        file.listen.backend = Some("parakeet".to_owned());
        file.listen.model = Some("~/models/tdt".to_owned());

        let listen = listen(&file, &empty_environment()).expect("the listen section must win");

        assert_eq!(
            listen.transcription,
            ListenTranscription::Parakeet {
                model_directory: PathBuf::from("/home/user/models/tdt"),
                device: TranscriptionDevice::Cpu,
            }
        );
    }

    #[test]
    fn capture_requires_a_path_for_the_wav_file_backend() {
        let error = resolve("wav-file", None, None).expect_err("a missing path must be rejected");
        assert!(matches!(
            error,
            ConfigError::MissingValue {
                field: "capture.path",
                ..
            }
        ));
    }
}
