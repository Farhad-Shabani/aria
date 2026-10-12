//! The choices setup offers, and what this machine can run.

use std::env;
use std::path::PathBuf;

use crate::config::{backend, chat_agent, default_model_path, tts_backend};
use crate::primitives::file_inspection::InspectedFile;

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

/// One engine as setup offers it: what config.yaml calls it, what a person
/// sees, and whether this host can run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub key: &'static str,
    pub label: &'static str,
    pub availability: Availability,
    /// Exactly one choice per family. The configured one wins, even with
    /// its model missing: that is a fix to show, not a reason to choose for
    /// the user. Otherwise the first that is ready, else the first listed.
    pub default: bool,
}

pub(super) fn choices<B: Backend>(current: Option<&str>, host: &Host) -> Vec<Choice> {
    let mut choices: Vec<Choice> = B::ALL
        .iter()
        .map(|backend| Choice {
            key: backend.key(),
            label: backend.label(),
            availability: backend.availability(host),
            default: false,
        })
        .collect();
    let default = choices
        .iter()
        .position(|choice| {
            Some(choice.key) == current
                && !matches!(choice.availability, Availability::Unsupported { .. })
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

/// What is known about this machine before any question is asked.
pub(super) struct Host {
    pub(super) macos: bool,
    pub(super) home: PathBuf,
    pub(super) system_voice: Availability,
}

impl Host {
    pub(super) fn probe(home: PathBuf) -> Self {
        Self {
            macos: cfg!(target_os = "macos"),
            home,
            system_voice: match aria_tts::system::SystemVoice::new() {
                Ok(_) => Availability::Ready,
                Err(error) => Availability::Missing {
                    fix: error.to_string(),
                },
            },
        }
    }

    /// The daemon spawns these by bare name, so the probe has to agree with
    /// execvp: a file without the execute bit is not on the path.
    fn on_path(&self, program: &str) -> bool {
        env::var_os("PATH")
            .map(|path| {
                env::split_paths(&path).any(|directory| {
                    InspectedFile::inspect(directory.join(program))
                        .and_then(InspectedFile::require_executable)
                        .is_ok()
                })
            })
            .unwrap_or(false)
    }

    /// Ready when the backend's default model is on disk; the fix names
    /// where to put it.
    fn model_availability(&self, backend_name: &str) -> Availability {
        match default_model_path(backend_name, &self.home) {
            Some(path) if path.exists() => Availability::Ready,
            Some(path) => Availability::Missing {
                fix: format!("place the model at {}", path.display()),
            },
            None => Availability::Ready,
        }
    }
}

/// One family of interchangeable engines the user picks from.
pub(super) trait Backend: Copy + Eq + 'static {
    const TITLE: &'static str;
    const ALL: &'static [Self];
    /// The value written to config.yaml.
    fn key(self) -> &'static str;
    fn label(self) -> &'static str;
    fn availability(self, host: &Host) -> Availability;
    /// What setup downloads for it.
    fn assets(self) -> &'static [Asset];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stt {
    Whisper,
    WhisperCandle,
    Parakeet,
    Moonshine,
    Nemotron,
    ParakeetMlx,
}

impl Backend for Stt {
    const TITLE: &'static str = "Speech to text";
    const ALL: &'static [Self] = &[
        Self::Whisper,
        Self::WhisperCandle,
        Self::Parakeet,
        Self::Moonshine,
        Self::Nemotron,
        Self::ParakeetMlx,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::Whisper => backend::WHISPER,
            Self::WhisperCandle => backend::WHISPER_CANDLE,
            Self::Parakeet => backend::PARAKEET,
            Self::Moonshine => backend::MOONSHINE,
            Self::Nemotron => backend::NEMOTRON,
            Self::ParakeetMlx => backend::PARAKEET_MLX,
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
        }
    }

    fn availability(self, host: &Host) -> Availability {
        match self {
            Self::Whisper => Availability::Ready,
            Self::ParakeetMlx if !host.macos => Availability::Unsupported {
                why: "needs macOS (MLX)",
            },
            Self::ParakeetMlx => Availability::Missing {
                fix: "set transcription.interpreter to a Python with parakeet-mlx".to_owned(),
            },
            _ => host.model_availability(self.key()),
        }
    }

    fn assets(self) -> &'static [Asset] {
        match self {
            Self::Whisper => &[assets::WHISPER],
            _ => &[],
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
            Self::Kokoro => Availability::Ready,
            Self::Qwen => host.model_availability(self.key()),
        }
    }

    fn assets(self) -> &'static [Asset] {
        match self {
            Self::Kokoro => &[assets::KOKORO],
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Chat {
    ClaudeCode,
    Codex,
}

impl Backend for Chat {
    const TITLE: &'static str = "Chat agent (for `aria ask`)";
    const ALL: &'static [Self] = &[Self::ClaudeCode, Self::Codex];

    fn key(self) -> &'static str {
        match self {
            Self::ClaudeCode => chat_agent::CLAUDE_CODE,
            Self::Codex => chat_agent::CODEX,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    fn availability(self, host: &Host) -> Availability {
        let program = match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        };
        if host.on_path(program) {
            Availability::Ready
        } else {
            Availability::Missing {
                fix: format!("install the `{program}` command"),
            }
        }
    }

    fn assets(self) -> &'static [Asset] {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_backend_is_missing_until_its_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host {
            macos: true,
            home: dir.path().to_path_buf(),
            system_voice: Availability::Ready,
        };

        assert!(matches!(
            Stt::Parakeet.availability(&host),
            Availability::Missing { .. }
        ));

        let model = default_model_path(backend::PARAKEET, dir.path()).unwrap();
        std::fs::create_dir_all(&model).unwrap();
        assert_eq!(Stt::Parakeet.availability(&host), Availability::Ready);
    }

    #[test]
    fn mlx_needs_macos() {
        let host = Host {
            macos: false,
            home: PathBuf::from("/nonexistent"),
            system_voice: Availability::Ready,
        };

        assert!(matches!(
            Stt::ParakeetMlx.availability(&host),
            Availability::Unsupported { .. }
        ));
    }
}
