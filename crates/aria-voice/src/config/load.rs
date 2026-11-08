//! Resolution of checked settings and defaults.

use std::path::{Path, PathBuf};

use aria_kit::audio::ReplayPace;

use super::{
    CaptureConfig, DictationConfig, PairingId, SpeakerConfig, SpeechDefaults,
    SystemTranscriptionConfig, SystemTranscriptionEngine, TranscriptionBackend,
    TranscriptionConfig, TranscriptionDevice, TtsBackend, TtsConfig, TtsFallback, TtsModel,
};
use aria_settings::config::{ConfigError, backend, model_file, model_folder, model_location};
use aria_settings::{
    CaptureChoice, CheckedSettings, DeviceName, InvalidValue, Key, LocalStt, PaceName, RunsOn,
    Speaker, SttChoice, TtsEngine, TtsFallbackName,
};

/// A HuggingFace repo id, resolved by the sidecar against its own cache.
const DEFAULT_PARAKEET_MLX_MODEL: &str = "mlx-community/parakeet-tdt-0.6b-v3";

const DEFAULT_SYSTEM_TRANSCRIPTION_TRANSCRIPTS_DIR: &str = ".aria/transcripts";
const DEFAULT_SPEAKERS_DIR: &str = ".aria/speakers";
/// The engine that a setting makes system transcription run: its own backend, or the stt engine
/// when that one streams. `None` when no setting chooses one.
fn chosen_system_transcription_engine(
    settings: &CheckedSettings,
    home: &Path,
) -> Option<TranscriptionConfig> {
    match settings.choices().system_transcription {
        Some(engine) => Some(local(
            engine,
            settings.system_transcription.model.as_deref(),
            settings.system_transcription.device,
            home,
        )),
        None => match settings.choices().stt_runs_on {
            RunsOn::Local => Some(TranscriptionConfig::load(settings, home))
                .filter(crate::transcription::streams),
            RunsOn::Server => None,
        },
    }
}

impl SystemTranscriptionConfig {
    /// Parakeet stands in when no setting chooses an engine system transcription can run, so
    /// changing dictation's model never turns system transcription off.
    pub fn load(settings: &CheckedSettings, home: &Path) -> Result<Self, ConfigError> {
        let transcription = chosen_system_transcription_engine(settings, home)
            .unwrap_or_else(|| local(LocalStt::Parakeet, None, None, home));
        Ok(Self {
            transcription: SystemTranscriptionEngine::new(transcription)?,
            transcripts_dir: settings
                .system_transcription
                .transcripts_dir
                .clone()
                .unwrap_or_else(|| home.join(DEFAULT_SYSTEM_TRANSCRIPTION_TRANSCRIPTS_DIR)),
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
    pub fn load(settings: &CheckedSettings, home: &Path) -> Self {
        let tts = &settings.tts;
        let backend = match settings.choices().tts {
            TtsEngine::Kokoro => TtsBackend::Model(TtsModel::Kokoro {
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
            TtsEngine::Qwen => TtsBackend::Model(TtsModel::Qwen {
                model_dir: model_location(tts.qwen.model.as_deref(), model_folder::QWEN, home),
                tokenizer_dir: model_location(
                    tts.qwen.tokenizer.as_deref(),
                    model_folder::QWEN_TOKENIZER,
                    home,
                ),
            }),
            TtsEngine::System => TtsBackend::System,
        };
        let fallback = tts_fallback(tts.fallback);
        Self { backend, fallback }
    }
}

impl SpeechDefaults {
    pub fn load(settings: &CheckedSettings, engine: TtsEngine) -> Self {
        let choices = settings.choices();
        let voice = match engine {
            TtsEngine::Kokoro => &choices.voices.kokoro,
            TtsEngine::Qwen => &choices.voices.qwen,
            TtsEngine::System => &choices.voices.system,
        };
        Self {
            voice: voice.clone(),
            rate: choices.rate,
        }
    }
}

/// The settings table that makes Aria load a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Stt,
    SystemTranscription,
    Tts,
    Speaker,
}

impl std::fmt::Display for Section {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Stt => "stt",
            Self::SystemTranscription => "transcription",
            Self::Tts => "tts",
            Self::Speaker => "speaker",
        })
    }
}

/// A model file or folder Aria loads. `engine` is `None` for the speaker
/// models, which have no engine choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModel {
    pub section: Section,
    pub engine: Option<&'static str>,
    pub path: PathBuf,
}

/// The models `settings` make Aria load. The local engine counts while the
/// server transcribes, since going back loads it.
pub fn loaded_models(settings: &CheckedSettings, home: &Path) -> Vec<LoadedModel> {
    let mut models = Vec::new();
    let mut add = |section, engine, path| {
        models.push(LoadedModel {
            section,
            engine,
            path,
        });
    };
    let engine = TranscriptionConfig::load(settings, home);
    let name = engine.backend_name();
    if let Some(path) = engine.model_path() {
        add(Section::Stt, Some(name), path);
    }
    // Fallback Parakeet isn't any setting's choice, so it stays deletable.
    // System transcription then reports it missing when a session starts.
    if let Some(engine) = chosen_system_transcription_engine(settings, home)
        && let Ok(system_transcription) = SystemTranscriptionEngine::new(engine)
        && let Some(path) = system_transcription.config().clone().model_path()
    {
        add(
            Section::SystemTranscription,
            Some(system_transcription.backend_name()),
            path,
        );
    }
    let tts = TtsConfig::load(settings, home);
    let engine = Some(tts.backend.name());
    match tts.backend {
        TtsBackend::Model(TtsModel::Kokoro { model, voices }) => {
            add(Section::Tts, engine, model);
            add(Section::Tts, engine, voices);
        }
        TtsBackend::Model(TtsModel::Qwen {
            model_dir,
            tokenizer_dir,
        }) => {
            add(Section::Tts, engine, model_dir);
            add(Section::Tts, engine, tokenizer_dir);
        }
        TtsBackend::System => {}
    }
    let speaker = SpeakerConfig::load(&settings.speaker, home);
    add(Section::Speaker, None, speaker.model);
    add(Section::Speaker, None, speaker.segmentation_model);
    models
}

fn tts_fallback(file_value: Option<TtsFallbackName>) -> TtsFallback {
    match file_value {
        None | Some(TtsFallbackName::None) => TtsFallback::None,
        Some(TtsFallbackName::System) => TtsFallback::System,
    }
}

impl DictationConfig {
    pub fn load(settings: &CheckedSettings, home: &Path, pairing: Option<PairingId>) -> Self {
        let choices = settings.choices();
        Self {
            capture: match &choices.capture {
                CaptureChoice::Microphone => CaptureConfig::Microphone,
                CaptureChoice::WavFile { path } => CaptureConfig::WavFile {
                    path: path.clone(),
                    pace: replay_pace(settings.capture.pace),
                },
            },
            transcription: TranscriptionBackend::load(settings, home, pairing),
            dictation_timeouts: choices.timeouts,
            cleanup_llm: choices.cleanup.clone(),
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
    pub fn load(settings: &CheckedSettings, home: &Path, pairing: Option<PairingId>) -> Self {
        match settings.choices().stt_runs_on {
            RunsOn::Server => Self::Server(pairing),
            RunsOn::Local => Self::Local(TranscriptionConfig::load(settings, home)),
        }
    }
}

impl TranscriptionConfig {
    /// The local engine, which the settings keep while the server transcribes.
    pub fn load(settings: &CheckedSettings, home: &Path) -> Self {
        let stt = &settings.stt;
        match &settings.choices().stt {
            SttChoice::ParakeetMlx { interpreter } => Self::ParakeetMlx {
                model: stt
                    .model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_PARAKEET_MLX_MODEL.to_owned()),
                interpreter: interpreter.clone(),
            },
            SttChoice::Local(engine) => local(
                *engine,
                stt.model.as_deref().map(Path::new),
                stt.device,
                home,
            ),
        }
    }
}

/// The defaults of an engine a caller names, not the file.
pub(super) fn transcription_defaults(
    backend: &str,
    home: &Path,
) -> Result<TranscriptionConfig, ConfigError> {
    match LocalStt::named(backend) {
        Some(engine) => Ok(local(engine, None, None, home)),
        None if backend == backend::PARAKEET_MLX => Err(ConfigError::Invalid {
            key: Key::SttInterpreter,
            source: InvalidValue::Required {
                backend: backend::PARAKEET_MLX.to_owned(),
            },
        }),
        None => Err(ConfigError::Invalid {
            key: Key::SttBackend,
            source: InvalidValue::Unsupported {
                value: backend.to_owned(),
                supported: backend::LOCAL.join(", "),
            },
        }),
    }
}

fn local(
    engine: LocalStt,
    model: Option<&Path>,
    device: Option<DeviceName>,
    home: &Path,
) -> TranscriptionConfig {
    match engine {
        LocalStt::Whisper => TranscriptionConfig::Whisper {
            model: model_location(
                model,
                model_file(model_folder::WHISPER, &aria_stt::whisper::MODEL[0]),
                home,
            ),
        },
        LocalStt::WhisperCandle => TranscriptionConfig::WhisperCandle {
            model_directory: model_location(model, model_folder::WHISPER_CANDLE, home),
        },
        LocalStt::Parakeet => TranscriptionConfig::Parakeet {
            model_directory: model_location(model, model_folder::PARAKEET, home),
            device: transcription_device(device),
        },
        LocalStt::Moonshine => TranscriptionConfig::Moonshine {
            model_directory: model_location(model, model_folder::MOONSHINE, home),
        },
        LocalStt::Nemotron => TranscriptionConfig::Nemotron {
            model_directory: model_location(model, model_folder::NEMOTRON, home),
        },
    }
}

fn transcription_device(device: Option<DeviceName>) -> TranscriptionDevice {
    match device {
        None | Some(DeviceName::Cpu) => TranscriptionDevice::Cpu,
        Some(DeviceName::Gpu) => TranscriptionDevice::Gpu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_kit::synthesis::{SpeakingRate, Voice};
    use aria_settings::config::tts_backend;
    use aria_settings::testing::{file, test_home};
    use aria_settings::{LlmCleanupTarget, Loaded};

    #[test]
    fn the_server_transcribes_and_the_local_model_stays_in_use() {
        let file = file(
            "[stt]\nbackend = \"whisper\"\nmodel = \"/models/whisper\"\nruns_on = \"server\"\n",
        );

        let pairing = PairingId::new([7; 32]);
        assert_eq!(
            TranscriptionBackend::load(&file, &test_home(), Some(pairing.clone())),
            TranscriptionBackend::Server(Some(pairing))
        );
        let models = loaded_models(&file, Path::new("/missing-home"));
        assert!(models.iter().any(|model| model.section == Section::Stt
            && model.engine == Some(backend::WHISPER)
            && model.path == Path::new("/models/whisper")));
    }

    #[test]
    fn system_transcription_runs_locally_while_dictation_uses_a_server() {
        let server = "[stt]\nbackend = \"moonshine\"\nruns_on = \"server\"\n";
        let engine = |text: &str| {
            SystemTranscriptionConfig::load(&file(text), &test_home())
                .unwrap()
                .transcription
                .backend_name()
        };
        assert_eq!(engine(server), backend::PARAKEET);
        let moonshine = format!("{server}[transcription]\nbackend = \"moonshine\"\n");
        assert_eq!(engine(&moonshine), backend::MOONSHINE);
    }

    #[test]
    fn the_cleanup_model_loads_from_its_table_alone() {
        let home_dir = test_home();
        assert!(
            DictationConfig::load(&file(""), &home_dir, None)
                .cleanup_llm
                .is_none()
        );
        let llm = "[dictation.cleanup.llm]\nendpoint = \"http://127.0.0.1:8080/v1/chat/completions\"\nmodel = \"local-model\"\n";
        let config = DictationConfig::load(&file(llm), &home_dir, None);
        assert_eq!(
            config.cleanup_llm,
            Some(
                LlmCleanupTarget::new("http://127.0.0.1:8080/v1/chat/completions", "local-model")
                    .unwrap()
            )
        );
    }

    #[test]
    fn capture_defaults_to_the_microphone() {
        let capture = DictationConfig::load(&file(""), &test_home(), None).capture;
        assert_eq!(capture, CaptureConfig::Microphone);
    }

    #[test]
    fn capture_resolves_a_wav_file_backend() {
        let file = file("[capture]\nbackend = \"wav_file\"\npath = \"~/fixture.wav\"\n");
        assert_eq!(
            DictationConfig::load(&file, &test_home(), None).capture,
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
            DictationConfig::load(&file, &test_home(), None).capture,
            CaptureConfig::WavFile {
                pace: ReplayPace::Realtime,
                ..
            }
        ));
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
            let transcription = transcription_defaults(backend, Path::new("/home/user"))
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
            TranscriptionBackend::load(&file, &test_home(), None),
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
            TranscriptionBackend::load(&file, &test_home(), None),
            TranscriptionBackend::Local(TranscriptionConfig::ParakeetMlx {
                model: "mlx-community/whisper".to_owned(),
                interpreter: PathBuf::from("/home/user/py"),
            })
        );
    }

    #[test]
    fn defaults_for_an_unknown_engine_are_refused() {
        let error = transcription_defaults("vosk", Path::new("/home/user"))
            .expect_err("an unknown backend must be rejected");
        // A retry never names the server, so the list offers only local engines.
        assert!(matches!(
            error,
            ConfigError::Invalid {
                key: Key::SttBackend,
                source: InvalidValue::Unsupported { ref supported, .. },
            } if !supported.contains(backend::SERVER) && supported.contains(backend::PARAKEET_MLX)
        ));
    }

    #[test]
    fn tts_defaults_to_the_system_backend_at_the_natural_pace() {
        let defaults = file("");
        let tts = TtsConfig::load(&defaults, &test_home());

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

        let saved = SpeechDefaults::load(&file, TtsEngine::System);

        assert_eq!(saved.voice, Some(Voice::new("Samantha").expect("valid")));
        assert_eq!(saved.rate, SpeakingRate::new(1.5).expect("valid"));
    }

    #[test]
    fn each_engine_keeps_its_own_voice() {
        let voices = "[tts.system]\nvoice = \"Samantha\"\n[tts.kokoro]\nvoice = \"af_heart\"\n";
        let voice = |engine| SpeechDefaults::load(&file(voices), engine).voice;

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

        let tts = TtsConfig::load(&file, &test_home());

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

        let tts = TtsConfig::load(&file, &test_home());

        assert!(matches!(
            tts.backend,
            TtsBackend::Model(TtsModel::Qwen { .. })
        ));
    }

    #[test]
    fn tts_resolves_qwen_with_its_own_directory_defaults() {
        let qwen = file("[tts]\nbackend = \"qwen\"\n");
        let tts = TtsConfig::load(&qwen, &test_home());

        assert_eq!(
            tts.backend,
            TtsBackend::Model(TtsModel::Qwen {
                model_dir: PathBuf::from("/home/user/.aria/models/qwen/qwen3-tts-0.6b-customvoice"),
                tokenizer_dir: PathBuf::from("/home/user/.aria/models/qwen/qwen3-tts-tokenizer"),
            })
        );
    }

    #[test]
    fn system_transcription_inherits_the_transcription_model() {
        let home = Path::new("/home/user");
        let loaded = loaded_models(&file("[stt]\nbackend = \"parakeet\"\n"), home);
        let models = home.join(".aria/models");

        assert_eq!(
            loaded,
            [
                LoadedModel {
                    section: Section::Stt,
                    engine: Some(backend::PARAKEET),
                    path: models.join("parakeet/parakeet-tdt-0.6b-v3"),
                },
                LoadedModel {
                    section: Section::SystemTranscription,
                    engine: Some(backend::PARAKEET),
                    path: models.join("parakeet/parakeet-tdt-0.6b-v3"),
                },
                LoadedModel {
                    section: Section::Speaker,
                    engine: None,
                    path: models.join("wespeaker/en/model.onnx"),
                },
                LoadedModel {
                    section: Section::Speaker,
                    engine: None,
                    path: models.join("pyannote/segmentation-3.0.onnx"),
                },
            ],
            "System transcription inherits the dictation model"
        );
    }

    #[test]
    fn a_configured_model_path_is_the_one_loaded() {
        let file = file("[stt]\nbackend = \"parakeet\"\nmodel = \"~/tdt\"\n");

        let loaded = loaded_models(&file, Path::new("/home/user"));

        assert!(loaded.iter().any(
            |model| model.section == Section::Stt && model.path == Path::new("/home/user/tdt")
        ));
        assert!(
            loaded
                .iter()
                .any(|model| model.section == Section::SystemTranscription
                    && model.path == Path::new("/home/user/tdt"))
        );
    }

    #[test]
    fn mac_audio_admits_only_an_engine_that_streams() {
        let whisper = TranscriptionConfig::Whisper {
            model: PathBuf::from("/models/whisper.bin"),
        };
        let tiny = TranscriptionConfig::WhisperCandle {
            model_directory: PathBuf::from("/models/tiny"),
        };

        assert!(SystemTranscriptionEngine::new(whisper).is_ok());
        assert!(matches!(
            SystemTranscriptionEngine::new(tiny),
            Err(ConfigError::SystemTranscriptionUnusableEngine(_))
        ));
    }

    #[test]
    fn system_transcription_inherits_a_streaming_transcription_backend() {
        let system_transcription = SystemTranscriptionConfig::load(
            &file("[stt]\nbackend = \"moonshine\"\n"),
            &test_home(),
        )
        .expect("moonshine must be inherited");

        assert_eq!(
            system_transcription,
            SystemTranscriptionConfig {
                transcription: SystemTranscriptionEngine::new(TranscriptionConfig::Moonshine {
                    model_directory: PathBuf::from("/home/user/.aria/models/moonshine/base"),
                })
                .unwrap(),
                transcripts_dir: PathBuf::from("/home/user/.aria/transcripts"),
            }
        );
    }

    #[test]
    fn mac_audio_runs_the_dictation_model_it_can_and_parakeet_otherwise() {
        for &engine in backend::LOCAL {
            let interpreter = if engine == backend::PARAKEET_MLX {
                "interpreter = \"/usr/bin/python3\"\n"
            } else {
                ""
            };
            let file = file(&format!("[stt]\nbackend = \"{engine}\"\n{interpreter}"));
            let system_transcription = SystemTranscriptionConfig::load(&file, &test_home())
                .expect("System transcription always loads");
            let ran = system_transcription.transcription.backend_name();
            let expected = if backend::STREAMING.contains(&engine) {
                engine
            } else {
                backend::PARAKEET
            };
            assert_eq!(ran, expected, "{engine}");
        }
    }

    #[test]
    fn an_own_system_transcription_backend_outranks_the_stt_table() {
        let file = file(
            "[stt]\nbackend = \"whisper\"\n[transcription]\nbackend = \"parakeet\"\nmodel = \"~/models/tdt\"\n",
        );

        let system_transcription = SystemTranscriptionConfig::load(&file, &test_home())
            .expect("the transcription table must win");

        assert_eq!(
            system_transcription.transcription,
            SystemTranscriptionEngine::new(TranscriptionConfig::Parakeet {
                model_directory: PathBuf::from("/home/user/models/tdt"),
                device: TranscriptionDevice::Cpu,
            })
            .unwrap()
        );
    }

    #[test]
    fn the_defaults_load_and_match_what_code_does_without_them() {
        let home = Path::new("/home/user");
        let defaults = Loaded::parse(&aria_settings::testing::defaults_text(), home)
            .expect("every default is valid")
            .into_settings();
        let empty = file("");
        let home_dir = home.to_path_buf();

        assert_eq!(
            DictationConfig::load(&defaults, &home_dir, None),
            DictationConfig::load(&empty, &home_dir, None)
        );
        assert_eq!(
            TtsConfig::load(&defaults, &home_dir),
            TtsConfig::load(&empty, &home_dir)
        );
        assert_eq!(
            SpeakerConfig::load(&defaults.speaker, &home_dir),
            SpeakerConfig::load(&empty.speaker, &home_dir)
        );
        assert_eq!(
            SystemTranscriptionConfig::load(&defaults, &home_dir).unwrap(),
            SystemTranscriptionConfig::load(&empty, &home_dir).unwrap()
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
