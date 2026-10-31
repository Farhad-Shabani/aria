//! The choices setup offers, and what this machine can run.

use crate::config::{backend, tts_backend};

use super::assets::{self, Asset};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Ready,
    /// Pickable, but setup cannot make it work by itself.
    Missing {
        fix: String,
    },
    /// Not pickable on this host.
    Unsupported {
        why: &'static str,
    },
}

/// One engine as setup offers it: what settings.toml calls it, what a person
/// sees, and whether this host can run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub key: &'static str,
    pub label: &'static str,
    pub availability: Availability,
    /// Exactly one choice per family. The configured one wins, even with
    /// its model missing: that is a fix to show, not a reason to choose for
    /// the user. Otherwise the one unset settings run, if ready; else the
    /// first that is ready, else the first listed.
    pub default: bool,
    pub model: ModelSource,
}

pub(super) fn choices<B: Backend>(current: Option<&str>, host: &Host) -> Vec<Choice> {
    let mut choices: Vec<Choice> = B::ALL
        .iter()
        .map(|backend| Choice {
            key: backend.key(),
            label: backend.label(),
            availability: backend.availability(host),
            default: false,
            model: backend.model(),
        })
        .collect();
    let default = choices
        .iter()
        .position(|choice| {
            Some(choice.key) == current
                && !matches!(choice.availability, Availability::Unsupported { .. })
        })
        .or_else(|| {
            choices.iter().position(|choice| {
                choice.key == B::DEFAULT && choice.availability == Availability::Ready
            })
        })
        .or_else(|| {
            choices
                .iter()
                .position(|choice| choice.availability == Availability::Ready)
        })
        .unwrap_or(0);
    choices[default].default = true;
    choices
}

/// Applying the server choice writes the paired server's address, so a
/// pairing alone makes it ready.
pub(super) fn stt_choices(file: &crate::settings::Stt, paired: bool, host: &Host) -> Vec<Choice> {
    let mut choices = choices::<Stt>(file.backend.as_deref(), host);
    if paired
        && let Some(server) = choices
            .iter_mut()
            .find(|choice| choice.key == backend::SERVER)
    {
        server.availability = Availability::Ready;
    }
    choices
}

/// What is known about this machine before any question is asked.
pub(super) struct Host {
    pub(super) macos: bool,
    pub(super) system_voice: Availability,
}

impl Host {
    pub(super) fn probe() -> Self {
        Self {
            macos: cfg!(target_os = "macos"),
            system_voice: match aria_tts::system::SystemVoice::new() {
                Ok(_) => Availability::Ready,
                Err(error) => Availability::Missing {
                    fix: error.to_string(),
                },
            },
        }
    }
}

/// Where an engine's model comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Downloads(&'static [Asset]),
    /// The engine fetches its model itself, so setup cannot tell its size.
    FetchedOnFirstUse,
    BuiltIn,
    /// A paired inference server holds the model.
    Remote,
}

impl ModelSource {
    pub(super) fn assets(self) -> &'static [Asset] {
        match self {
            Self::Downloads(assets) => assets,
            Self::FetchedOnFirstUse | Self::BuiltIn | Self::Remote => &[],
        }
    }
}

/// One family of interchangeable engines the user picks from.
pub(super) trait Backend: Copy + Eq + 'static {
    const TITLE: &'static str;
    const ALL: &'static [Self];
    const DEFAULT: &'static str;
    /// The value written to settings.toml.
    fn key(self) -> &'static str;
    fn label(self) -> &'static str;
    fn availability(self, host: &Host) -> Availability;
    fn model(self) -> ModelSource;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stt {
    Whisper,
    WhisperCandle,
    Parakeet,
    Moonshine,
    Nemotron,
    ParakeetMlx,
    Server,
}

impl Backend for Stt {
    const TITLE: &'static str = "Speech to text";
    const ALL: &'static [Self] = &[
        Self::Parakeet,
        Self::Whisper,
        Self::WhisperCandle,
        Self::Moonshine,
        Self::Nemotron,
        Self::ParakeetMlx,
        Self::Server,
    ];
    const DEFAULT: &'static str = backend::DEFAULT;

    fn key(self) -> &'static str {
        match self {
            Self::Whisper => backend::WHISPER,
            Self::WhisperCandle => backend::WHISPER_CANDLE,
            Self::Parakeet => backend::PARAKEET,
            Self::Moonshine => backend::MOONSHINE,
            Self::Nemotron => backend::NEMOTRON,
            Self::ParakeetMlx => backend::PARAKEET_MLX,
            Self::Server => backend::SERVER,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Whisper => "Whisper large-v3-turbo (whisper.cpp)",
            Self::WhisperCandle => "Whisper (candle)",
            Self::Parakeet => "Parakeet TDT 0.6B",
            Self::Moonshine => "Moonshine base",
            Self::Nemotron => "Nemotron streaming 0.6B",
            Self::ParakeetMlx => "Parakeet TDT 0.6B (MLX)",
            Self::Server => "Paired inference server",
        }
    }

    fn availability(self, host: &Host) -> Availability {
        match self {
            Self::Whisper
            | Self::WhisperCandle
            | Self::Parakeet
            | Self::Moonshine
            | Self::Nemotron => Availability::Ready,
            Self::ParakeetMlx if !host.macos => Availability::Unsupported {
                why: "needs macOS (MLX)",
            },
            Self::ParakeetMlx => Availability::Missing {
                fix: "set stt.interpreter to a Python with parakeet-mlx".to_owned(),
            },
            Self::Server => Availability::Missing {
                fix: "pair an inference server first".to_owned(),
            },
        }
    }

    fn model(self) -> ModelSource {
        match self {
            Self::Whisper => ModelSource::Downloads(&[assets::WHISPER]),
            Self::WhisperCandle => ModelSource::Downloads(&[assets::WHISPER_CANDLE]),
            Self::Parakeet => ModelSource::Downloads(&[assets::PARAKEET]),
            Self::Moonshine => ModelSource::Downloads(&[assets::MOONSHINE]),
            Self::Nemotron => ModelSource::Downloads(&[assets::NEMOTRON]),
            // The sidecar downloads the model into its own Hugging Face cache.
            Self::ParakeetMlx => ModelSource::FetchedOnFirstUse,
            Self::Server => ModelSource::Remote,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tts {
    System,
    Kokoro,
    Qwen,
}

impl Backend for Tts {
    const TITLE: &'static str = "Text to speech";
    const ALL: &'static [Self] = &[Self::System, Self::Kokoro, Self::Qwen];
    const DEFAULT: &'static str = tts_backend::DEFAULT;

    fn key(self) -> &'static str {
        match self {
            Self::System => tts_backend::SYSTEM,
            Self::Kokoro => tts_backend::KOKORO,
            Self::Qwen => tts_backend::QWEN,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::System => "System voice (say / espeak)",
            Self::Kokoro => "Kokoro",
            Self::Qwen => "Qwen3 TTS",
        }
    }

    fn availability(self, host: &Host) -> Availability {
        match self {
            Self::System => host.system_voice.clone(),
            Self::Kokoro | Self::Qwen => Availability::Ready,
        }
    }

    fn model(self) -> ModelSource {
        match self {
            Self::System => ModelSource::BuiltIn,
            Self::Kokoro => ModelSource::Downloads(&[assets::KOKORO]),
            Self::Qwen => ModelSource::Downloads(&[assets::QWEN]),
        }
    }
}

#[cfg(test)]
mod tests {
    use aria_kit::synthesis::Engine as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::config::{self, DictationConfig, TtsBackend, TtsConfig, TtsModel};
    use crate::primitives::error_chain;
    use crate::settings::SettingsFile;

    #[test]
    fn server_is_ready_once_a_server_is_paired() {
        let host = Host {
            macos: true,
            system_voice: Availability::Ready,
        };
        let file = SettingsFile::default();
        let available = |paired: bool| {
            stt_choices(&file.stt, paired, &host)
                .into_iter()
                .find(|choice| choice.key == backend::SERVER)
                .unwrap()
                .availability
        };
        assert!(matches!(available(false), Availability::Missing { .. }));
        assert_eq!(available(true), Availability::Ready);
    }

    /// Setup must download each model to the path the daemon loads it from.
    #[test]
    fn every_model_lands_where_the_daemon_loads_it() {
        let home = Path::new("/home/someone");
        let models_dir = config::models_dir(home);
        let engines = Stt::ALL
            .iter()
            .map(|stt| ("stt", stt.key(), stt.model().assets()))
            .chain(
                Tts::ALL
                    .iter()
                    .map(|tts| ("tts", tts.key(), tts.model().assets())),
            );
        for (section, key, assets) in engines {
            let required = match key {
                backend::SERVER => "server = \"https://example.test\"\n",
                backend::PARAKEET_MLX => "interpreter = \"/usr/bin/python3\"\n",
                _ => "",
            };
            let file: SettingsFile =
                toml_edit::de::from_str(&format!("[{section}]\nbackend = \"{key}\"\n{required}"))
                    .unwrap();
            let loaded: Vec<_> = config::loaded_models(&file, home)
                .unwrap()
                .into_iter()
                .filter(|model| model.section == section)
                .collect();
            assert_eq!(assets.is_empty(), loaded.is_empty(), "{key}");
            for model in loaded {
                let lands = assets
                    .iter()
                    .any(|asset| assets::holds(&models_dir, asset, &model.path));
                assert!(
                    lands,
                    "{key} downloads nothing into {}",
                    model.path.display()
                );
            }
        }
    }

    /// A home whose settings pick `backend` in `section` and whose models
    /// directory holds every file setup downloads for it.
    fn home_with_downloads(section: &str, backend: &str, assets: &[Asset]) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let aria = home.path().join(".aria");
        fs::create_dir_all(&aria).unwrap();
        fs::write(
            aria.join("settings.toml"),
            format!("[{section}]\nbackend = \"{backend}\"\n"),
        )
        .unwrap();
        let models_dir = config::models_dir(home.path());
        for (relative, _) in assets.iter().flat_map(Asset::files) {
            // Engine checks refuse anything group- or world-writable, which
            // a loose umask would otherwise leave behind.
            let file = models_dir.join(relative);
            let folder = file.parent().unwrap();
            fs::create_dir_all(folder).unwrap();
            fs::set_permissions(folder, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(&file, b"").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        home
    }

    #[test]
    fn a_fresh_download_passes_the_engine_check() {
        for stt in Stt::ALL
            .iter()
            .filter(|stt| !stt.model().assets().is_empty())
        {
            let home = home_with_downloads("stt", stt.key(), stt.model().assets());
            let home_dir = home.path().to_path_buf();
            let crate::config::TranscriptionBackend::Local(transcription) =
                DictationConfig::load(&crate::settings::load(&home_dir).unwrap(), &home_dir)
                    .unwrap()
                    .transcription
            else {
                panic!("{} is a local engine", stt.key());
            };
            if let Err(error) = crate::transcription::check_model(&transcription) {
                panic!("{}: {}", stt.key(), error_chain(&error));
            }
        }
        for tts in Tts::ALL
            .iter()
            .filter(|tts| !tts.model().assets().is_empty())
        {
            let home = home_with_downloads("tts", tts.key(), tts.model().assets());
            let home_dir = home.path().to_path_buf();
            let checked =
                match TtsConfig::load(&crate::settings::load(&home_dir).unwrap().tts, &home_dir)
                    .unwrap()
                    .backend
                {
                    TtsBackend::Model(TtsModel::Kokoro { model, voices }) => {
                        aria_tts::kokoro::Kokoro::check_from(&aria_tts::kokoro::Config {
                            model,
                            voices,
                        })
                    }
                    TtsBackend::Model(TtsModel::Qwen {
                        model_dir,
                        tokenizer_dir,
                    }) => aria_tts::qwen::Qwen::check_from(&aria_tts::qwen::model_files::Config {
                        model_dir,
                        speech_tokenizer_dir: tokenizer_dir,
                    }),
                    TtsBackend::System => panic!("the system voice downloads nothing"),
                };
            if let Err(error) = checked {
                panic!("{}: {}", tts.key(), error_chain(&error));
            }
        }
    }

    #[test]
    fn setup_defaults_to_the_engines_an_unset_config_runs() {
        let host = Host {
            macos: true,
            system_voice: Availability::Ready,
        };
        let default = |choices: Vec<Choice>| choices.into_iter().find(|choice| choice.default);
        let unset = SettingsFile::default();

        assert_eq!(
            default(choices::<Stt>(None, &host)).map(|choice| choice.key),
            Some(config::transcription_engine(&unset.stt))
        );
        assert_eq!(
            default(choices::<Tts>(None, &host)).map(|choice| choice.key),
            Some(unset.tts.backend.as_deref().unwrap_or(tts_backend::DEFAULT))
        );
    }

    #[test]
    fn mlx_needs_macos() {
        let host = Host {
            macos: false,
            system_voice: Availability::Ready,
        };

        assert!(matches!(
            Stt::ParakeetMlx.availability(&host),
            Availability::Unsupported { .. }
        ));
    }
}
