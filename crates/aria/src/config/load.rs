//! Resolution of settings-file values and defaults.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::audio::ReplayPace;
use crate::primitives::user_home;

use aria_kit::backend::ModelDownload;
use aria_kit::synthesis::{SpeakingRate, Voice};

use super::ChatConfig;
use super::WakeWordOverride;
use super::{
    CaptureConfig, ConfigError, DaemonConfig, DictationConfig, DictationTimeouts, HotkeyConfig,
    ListenConfig, ListenTranscription, MemoryConfig, RelayConfig, SpeakerConfig, SpeechDefaults,
    TranscriptionBackend, TranscriptionConfig, TranscriptionDevice, TtsBackend, TtsConfig,
    TtsEngine, TtsFallback, TtsModel, WakeBackend, WakeConfig, backend, tts_backend, wake_backend,
};
use crate::settings::{
    Capture, CaptureBackendName, Delivery, DeviceName, Dictation, Memory, PaceName, Phone,
    SettingsFile, Speaker, Stt, Tts, TtsFallbackName, Wake,
};

/// Where setup downloads models, relative to home. The default model paths
/// below sit inside it.
const MODELS_DIR: &str = ".aria/models";

/// Where each model sits inside the models directory. Setup downloads into
/// the same folders.
pub(crate) mod model_folder {
    pub(crate) const WHISPER: &str = "whisper";
    pub(crate) const WHISPER_CANDLE: &str = "whisper-candle/tiny.en";
    pub(crate) const PARAKEET: &str = "parakeet/parakeet-tdt-0.6b-v3";
    pub(crate) const MOONSHINE: &str = "moonshine/base";
    pub(crate) const NEMOTRON: &str = "nemotron/nemotron-speech-streaming-en-0.6b";
    pub(crate) const QWEN: &str = "qwen/qwen3-tts-0.6b-customvoice";
    pub(crate) const QWEN_TOKENIZER: &str = "qwen/qwen3-tts-tokenizer";
    pub(crate) const KOKORO: &str = "kokoro";
    pub(crate) const SPEAKER: &str = "wespeaker/en";
    pub(crate) const SEGMENTATION: &str = "pyannote";
}

const DEFAULT_WAKE_MODEL: &str = "wakeword/aria.onnx";
const DEFAULT_WAKE_THRESHOLD: f32 = 0.5;
/// A HuggingFace repo id, resolved by the sidecar against its own cache.
const DEFAULT_PARAKEET_MLX_MODEL: &str = "mlx-community/parakeet-tdt-0.6b-v3";
pub(crate) const DEFAULT_RECORDING_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_OVERALL_TIMEOUT: Duration = Duration::from_secs(660);

const WAV_FILE_CAPTURE_BACKEND: &str = "wav_file";

/// Long enough for an assistant to read a repo and think.
const DEFAULT_CHAT_TIMEOUT: Duration = Duration::from_secs(300);

const DEFAULT_LISTEN_TRANSCRIPTS_DIR: &str = ".aria/transcripts";
const DEFAULT_SPEAKERS_DIR: &str = ".aria/speakers";
const DEFAULT_MEMORY_DIR: &str = ".aria/memory";
const DEFAULT_MEMORY_EXTRACT_MODEL: &str = "sonnet";
const DEFAULT_MEMORY_MAX_FACTS: u32 = 10;

pub(crate) const UNATTENDED_ENV: &str = "ARIA_UNATTENDED";
const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Where the Codex CLI keeps its login, as it finds it itself.
pub(crate) fn codex_home(home: &Path) -> PathBuf {
    std::env::var_os(CODEX_HOME_ENV).map_or_else(|| home.join(".codex"), PathBuf::from)
}

/// Raw, because what counts as unattended is the daemon's call.
pub(crate) fn unattended() -> Result<Option<String>, ConfigError> {
    environment(UNATTENDED_ENV)
}

impl DaemonConfig {
    pub(crate) fn load(
        settings: &SettingsFile,
        home: &Path,
        wake_word: WakeWordOverride,
    ) -> Result<Self, ConfigError> {
        let dictation = DictationConfig::load(settings, home)?;
        let wake = wake(&settings.wake, home, &dictation.capture, wake_word)?;
        let tts = TtsConfig::load(&settings.tts, home)?;
        Ok(Self {
            dictation,
            wake,
            hotkey: HotkeyConfig::Enabled,
            relay: relay(&settings.phone),
            tts,
        })
    }
}

/// A paired phone can type into the focused app, so the LAN listener stays
/// off until configured.
const DEFAULT_RELAY_ENABLED: bool = false;
const DEFAULT_RELAY_PORT: u16 = 9850;

fn relay(phone: &Phone) -> RelayConfig {
    if phone.enabled.unwrap_or(DEFAULT_RELAY_ENABLED) {
        RelayConfig::Enabled {
            port: phone.port.unwrap_or(DEFAULT_RELAY_PORT),
        }
    } else {
        RelayConfig::Disabled
    }
}

impl ListenConfig {
    pub(crate) fn load(settings: &SettingsFile, home: &Path) -> Result<Self, ConfigError> {
        let backend = settings.listen.backend.clone();
        // Without its own backend, listen uses the stt engine, when that
        // engine can stream.
        let transcription = match backend {
            Some(backend) => TranscriptionSettings {
                backend,
                model: settings.listen.model.as_deref(),
                device: settings.listen.device,
                interpreter: None,
                server: None,
            }
            .resolve_local(home)?,
            None => match TranscriptionBackend::load(&settings.stt, home)? {
                TranscriptionBackend::Local(engine) => engine,
                TranscriptionBackend::Server { .. } => {
                    return Err(ConfigError::ListenRequiresLocal);
                }
            },
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
        Ok(Self {
            transcription,
            transcripts_dir: settings
                .listen
                .transcripts_dir
                .clone()
                .unwrap_or_else(|| home.join(DEFAULT_LISTEN_TRANSCRIPTS_DIR)),
        })
    }
}

impl SpeakerConfig {
    pub(crate) fn load(speaker: &Speaker, home: &Path) -> Self {
        Self {
            model: model_location(
                speaker.model.as_deref(),
                model_file(model_folder::SPEAKER, &aria_speaker::wespeaker::MODEL[0]),
                home,
            ),
            segmentation_model: model_location(
                speaker.segmentation_model.as_deref(),
                model_file(
                    model_folder::SEGMENTATION,
                    &aria_segmentation::pyannote::MODEL[0],
                ),
                home,
            ),
            profiles_dir: speaker
                .profiles_dir
                .clone()
                .unwrap_or_else(|| home.join(DEFAULT_SPEAKERS_DIR)),
        }
    }
}

pub(crate) fn memory_directory(memory: &Memory, home: &Path) -> PathBuf {
    memory
        .dir
        .clone()
        .unwrap_or_else(|| home.join(DEFAULT_MEMORY_DIR))
}

impl MemoryConfig {
    pub(crate) fn load(memory: &Memory, home: &Path) -> Self {
        Self {
            enabled: memory.enabled.unwrap_or(false),
            extract_model: memory
                .extract_model
                .clone()
                .unwrap_or_else(|| DEFAULT_MEMORY_EXTRACT_MODEL.to_owned()),
            max_facts_per_session: memory
                .max_facts_per_session
                .unwrap_or(DEFAULT_MEMORY_MAX_FACTS),
            dir: memory_directory(memory, home),
        }
    }
}

impl ChatConfig {
    pub(crate) fn load(settings: &SettingsFile) -> Result<Self, ConfigError> {
        let timeout = timeout(
            settings.assistant.timeout_secs,
            "assistant.timeout_secs",
            DEFAULT_CHAT_TIMEOUT,
        )?;
        Ok(Self {
            workdir: settings.assistant.workdir.clone(),
            project_roots: settings.assistant.project_roots.clone().unwrap_or_default(),
            timeout,
            delivery_timeout: delivery_timeout(&settings.delivery)?,
        })
    }
}

impl TtsConfig {
    pub(crate) fn load(tts: &Tts, home: &Path) -> Result<Self, ConfigError> {
        let backend = string_value(tts.backend.clone(), tts_backend::DEFAULT);
        let backend = match backend.as_str() {
            tts_backend::KOKORO => TtsBackend::Model(TtsModel::Kokoro {
                model: model_location(
                    tts.kokoro.model.as_deref(),
                    model_file(model_folder::KOKORO, &aria_tts::kokoro::MODEL[0]),
                    home,
                ),
                voices: model_location(
                    tts.kokoro.voices.as_deref(),
                    model_file(model_folder::KOKORO, &aria_tts::kokoro::MODEL[1]),
                    home,
                ),
            }),
            tts_backend::QWEN => TtsBackend::Model(TtsModel::Qwen {
                model_dir: model_location(tts.qwen.model.as_deref(), model_folder::QWEN, home),
                tokenizer_dir: model_location(
                    tts.qwen.tokenizer.as_deref(),
                    model_folder::QWEN_TOKENIZER,
                    home,
                ),
            }),
            tts_backend::SYSTEM => TtsBackend::System,
            _ => {
                return Err(ConfigError::UnsupportedValue {
                    field: "tts.backend",
                    value: backend,
                    supported: "kokoro, qwen, and system",
                });
            }
        };
        let fallback = tts_fallback(tts.fallback);
        Ok(Self { backend, fallback })
    }
}

impl SpeechDefaults {
    pub(crate) fn load(tts: &Tts, engine: TtsEngine) -> Result<Self, ConfigError> {
        let (voice, field) = match engine {
            TtsEngine::Kokoro => (&tts.kokoro.voice, "tts.kokoro.voice"),
            TtsEngine::Qwen => (&tts.qwen.voice, "tts.qwen.voice"),
            TtsEngine::System => (&tts.system.voice, "tts.system.voice"),
        };
        let voice = voice
            .clone()
            .map(|name| {
                Voice::new(name).map_err(|source| ConfigError::InvalidVoice { field, source })
            })
            .transpose()?;
        Ok(Self {
            voice,
            rate: speaking_rate(tts.rate)?,
        })
    }
}

/// A downloaded file's path relative to the models directory.
fn model_file(folder: &str, file: &ModelDownload) -> PathBuf {
    Path::new(folder).join(file.file_name)
}

/// The configured path, else `default` inside the models directory.
fn model_location(value: Option<&Path>, default: impl AsRef<Path>, home: &Path) -> PathBuf {
    value.map_or_else(|| models_dir(home).join(default), Path::to_path_buf)
}

/// The transcription engine the section names, or the default.
pub(crate) fn transcription_engine(file: &Stt) -> &str {
    file.backend.as_deref().unwrap_or(backend::DEFAULT)
}

pub(crate) fn models_dir(home: &Path) -> PathBuf {
    home.join(MODELS_DIR)
}

/// A model file or folder Aria loads. `section` is the settings table that
/// loads it. `engine` is `None` for the speaker models, which have no
/// engine choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedModel {
    pub(crate) section: &'static str,
    pub(crate) engine: Option<&'static str>,
    pub(crate) path: PathBuf,
}

/// The models `settings` make Aria load.
pub(crate) fn loaded_models(
    settings: &SettingsFile,
    home: &Path,
) -> Result<Vec<LoadedModel>, ConfigError> {
    let mut models = Vec::new();
    let mut add = |section, engine, path| {
        models.push(LoadedModel {
            section,
            engine,
            path,
        });
    };
    if let TranscriptionBackend::Local(engine) = TranscriptionBackend::load(&settings.stt, home)? {
        let name = engine.backend_name();
        if let Some(path) = engine.model_path() {
            add("stt", Some(name), path);
        }
    }
    match ListenConfig::load(settings, home) {
        Ok(listen) => {
            let engine = listen.transcription.backend_name();
            let path = listen.transcription.model_directory();
            add("listen", Some(engine), path);
        }
        // Listen without a backend inherits transcription's. If that engine
        // cannot stream, listen is simply off.
        Err(_) if settings.listen.backend.is_none() => {}
        Err(error) => return Err(error),
    }
    let tts = TtsConfig::load(&settings.tts, home)?;
    let engine = Some(tts.backend.name());
    match tts.backend {
        TtsBackend::Model(TtsModel::Kokoro { model, voices }) => {
            add("tts", engine, model);
            add("tts", engine, voices);
        }
        TtsBackend::Model(TtsModel::Qwen {
            model_dir,
            tokenizer_dir,
        }) => {
            add("tts", engine, model_dir);
            add("tts", engine, tokenizer_dir);
        }
        TtsBackend::System => {}
    }
    let speaker = SpeakerConfig::load(&settings.speaker, home);
    add("speaker", None, speaker.model);
    add("speaker", None, speaker.segmentation_model);
    Ok(models)
}

fn tts_fallback(file_value: Option<TtsFallbackName>) -> TtsFallback {
    match file_value {
        None | Some(TtsFallbackName::None) => TtsFallback::None,
        Some(TtsFallbackName::System) => TtsFallback::System,
    }
}

fn speaking_rate(file_value: Option<f64>) -> Result<SpeakingRate, ConfigError> {
    let Some(rate) = file_value else {
        return Ok(SpeakingRate::standard());
    };
    SpeakingRate::new(rate as f32).map_err(|source| ConfigError::InvalidSpeakingRate {
        field: "tts.rate",
        source,
    })
}

impl DictationConfig {
    pub(crate) fn load(settings: &SettingsFile, home: &Path) -> Result<Self, ConfigError> {
        Ok(Self {
            capture: capture(&settings.capture)?,
            transcription: TranscriptionBackend::load(&settings.stt, home)?,
            dictation_timeouts: timeouts(&settings.dictation, &settings.delivery)?,
            cleanup_llm: cleanup_llm(&settings.dictation)?,
        })
    }
}

fn cleanup_llm(dictation: &Dictation) -> Result<Option<super::LlmCleanupConfig>, ConfigError> {
    let Some(config) = &dictation.cleanup_llm else {
        return Ok(None);
    };
    crate::settings::validate_cleanup_llm(&config.endpoint, &config.model)
        .map_err(ConfigError::CleanupLlm)?;
    Ok(Some(super::LlmCleanupConfig {
        endpoint: config.endpoint.clone(),
        model: config.model.clone(),
    }))
}

fn capture(capture: &Capture) -> Result<CaptureConfig, ConfigError> {
    match capture.backend.unwrap_or(CaptureBackendName::Microphone) {
        CaptureBackendName::Microphone => Ok(CaptureConfig::Microphone),
        CaptureBackendName::WavFile => {
            let path = capture.path.clone().ok_or(ConfigError::MissingValue {
                field: "capture.path",
                backend: WAV_FILE_CAPTURE_BACKEND,
            })?;
            Ok(CaptureConfig::WavFile {
                path,
                pace: replay_pace(capture.pace),
            })
        }
    }
}

fn replay_pace(pace: Option<PaceName>) -> ReplayPace {
    match pace {
        None | Some(PaceName::Immediate) => ReplayPace::Immediate,
        Some(PaceName::Realtime) => ReplayPace::Realtime,
    }
}

fn wake(
    wake: &Wake,
    home: &Path,
    capture: &CaptureConfig,
    wake_word: WakeWordOverride,
) -> Result<WakeConfig, ConfigError> {
    let enabled = matches!(wake_word, WakeWordOverride::Enable) || wake.enabled.unwrap_or(false);
    if !enabled {
        return Ok(WakeConfig::Disabled);
    }

    let backend = parse_wake_backend(&string_value(wake.backend.clone(), wake_backend::DEFAULT))?;

    if !matches!(capture, CaptureConfig::Microphone) {
        return Err(ConfigError::WakeRequiresMicrophone);
    }

    let model = match (wake.model.as_deref(), backend) {
        (Some(model), _) => Some(model),
        (None, WakeBackend::OpenWakeWord) => None,
        // No aria-trained livekit head ships with Aria, and the openWakeWord
        // default would load into the wrong backend.
        (None, WakeBackend::LiveKit) => {
            return Err(ConfigError::MissingValue {
                field: "wake.model",
                backend: wake_backend::LIVEKIT,
            });
        }
    };
    let classifier = model_location(model, DEFAULT_WAKE_MODEL, home);
    Ok(WakeConfig::Enabled {
        backend,
        classifier,
        threshold: wake.threshold.unwrap_or(DEFAULT_WAKE_THRESHOLD),
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

impl TranscriptionBackend {
    pub(crate) fn load(file: &Stt, home: &Path) -> Result<Self, ConfigError> {
        TranscriptionSettings::read(file).resolve(home)
    }
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
        server: None,
    }
    .resolve_local(home)
}

/// The transcription settings as written, before a backend claims them. Only
/// `backend` applies to every backend; the rest belong to some of them.
struct TranscriptionSettings<'a> {
    backend: String,
    model: Option<&'a Path>,
    device: Option<DeviceName>,
    interpreter: Option<&'a Path>,
    server: Option<String>,
}

impl<'a> TranscriptionSettings<'a> {
    fn read(file: &'a Stt) -> Self {
        Self {
            backend: string_value(file.backend.clone(), backend::DEFAULT),
            model: file.model.as_deref().map(Path::new),
            device: file.device,
            interpreter: file.interpreter.as_deref(),
            server: file.server.clone(),
        }
    }

    fn resolve(self, home: &Path) -> Result<TranscriptionBackend, ConfigError> {
        if self.backend != backend::SERVER {
            return self.resolve_local(home).map(TranscriptionBackend::Local);
        }
        let endpoint = self.server.ok_or(ConfigError::MissingValue {
            field: "stt.server",
            backend: backend::SERVER,
        })?;
        Ok(TranscriptionBackend::Server {
            endpoint: endpoint.try_into()?,
        })
    }

    fn resolve_local(self, home: &Path) -> Result<TranscriptionConfig, ConfigError> {
        match self.backend.as_str() {
            backend::WHISPER => Ok(TranscriptionConfig::Whisper {
                model: model_location(
                    self.model,
                    model_file(model_folder::WHISPER, &aria_stt::whisper::MODEL[0]),
                    home,
                ),
            }),
            backend::WHISPER_CANDLE => Ok(TranscriptionConfig::WhisperCandle {
                model_directory: model_location(self.model, model_folder::WHISPER_CANDLE, home),
            }),
            backend::PARAKEET => Ok(TranscriptionConfig::Parakeet {
                model_directory: model_location(self.model, model_folder::PARAKEET, home),
                device: transcription_device(self.device),
            }),
            backend::MOONSHINE => Ok(TranscriptionConfig::Moonshine {
                model_directory: model_location(self.model, model_folder::MOONSHINE, home),
            }),
            backend::NEMOTRON => Ok(TranscriptionConfig::Nemotron {
                model_directory: model_location(self.model, model_folder::NEMOTRON, home),
            }),
            backend::PARAKEET_MLX => {
                let interpreter = self.interpreter.ok_or(ConfigError::MissingValue {
                    field: "stt.interpreter",
                    backend: backend::PARAKEET_MLX,
                })?;
                Ok(TranscriptionConfig::ParakeetMlx {
                    // Only `stt.model` reaches this engine, and it
                    // is a string, so the text comes back whole.
                    model: self.model.map_or_else(
                        || DEFAULT_PARAKEET_MLX_MODEL.to_owned(),
                        |model| model.to_string_lossy().into_owned(),
                    ),
                    interpreter: interpreter.to_path_buf(),
                })
            }
            _ => Err(ConfigError::UnsupportedValue {
                field: "stt.backend",
                value: self.backend,
                supported: "whisper, whisper-candle, parakeet, moonshine, nemotron, parakeet-mlx, and server",
            }),
        }
    }
}

fn transcription_device(device: Option<DeviceName>) -> TranscriptionDevice {
    match device {
        None | Some(DeviceName::Cpu) => TranscriptionDevice::Cpu,
        Some(DeviceName::Gpu) => TranscriptionDevice::Gpu,
    }
}

fn timeouts(dictation: &Dictation, delivery: &Delivery) -> Result<DictationTimeouts, ConfigError> {
    let recording = timeout(
        dictation.max_recording_secs,
        "dictation.max_recording_secs",
        DEFAULT_RECORDING_TIMEOUT,
    )?;
    let overall = timeout(
        dictation.timeout_secs,
        "dictation.timeout_secs",
        DEFAULT_OVERALL_TIMEOUT,
    )?;

    Ok(DictationTimeouts {
        recording,
        delivery: delivery_timeout(delivery)?,
        overall,
    })
}

fn delivery_timeout(delivery: &Delivery) -> Result<Duration, ConfigError> {
    timeout(
        delivery.timeout_secs,
        "delivery.timeout_secs",
        DEFAULT_DELIVERY_TIMEOUT,
    )
}

fn string_value(file_value: Option<String>, default: &'static str) -> String {
    file_value.unwrap_or_else(|| default.to_owned())
}

fn timeout(
    file_value: Option<f64>,
    field: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    match file_value {
        Some(seconds) => duration(seconds, field),
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

pub(crate) fn home_directory() -> Result<PathBuf, ConfigError> {
    user_home::resolve().map_err(ConfigError::Home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Loaded;

    fn test_home() -> PathBuf {
        PathBuf::from("/home/user")
    }

    fn file(text: &str) -> SettingsFile {
        Loaded::parse(text, &test_home())
            .expect("the file is valid")
            .settings
    }

    /// The typed values of `text`, before the file's own rules, for a state
    /// only a mix of live and frozen keys or a direct call can produce.
    fn unchecked(text: &str) -> SettingsFile {
        toml_edit::de::from_str(text).expect("the TOML parses")
    }

    #[test]
    fn server_requires_an_explicit_https_origin_without_credentials() {
        assert!(matches!(
            settings(backend::SERVER).resolve(Path::new("/missing-home")),
            Err(ConfigError::MissingValue {
                field: "stt.server",
                ..
            })
        ));
        for (endpoint, canonical) in [
            ("https://localhost:9851", "https://localhost:9851"),
            ("https://Example.Test/", "https://example.test"),
            ("https://[::1]:443", "https://[::1]:443"),
        ] {
            let file = file(&format!(
                "[stt]\nbackend = \"server\"\nserver = '{endpoint}'\n"
            ));
            let TranscriptionBackend::Server { endpoint } =
                TranscriptionBackend::load(&file.stt, &test_home()).unwrap()
            else {
                panic!("expected a server backend");
            };
            assert_eq!(endpoint.to_string(), canonical);
            let models = loaded_models(&file, Path::new("/missing-home")).expect("resolves");
            assert!(models.iter().all(|model| model.section != "stt"));
        }
        for endpoint in [
            "http://localhost",
            "https://user:secret@example.test",
            "https://example.test?key=secret",
            "https://example.test#secret",
            "https://example.test/path",
            "",
            "unix:///tmp/server.sock",
        ] {
            assert!(matches!(
                TranscriptionSettings {
                    server: Some(endpoint.into()),
                    ..settings(backend::SERVER)
                }
                .resolve(Path::new("/missing-home")),
                Err(ConfigError::InvalidServerEndpoint)
            ));
        }
    }

    #[test]
    fn listen_requires_an_explicit_local_engine_with_server_dictation() {
        let server = "[stt]\nbackend = \"server\"\nserver = \"https://example.test\"\n";
        assert!(matches!(
            ListenConfig::load(&file(server), &test_home()),
            Err(ConfigError::ListenRequiresLocal)
        ));
        let moonshine = format!("{server}[listen]\nbackend = \"moonshine\"\n");
        assert!(matches!(
            ListenConfig::load(&file(&moonshine), &test_home())
                .unwrap()
                .transcription,
            ListenTranscription::Moonshine { .. }
        ));
    }

    #[test]
    fn llm_cleanup_requires_cleanup_and_preserves_settings() {
        let home_dir = test_home();
        assert!(
            DictationConfig::load(&SettingsFile::default(), &home_dir)
                .unwrap()
                .cleanup_llm
                .is_none()
        );
        let llm = "[dictation.cleanup_llm]\nendpoint = \"http://127.0.0.1:8080/v1/chat/completions\"\nmodel = \"local-model\"\n";
        let config = DictationConfig::load(
            &file(&format!("[dictation]\ncleanup = true\n{llm}")),
            &home_dir,
        )
        .unwrap();
        assert_eq!(
            config.cleanup_llm,
            Some(super::super::LlmCleanupConfig {
                endpoint: "http://127.0.0.1:8080/v1/chat/completions".to_owned(),
                model: "local-model".to_owned(),
            })
        );
        // With cleanup off the table goes unused: each dictation reads the
        // switch, so a live change never meets a frozen table as an error.
        DictationConfig::load(
            &unchecked(&format!("[dictation]\ncleanup = false\n{llm}")),
            &home_dir,
        )
        .unwrap();
    }

    #[test]
    fn llm_cleanup_rejects_invalid_settings() {
        for (endpoint, model) in [
            ("https://example.com/v1/chat/completions", "local-model"),
            ("http://127.0.0.1:8080/v1/chat/completions", ""),
        ] {
            let file = unchecked(&format!(
                "[dictation]\ncleanup = true\n[dictation.cleanup_llm]\nendpoint = \"{endpoint}\"\nmodel = \"{model}\"\n"
            ));
            assert!(matches!(
                DictationConfig::load(&file, &test_home()),
                Err(ConfigError::CleanupLlm(_))
            ));
        }
    }

    #[test]
    fn capture_defaults_to_the_microphone() {
        let capture = capture(&SettingsFile::default().capture);
        assert_eq!(capture.expect("resolves"), CaptureConfig::Microphone);
    }

    #[test]
    fn capture_resolves_a_wav_file_backend() {
        let file = file("[capture]\nbackend = \"wav_file\"\npath = \"~/fixture.wav\"\n");
        assert_eq!(
            capture(&file.capture).expect("resolves"),
            CaptureConfig::WavFile {
                path: PathBuf::from("/home/user/fixture.wav"),
                pace: ReplayPace::Immediate,
            }
        );
    }

    #[test]
    fn capture_resolves_a_paced_replay() {
        let file = file(
            "[capture]\nbackend = \"wav_file\"\npath = \"/fixture.wav\"\npace = \"realtime\"\n",
        );
        assert!(matches!(
            capture(&file.capture).expect("resolves"),
            CaptureConfig::WavFile {
                pace: ReplayPace::Realtime,
                ..
            }
        ));
    }

    #[test]
    fn capture_requires_a_path_for_the_wav_file_backend() {
        let file = unchecked("[capture]\nbackend = \"wav_file\"\n");
        let error = capture(&file.capture).expect_err("a missing path must be rejected");
        assert!(matches!(
            error,
            ConfigError::MissingValue {
                field: "capture.path",
                ..
            }
        ));
    }

    #[test]
    fn disabled_wake_ignores_its_dormant_settings() {
        let file = file("[wake]\nenabled = false\nmodel = \"/missing/model.onnx\"\n");

        let wake = wake(
            &file.wake,
            &test_home(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect("a disabled wake word reads nothing else");

        assert_eq!(wake, WakeConfig::Disabled);
    }

    #[test]
    fn a_wake_word_run_outranks_a_disabled_file() {
        let wake = wake(
            &file("[wake]\nenabled = false\n").wake,
            &test_home(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Enable,
        )
        .expect("the command should enable monitoring");

        assert!(matches!(wake, WakeConfig::Enabled { .. }));
    }

    /// The file's rule skips a disabled wake word, so a run that enables one
    /// meets this rule here first.
    #[test]
    fn a_wake_word_run_needs_the_microphone() {
        let error = wake(
            &file("[wake]\nenabled = false\n").wake,
            &test_home(),
            &CaptureConfig::WavFile {
                path: PathBuf::from("/fixture.wav"),
                pace: ReplayPace::Immediate,
            },
            WakeWordOverride::Enable,
        )
        .expect_err("a replay has no live audio to monitor");
        assert!(matches!(error, ConfigError::WakeRequiresMicrophone));
    }

    fn enabled_wake(rest: &str) -> SettingsFile {
        file(&format!("[wake]\nenabled = true\n{rest}"))
    }

    #[test]
    fn wake_defaults_to_openwakeword_and_its_own_model() {
        let wake = wake(
            &enabled_wake("").wake,
            &test_home(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect("the default backend must resolve");

        assert_eq!(
            wake,
            WakeConfig::Enabled {
                backend: WakeBackend::OpenWakeWord,
                classifier: PathBuf::from("/home/user/.aria/models/wakeword/aria.onnx"),
                threshold: DEFAULT_WAKE_THRESHOLD,
            }
        );
    }

    #[test]
    fn wake_resolves_the_livekit_backend() {
        let file = enabled_wake("backend = \"livekit\"\nmodel = \"~/models/hey_livekit.onnx\"\n");

        let wake = wake(
            &file.wake,
            &test_home(),
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
    fn a_wake_word_run_of_livekit_without_a_model_is_refused() {
        let error = wake(
            &file("[wake]\nbackend = \"livekit\"\n").wake,
            &test_home(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Enable,
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

    /// Every setting unset but the backend, so each test names only what it
    /// is about.
    fn settings(backend: &str) -> TranscriptionSettings<'static> {
        TranscriptionSettings {
            backend: backend.to_owned(),
            model: None,
            device: None,
            interpreter: None,
            server: None,
        }
    }

    #[test]
    fn each_backend_falls_back_to_its_own_model() {
        let defaults = [
            (
                "whisper",
                TranscriptionConfig::Whisper {
                    model: PathBuf::from(
                        "/home/user/.aria/models/whisper/ggml-large-v3-turbo-q8_0.bin",
                    ),
                },
            ),
            (
                "parakeet",
                TranscriptionConfig::Parakeet {
                    model_directory: PathBuf::from(
                        "/home/user/.aria/models/parakeet/parakeet-tdt-0.6b-v3",
                    ),
                    device: TranscriptionDevice::Cpu,
                },
            ),
            (
                "moonshine",
                TranscriptionConfig::Moonshine {
                    model_directory: PathBuf::from("/home/user/.aria/models/moonshine/base"),
                },
            ),
            (
                "nemotron",
                TranscriptionConfig::Nemotron {
                    model_directory: PathBuf::from(
                        "/home/user/.aria/models/nemotron/nemotron-speech-streaming-en-0.6b",
                    ),
                },
            ),
        ];

        for (backend, expected) in defaults {
            let transcription = settings(backend)
                .resolve_local(Path::new("/home/user"))
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
        let file = file("[stt]\nbackend = \"parakeet\"\nmodel = \"~/models/tdt\"\n");
        assert_eq!(
            TranscriptionBackend::load(&file.stt, &test_home()).expect("parakeet must resolve"),
            TranscriptionBackend::Local(TranscriptionConfig::Parakeet {
                model_directory: PathBuf::from("/home/user/models/tdt"),
                device: TranscriptionDevice::Cpu,
            })
        );
    }

    #[test]
    fn parakeet_mlx_keeps_its_model_as_a_repo_id() {
        let file = file(
            "[stt]\nbackend = \"parakeet-mlx\"\ninterpreter = \"~/py\"\n\
             model = \"mlx-community/whisper\"\n",
        );
        assert_eq!(
            TranscriptionBackend::load(&file.stt, &test_home()).expect("parakeet-mlx must resolve"),
            TranscriptionBackend::Local(TranscriptionConfig::ParakeetMlx {
                model: "mlx-community/whisper".to_owned(),
                interpreter: PathBuf::from("/home/user/py"),
            })
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
                field: "stt.backend",
                ..
            }
        ));
    }

    #[test]
    fn chat_defaults_to_the_caller_directory_and_default_timeouts() {
        let chat = ChatConfig::load(&SettingsFile::default()).expect("defaults resolve");

        assert_eq!(
            chat,
            ChatConfig {
                workdir: None,
                project_roots: Vec::new(),
                timeout: DEFAULT_CHAT_TIMEOUT,
                delivery_timeout: DEFAULT_DELIVERY_TIMEOUT,
            }
        );
    }

    #[test]
    fn chat_resolves_its_workdir_and_timeouts() {
        let file = file(
            "[assistant]\nworkdir = \"~/src\"\nproject_roots = [\"~\", \"/srv/code\"]\ntimeout_secs = 30.0\n\
             [delivery]\ntimeout_secs = 2.0\n",
        );
        let home_dir = test_home();

        let chat = ChatConfig::load(&file).expect("the assistant table resolves");

        assert_eq!(chat.workdir, Some(home_dir.join("src")));
        assert_eq!(
            chat.project_roots,
            vec![home_dir, PathBuf::from("/srv/code")]
        );
        assert_eq!(chat.timeout, Duration::from_secs(30));
        assert_eq!(chat.delivery_timeout, Duration::from_secs(2));
    }

    #[test]
    fn tts_defaults_to_the_system_backend_at_the_natural_pace() {
        let tts = TtsConfig::load(&SettingsFile::default().tts, &test_home())
            .expect("defaults must resolve");

        assert_eq!(
            tts,
            TtsConfig {
                backend: TtsBackend::System,
                fallback: TtsFallback::None,
            }
        );
    }

    #[test]
    fn tts_resolves_a_voice_and_rate() {
        let file = file("[tts]\nrate = 1.5\n[tts.system]\nvoice = \"Samantha\"\n");

        let saved =
            SpeechDefaults::load(&file.tts, TtsEngine::System).expect("a voice and rate resolve");

        assert_eq!(saved.voice, Some(Voice::new("Samantha").expect("valid")));
        assert_eq!(saved.rate, SpeakingRate::new(1.5).expect("valid"));
    }

    #[test]
    fn each_engine_keeps_its_own_voice() {
        let voices = "[tts.system]\nvoice = \"Samantha\"\n[tts.kokoro]\nvoice = \"af_heart\"\n";
        let voice = |engine| {
            SpeechDefaults::load(&file(voices).tts, engine)
                .expect("resolves")
                .voice
        };

        assert_eq!(
            voice(TtsEngine::System),
            Some(Voice::new("Samantha").expect("valid"))
        );
        assert_eq!(
            voice(TtsEngine::Kokoro),
            Some(Voice::new("af_heart").expect("valid"))
        );
        assert_eq!(voice(TtsEngine::Qwen), None);
    }

    #[test]
    fn tts_resolves_kokoro_with_its_own_model_defaults() {
        let file = file("[tts]\nbackend = \"kokoro\"\nfallback = \"system\"\n");

        let tts = TtsConfig::load(&file.tts, &test_home()).expect("kokoro must resolve");

        assert_eq!(
            tts.backend,
            TtsBackend::Model(TtsModel::Kokoro {
                model: PathBuf::from("/home/user/.aria/models/kokoro/kokoro.onnx"),
                voices: PathBuf::from("/home/user/.aria/models/kokoro/voices.bin"),
            })
        );
        assert_eq!(tts.fallback, TtsFallback::System);
    }

    #[test]
    fn an_engine_switch_ignores_the_other_engines_files() {
        let file = file(
            "[tts]\nbackend = \"qwen\"\n[tts.kokoro]\nvoices = \"~/models/kokoro/voices.bin\"\n",
        );

        let tts =
            TtsConfig::load(&file.tts, &test_home()).expect("kokoro's table is dormant under qwen");

        assert!(matches!(
            tts.backend,
            TtsBackend::Model(TtsModel::Qwen { .. })
        ));
    }

    #[test]
    fn tts_resolves_qwen_with_its_own_directory_defaults() {
        let tts = TtsConfig::load(&file("[tts]\nbackend = \"qwen\"\n").tts, &test_home())
            .expect("qwen must resolve");

        assert_eq!(
            tts.backend,
            TtsBackend::Model(TtsModel::Qwen {
                model_dir: PathBuf::from("/home/user/.aria/models/qwen/qwen3-tts-0.6b-customvoice"),
                tokenizer_dir: PathBuf::from("/home/user/.aria/models/qwen/qwen3-tts-tokenizer"),
            })
        );
    }

    #[test]
    fn listen_inherits_the_transcription_model() {
        let home = Path::new("/home/user");
        let loaded =
            loaded_models(&file("[stt]\nbackend = \"parakeet\"\n"), home).expect("resolves");
        let models = home.join(".aria/models");

        assert_eq!(
            loaded,
            [
                LoadedModel {
                    section: "stt",
                    engine: Some(backend::PARAKEET),
                    path: models.join("parakeet/parakeet-tdt-0.6b-v3"),
                },
                LoadedModel {
                    section: "listen",
                    engine: Some(backend::PARAKEET),
                    path: models.join("parakeet/parakeet-tdt-0.6b-v3"),
                },
                LoadedModel {
                    section: "speaker",
                    engine: None,
                    path: models.join("wespeaker/en/model.onnx"),
                },
                LoadedModel {
                    section: "speaker",
                    engine: None,
                    path: models.join("pyannote/segmentation-3.0.onnx"),
                },
            ],
            "listen inherits the transcription model"
        );
    }

    #[test]
    fn a_configured_model_path_is_the_one_loaded() {
        let file = file("[stt]\nbackend = \"parakeet\"\nmodel = \"~/tdt\"\n");

        let loaded = loaded_models(&file, Path::new("/home/user")).expect("resolves");

        assert!(
            loaded
                .iter()
                .any(|model| model.section == "stt" && model.path == Path::new("/home/user/tdt"))
        );
        assert!(loaded.iter().any(|model| model.section == "listen"
            && model.path == Path::new("/home/user/tdt")));
    }

    #[test]
    fn listen_inherits_a_streaming_transcription_backend() {
        let listen = ListenConfig::load(&file("[stt]\nbackend = \"moonshine\"\n"), &test_home())
            .expect("moonshine must be inherited");

        assert_eq!(
            listen,
            ListenConfig {
                transcription: ListenTranscription::Moonshine {
                    model_directory: PathBuf::from("/home/user/.aria/models/moonshine/base"),
                },
                transcripts_dir: PathBuf::from("/home/user/.aria/transcripts"),
            }
        );
    }

    #[test]
    fn listen_refuses_a_backend_that_cannot_stream() {
        let error = ListenConfig::load(&file("[stt]\nbackend = \"whisper\"\n"), &test_home())
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
    fn an_own_listen_backend_outranks_the_transcription_table() {
        let file = file(
            "[stt]\nbackend = \"whisper\"\n[listen]\nbackend = \"parakeet\"\nmodel = \"~/models/tdt\"\n",
        );

        let listen = ListenConfig::load(&file, &test_home()).expect("the listen table must win");

        assert_eq!(
            listen.transcription,
            ListenTranscription::Parakeet {
                model_directory: PathBuf::from("/home/user/models/tdt"),
                device: TranscriptionDevice::Cpu,
            }
        );
    }

    /// An edit to an egress key after startup does not move the endpoint in
    /// use. Dictation keeps the server it started with until a restart.
    #[test]
    fn a_server_edited_after_startup_is_not_the_one_dictation_uses() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        let started = "[stt]\nbackend = \"server\"\nserver = \"https://started.test:9851\"\n";
        std::fs::write(&path, started).expect("written");
        let store = crate::settings::SettingsCache::open(path.clone(), dir.path().to_path_buf())
            .expect("opens");
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(
            &path,
            "[stt]\nbackend = \"server\"\nserver = \"https://elsewhere.test:9851\"\n",
        )
        .expect("edited");
        store.poll().expect("reloads");

        let on_disk = Loaded::parse(&std::fs::read_to_string(&path).unwrap(), dir.path())
            .expect("valid")
            .settings;
        let in_use = DictationConfig::load(&store.effective(), &test_home()).expect("resolves");

        assert_eq!(
            on_disk.stt.server.as_deref(),
            Some("https://elsewhere.test:9851")
        );
        let TranscriptionBackend::Server { endpoint } = in_use.transcription else {
            panic!("expected a server backend");
        };
        assert_eq!(endpoint.to_string(), "https://started.test:9851");
    }
}
