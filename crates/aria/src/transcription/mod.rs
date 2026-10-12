//! Runtime engine selection over the speech-to-text contract in `aria-kit`.

use std::path::PathBuf;

use crate::config::TranscriptionConfig;

use aria_stt::moonshine::{Moonshine, MoonshineConfig};
use aria_stt::nemotron::{Nemotron, NemotronConfig};
use aria_stt::parakeet::{Parakeet, ParakeetConfig};
// MLX runs hand-written Metal kernels, so this backend is wired up only on
// Apple hardware.
#[cfg(target_os = "macos")]
use aria_stt::parakeet_mlx::{ParakeetMlx, ParakeetMlxConfig};
use aria_stt::whisper::{Whisper, WhisperConfig};
use aria_stt::whisper_candle::{WhisperCandle, WhisperCandleConfig};

pub use aria_kit::backend::ModelError;
pub use aria_kit::transcription::{
    AudioInputKind, Cadence, ChunkStream, Emitted, Engine, Hypothesis, Language, LanguageCode,
    LanguageCodeError, Mode, Operation, Stream, Streaming, StreamingEngine, Transcriber,
    Transcript, TranscriptSegment, TranscriptUpdate, Transcription, TranscriptionAudio,
    TranscriptionError, Whole,
};

/// Resolves the model a backend would load, without reading it or starting a
/// child.
pub fn check_model(config: &TranscriptionConfig) -> Result<String, TranscriptionError> {
    match config {
        TranscriptionConfig::Whisper { model } => check::<Whisper>(WhisperConfig::new(model)),
        TranscriptionConfig::WhisperCandle { model_directory } => {
            check::<WhisperCandle>(WhisperCandleConfig::new(model_directory))
        }
        TranscriptionConfig::Parakeet {
            model_directory,
            device,
        } => check::<Parakeet>(ParakeetConfig::new(model_directory, *device)),
        TranscriptionConfig::Moonshine { model_directory } => {
            check::<Moonshine>(MoonshineConfig::new(model_directory))
        }
        TranscriptionConfig::Nemotron { model_directory } => {
            check::<Nemotron>(NemotronConfig::new(model_directory))
        }
        #[cfg(target_os = "macos")]
        TranscriptionConfig::ParakeetMlx { model, interpreter } => {
            let config = ParakeetMlxConfig {
                interpreter: interpreter.clone(),
                model: model.clone(),
            };
            ParakeetMlx::check_from(&config).map(|interpreter| {
                format!(
                    "{}, model {model} from its own cache",
                    display_path(interpreter)
                )
            })
        }
        #[cfg(not(target_os = "macos"))]
        TranscriptionConfig::ParakeetMlx { .. } => Err(TranscriptionError::UnsupportedBackend {
            backend: crate::config::backend::PARAKEET_MLX,
        }),
    }
}

fn check<E: Engine>(config: E::Config) -> Result<String, TranscriptionError> {
    E::check_from(&config).map(display_path)
}

fn display_path(path: PathBuf) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::TempDir;

    use crate::config::TranscriptionDevice;
    use crate::primitives::file_inspection::InspectionError;

    use super::*;

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

        let whisper = check_model(&TranscriptionConfig::Whisper {
            model: models.join("vocab.txt"),
        })
        .expect("a whisper model is one file");
        let parakeet = check_model(&TranscriptionConfig::Parakeet {
            model_directory: models.clone(),
            device: TranscriptionDevice::Cpu,
        })
        .expect("a parakeet model is a directory holding vocab.txt");
        for name in [
            "tokenizer.json",
            "encoder_model.onnx",
            "decoder_model_merged.onnx",
        ] {
            write_model_file(&models, name, SECURE_MODE);
        }
        let moonshine = check_model(&TranscriptionConfig::Moonshine {
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
        let mlx = |interpreter: &str| TranscriptionConfig::ParakeetMlx {
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

        let error = check_model(&TranscriptionConfig::Whisper {
            model: directory.path().to_path_buf(),
        })
        .expect_err("whisper needs a file, not a directory");

        assert!(matches!(
            error,
            TranscriptionError::Model {
                backend: crate::config::backend::WHISPER,
                source: ModelError::Path(InspectionError::NotRegularFile { .. }),
            }
        ));
    }

    #[test]
    fn a_parakeet_directory_without_a_vocabulary_is_refused() {
        let directory = TempDir::new().expect("test directory should be created");

        let error = check_model(&TranscriptionConfig::Parakeet {
            model_directory: directory.path().to_path_buf(),
            device: TranscriptionDevice::Cpu,
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
