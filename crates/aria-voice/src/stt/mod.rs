//! Runtime engine selection over the speech-to-text contract in `aria-kit`.

use crate::config::SttConfig;

use aria_stt::EngineConfig;
use aria_stt::moonshine::MoonshineConfig;
use aria_stt::nemotron::NemotronConfig;
use aria_stt::parakeet::ParakeetConfig;
// MLX runs hand-written Metal kernels, so this backend is wired up only on
// Apple hardware.
#[cfg(target_os = "macos")]
use aria_stt::parakeet_mlx::ParakeetMlxConfig;
use aria_stt::whisper::WhisperConfig;
use aria_stt::whisper_candle::WhisperCandleConfig;

pub use aria_kit::backend::ModelError;
pub use aria_kit::transcription::{
    AudioInputKind, Cadence, ChunkStream, Emitted, Engine, Hypothesis, Language, LanguageCode,
    LanguageCodeError, Mode, Operation, RecognitionAudio, Recognizer, Stream, Streaming,
    StreamingEngine, Transcript, TranscriptSegment, TranscriptUpdate, Transcription,
    TranscriptionError, Whole,
};

impl TryFrom<SttConfig> for EngineConfig {
    type Error = TranscriptionError;

    fn try_from(config: SttConfig) -> Result<Self, TranscriptionError> {
        match config {
            SttConfig::Whisper { model } => Ok(Self::Whisper(WhisperConfig::new(model))),
            SttConfig::WhisperCandle { model_directory } => Ok(Self::WhisperCandle(
                WhisperCandleConfig::new(model_directory),
            )),
            SttConfig::Parakeet {
                model_directory,
                device,
            } => Ok(Self::Parakeet(ParakeetConfig::new(model_directory, device))),
            SttConfig::Moonshine { model_directory } => {
                Ok(Self::Moonshine(MoonshineConfig::new(model_directory)))
            }
            SttConfig::Nemotron { model_directory } => {
                Ok(Self::Nemotron(NemotronConfig::new(model_directory)))
            }
            #[cfg(target_os = "macos")]
            SttConfig::ParakeetMlx { model, interpreter } => {
                Ok(Self::ParakeetMlx(ParakeetMlxConfig { interpreter, model }))
            }
            #[cfg(not(target_os = "macos"))]
            SttConfig::ParakeetMlx { .. } => Err(TranscriptionError::UnsupportedBackend {
                backend: aria_settings::config::backend::PARAKEET_MLX,
            }),
        }
    }
}

/// Whether the engine decodes while audio arrives, as the engine declares.
pub(crate) fn streams(config: &SttConfig) -> bool {
    EngineConfig::try_from(config.clone()).is_ok_and(|engine| engine.stream().is_some())
}

/// Resolves the model a backend would load, without reading it or starting a
/// child.
pub fn check_model(config: &SttConfig) -> Result<String, TranscriptionError> {
    let checked = EngineConfig::try_from(config.clone())?.check()?;
    let checked = checked.display();
    Ok(match config {
        SttConfig::ParakeetMlx { model, .. } => {
            format!("{checked}, model {model} from its own cache")
        }
        SttConfig::Whisper { .. }
        | SttConfig::WhisperCandle { .. }
        | SttConfig::Parakeet { .. }
        | SttConfig::Moonshine { .. }
        | SttConfig::Nemotron { .. } => checked.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::TempDir;

    use crate::config::SttDevice;
    use aria_primitives::file_inspection::InspectionError;

    use super::*;

    #[test]
    fn the_settings_streaming_list_is_the_engines_that_declare_a_stream() {
        use aria_settings::config::backend;
        let models = Path::new("/models");
        let configs = [
            SttConfig::Whisper {
                model: models.join("whisper.bin"),
            },
            SttConfig::WhisperCandle {
                model_directory: models.to_path_buf(),
            },
            SttConfig::Parakeet {
                model_directory: models.to_path_buf(),
                device: SttDevice::Cpu,
            },
            SttConfig::Moonshine {
                model_directory: models.to_path_buf(),
            },
            SttConfig::Nemotron {
                model_directory: models.to_path_buf(),
            },
            #[cfg(target_os = "macos")]
            SttConfig::ParakeetMlx {
                model: "mlx-community/parakeet-tdt-0.6b-v3".to_owned(),
                interpreter: models.join("python3"),
            },
        ];
        let mut named = Vec::new();
        for config in configs {
            let engine = config.backend_name();
            assert_eq!(
                streams(&config),
                backend::STREAMING.contains(&engine),
                "{engine}"
            );
            named.push(engine);
        }
        #[cfg(target_os = "macos")]
        assert_eq!(named, backend::LOCAL, "every local engine is checked");
    }

    const SECURE_MODE: u32 = 0o644;

    fn model_directory() -> TempDir {
        let directory = TempDir::new().expect("test directory should be created");
        write_model_file(directory.path(), "vocab.txt", SECURE_MODE);
        directory
    }

    fn write_model_file(directory: &Path, name: &str, mode: u32) {
        let path = directory.join(name);
        fs::write(&path, []).expect("test model file should be written");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .expect("test model file permissions should be set");
    }

    #[test]
    fn each_backend_is_checked_against_its_own_layout() {
        let directory = model_directory();
        let models = directory.path().to_path_buf();

        let whisper = check_model(&SttConfig::Whisper {
            model: models.join("vocab.txt"),
        })
        .expect("a whisper model is one file");
        let parakeet = check_model(&SttConfig::Parakeet {
            model_directory: models.clone(),
            device: SttDevice::Cpu,
        })
        .expect("a parakeet model is a directory holding vocab.txt");
        for name in [
            "tokenizer.json",
            "encoder_model.onnx",
            "decoder_model_merged.onnx",
        ] {
            write_model_file(&models, name, SECURE_MODE);
        }
        let moonshine = check_model(&SttConfig::Moonshine {
            model_directory: models,
        })
        .expect("a moonshine model is a directory holding its graphs and tokenizer");

        assert!(whisper.ends_with("vocab.txt"));
        assert!(!parakeet.ends_with("vocab.txt"));
        assert_eq!(parakeet, moonshine);
    }

    /// parakeet-mlx names two things: an interpreter that must run, and a
    /// repository the child fetches for itself.
    #[cfg(target_os = "macos")]
    #[test]
    fn parakeet_mlx_checks_the_interpreter_and_leaves_the_repository_alone() {
        const EXECUTABLE_MODE: u32 = 0o755;
        const MODEL: &str = "mlx-community/parakeet-tdt-0.6b-v3";
        let directory = model_directory();
        write_model_file(directory.path(), "python3", EXECUTABLE_MODE);
        let mlx = |interpreter: &str| SttConfig::ParakeetMlx {
            model: MODEL.to_owned(),
            interpreter: directory.path().join(interpreter),
        };

        let checked = check_model(&mlx("python3")).expect("an executable interpreter is usable");
        let unusable = check_model(&mlx("vocab.txt")).expect_err("a file that cannot run is not");

        assert!(checked.contains("python3"));
        assert!(checked.contains(MODEL));
        assert!(matches!(unusable, TranscriptionError::Load { .. }));
    }

    #[test]
    fn a_whisper_model_that_is_a_directory_is_refused() {
        let directory = model_directory();

        let error = check_model(&SttConfig::Whisper {
            model: directory.path().to_path_buf(),
        })
        .expect_err("whisper needs a file, not a directory");

        assert!(matches!(
            error,
            TranscriptionError::Model {
                backend: aria_settings::config::backend::WHISPER,
                source: ModelError::Path(InspectionError::NotRegularFile { .. }),
            }
        ));
    }

    #[test]
    fn a_parakeet_directory_without_a_vocabulary_is_refused() {
        let directory = TempDir::new().expect("test directory should be created");

        let error = check_model(&SttConfig::Parakeet {
            model_directory: directory.path().to_path_buf(),
            device: SttDevice::Cpu,
        })
        .expect_err("parakeet needs a vocabulary");

        assert!(matches!(
            error,
            TranscriptionError::Model {
                source: ModelError::Missing {
                    file: "vocab.txt",
                    ..
                },
                ..
            }
        ));
    }
}
