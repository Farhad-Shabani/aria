//! Resolution of settings-file values and defaults.

use std::path::{Path, PathBuf};

use aria_kit::audio::ReplayPace;

use aria_kit::synthesis::{SpeakingRate, Voice};

use super::{
    CaptureConfig, DictationConfig, DictationTimeouts, ListenConfig, ListenTranscription,
    SpeakerConfig, SpeechDefaults, TranscriptionBackend, TranscriptionConfig, TranscriptionDevice,
    TtsBackend, TtsConfig, TtsEngine, TtsFallback, TtsModel,
};
use aria_settings::config::{
    ConfigError, DEFAULT_OVERALL_TIMEOUT, DEFAULT_RECORDING_TIMEOUT, backend, delivery_timeout,
    model_file, model_folder, model_location, string_value, timeout, tts_backend,
};
use aria_settings::{
    Capture, CaptureBackendName, Delivery, DeviceName, Dictation, InvalidValue, Key, PaceName,
    SettingsFile, Speaker, Stt, Tts, TtsFallbackName,
};

/// A HuggingFace repo id, resolved by the sidecar against its own cache.
const DEFAULT_PARAKEET_MLX_MODEL: &str = "mlx-community/parakeet-tdt-0.6b-v3";
const WAV_FILE_CAPTURE_BACKEND: &str = "wav_file";

const DEFAULT_LISTEN_TRANSCRIPTS_DIR: &str = ".aria/transcripts";
const DEFAULT_SPEAKERS_DIR: &str = ".aria/speakers";
impl ListenConfig {
    pub fn load(settings: &SettingsFile, home: &Path) -> Result<Self, ConfigError> {
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
                    return Err(ConfigError::ListenUnusableEngine(
                        backend::SERVER.to_owned(),
                    ));
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
                return Err(ConfigError::ListenUnusableEngine(
                    other.backend_name().to_owned(),
                ));
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
    pub fn load(speaker: &Speaker, home: &Path) -> Self {
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

impl TtsConfig {
    pub fn load(tts: &Tts, home: &Path) -> Result<Self, ConfigError> {
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
                return Err(ConfigError::Invalid {
                    key: Key::TtsBackend,
                    source: InvalidValue::Unsupported {
                        value: backend,
                        supported: "kokoro, qwen, and system".to_owned(),
                    },
                });
            }
        };
        let fallback = tts_fallback(tts.fallback);
        Ok(Self { backend, fallback })
    }
}

impl SpeechDefaults {
    pub fn load(tts: &Tts, engine: TtsEngine) -> Result<Self, ConfigError> {
        let (voice, key) = match engine {
            TtsEngine::Kokoro => (&tts.kokoro.voice, Key::TtsKokoroVoice),
            TtsEngine::Qwen => (&tts.qwen.voice, Key::TtsQwenVoice),
            TtsEngine::System => (&tts.system.voice, Key::TtsSystemVoice),
        };
        let voice = voice
            .clone()
            .map(|name| {
                Voice::new(name).map_err(|source| ConfigError::Invalid {
                    key,
                    source: InvalidValue::Voice(source),
                })
            })
            .transpose()?;
        Ok(Self {
            voice,
            rate: speaking_rate(tts.rate)?,
        })
    }
}

/// A model file or folder Aria loads. `section` is the settings table that
/// loads it. `engine` is `None` for the speaker models, which have no
/// engine choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModel {
    pub section: &'static str,
    pub engine: Option<&'static str>,
    pub path: PathBuf,
}

/// The models `settings` make Aria load.
pub fn loaded_models(
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
    SpeakingRate::new(rate as f32).map_err(|source| ConfigError::Invalid {
        key: Key::TtsRate,
        source: InvalidValue::SpeakingRate(source),
    })
}

impl DictationConfig {
    pub fn load(settings: &SettingsFile, home: &Path) -> Result<Self, ConfigError> {
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
    aria_settings::validate_cleanup_llm(&config.endpoint, &config.model).map_err(|source| {
        ConfigError::Invalid {
            key: Key::DictationCleanupLlmEndpoint,
            source: InvalidValue::Cleanup(source),
        }
    })?;
    Ok(Some(super::LlmCleanupConfig {
        endpoint: config.endpoint.clone(),
        model: config.model.clone(),
    }))
}

fn capture(capture: &Capture) -> Result<CaptureConfig, ConfigError> {
    match capture.backend.unwrap_or(CaptureBackendName::Microphone) {
        CaptureBackendName::Microphone => Ok(CaptureConfig::Microphone),
        CaptureBackendName::WavFile => {
            let path = capture.path.clone().ok_or_else(|| ConfigError::Invalid {
                key: Key::CapturePath,
                source: InvalidValue::Required {
                    backend: WAV_FILE_CAPTURE_BACKEND.to_owned(),
                },
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

impl TranscriptionBackend {
    pub fn load(file: &Stt, home: &Path) -> Result<Self, ConfigError> {
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
        let invalid = |source| ConfigError::Invalid {
            key: Key::SttServer,
            source,
        };
        let endpoint = self.server.ok_or_else(|| {
            invalid(InvalidValue::Required {
                backend: backend::SERVER.to_owned(),
            })
        })?;
        Ok(TranscriptionBackend::Server {
            endpoint: endpoint.try_into().map_err(invalid)?,
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
                let interpreter = self.interpreter.ok_or_else(|| {
                    ConfigError::Invalid { key: Key::SttInterpreter, source: InvalidValue::Required {
                            backend: backend::PARAKEET_MLX.to_owned(),
                        } }
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
            _ => Err(ConfigError::Invalid { key: Key::SttBackend, source: InvalidValue::Unsupported {
                    value: self.backend,
                    supported: "whisper, whisper-candle, parakeet, moonshine, nemotron, parakeet-mlx, and server".to_owned(),
                } }),
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
        Key::DictationMaxRecordingSecs,
        DEFAULT_RECORDING_TIMEOUT,
    )?;
    let overall = timeout(
        dictation.timeout_secs,
        Key::DictationTimeoutSecs,
        DEFAULT_OVERALL_TIMEOUT,
    )?;

    Ok(DictationTimeouts {
        recording,
        delivery: delivery_timeout(delivery)?,
        overall,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_settings::Loaded;
    use aria_settings::testing::{file, test_home};
    use std::time::Duration;

    /// The typed values of `text`, before the file's own rules, for a state
    /// only a mix of live and frozen keys or a direct call can produce.
    fn unchecked(text: &str) -> SettingsFile {
        toml_edit::de::from_str(text).expect("the TOML parses")
    }

    #[test]
    fn server_requires_an_explicit_https_origin_without_credentials() {
        assert!(matches!(
            settings(backend::SERVER).resolve(Path::new("/missing-home")),
            Err(ConfigError::Invalid {
                key: Key::SttServer,
                source: InvalidValue::Required { .. },
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
                Err(ConfigError::Invalid {
                    key: Key::SttServer,
                    source: InvalidValue::ServerEndpoint,
                })
            ));
        }
    }

    #[test]
    fn listen_requires_an_explicit_local_engine_with_server_dictation() {
        let server = "[stt]\nbackend = \"server\"\nserver = \"https://example.test\"\n";
        assert!(matches!(
            ListenConfig::load(&file(server), &test_home()),
            Err(ConfigError::ListenUnusableEngine(ref engine)) if engine == backend::SERVER
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
                Err(ConfigError::Invalid {
                    key: Key::DictationCleanupLlmEndpoint,
                    source: InvalidValue::Cleanup(_),
                })
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
            ConfigError::Invalid {
                key: Key::CapturePath,
                source: InvalidValue::Required { .. },
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
            ConfigError::Invalid {
                key: Key::SttBackend,
                source: InvalidValue::Unsupported { .. },
            }
        ));
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
            ConfigError::ListenUnusableEngine(ref engine) if engine == "whisper"
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
        let store = aria_settings::SettingsCache::open(path.clone(), dir.path().to_path_buf())
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
            .into_settings();
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

    #[test]
    fn the_defaults_load_and_match_what_code_does_without_them() {
        let home = Path::new("/home/user");
        let defaults = Loaded::parse(&aria_settings::testing::defaults_text(), home)
            .expect("every default is valid")
            .into_settings();
        let empty = SettingsFile::default();
        let home_dir = home.to_path_buf();

        assert_eq!(
            DictationConfig::load(&defaults, &home_dir).unwrap(),
            DictationConfig::load(&empty, &home_dir).unwrap()
        );
        assert_eq!(
            TtsConfig::load(&defaults.tts, &home_dir).unwrap(),
            TtsConfig::load(&empty.tts, &home_dir).unwrap()
        );
        assert_eq!(
            SpeakerConfig::load(&defaults.speaker, &home_dir),
            SpeakerConfig::load(&empty.speaker, &home_dir)
        );
        assert_eq!(
            ListenConfig::load(&defaults, &home_dir).unwrap(),
            ListenConfig::load(&empty, &home_dir).unwrap()
        );
    }

    #[test]
    fn engine_names_match_what_the_engines_declare() {
        assert_eq!(backend::WHISPER, aria_stt::whisper::NAME);
        assert_eq!(backend::WHISPER_CANDLE, aria_stt::whisper_candle::NAME);
        assert_eq!(backend::PARAKEET, aria_stt::parakeet::NAME);
        assert_eq!(backend::MOONSHINE, aria_stt::moonshine::NAME);
        assert_eq!(backend::NEMOTRON, aria_stt::nemotron::NAME);
        #[cfg(not(target_os = "ios"))]
        assert_eq!(backend::PARAKEET_MLX, aria_stt::parakeet_mlx::NAME);
        assert_eq!(tts_backend::KOKORO, aria_tts::kokoro::NAME);
        assert_eq!(tts_backend::QWEN, aria_tts::qwen::NAME);
        assert_eq!(tts_backend::SYSTEM, aria_tts::system::NAME);
    }
}
