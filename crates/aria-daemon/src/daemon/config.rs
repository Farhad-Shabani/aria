//! What the daemon itself reads from the settings: the wake word, the Aria
//! key, and the phone relay.

mod assistant;

pub use assistant::{load_chat, load_memory};

use std::path::{Path, PathBuf};

use aria_settings::config::{ConfigError, model_location, wake_backend};
use aria_settings::{CheckedSettings, InvalidValue, Key, Phone, WakeEngine};

use aria_voice::config::{CaptureConfig, DictationConfig};

pub struct DaemonConfig {
    pub dictation: DictationConfig,
    pub wake: WakeConfig,
    pub relay: RelayConfig,
}

/// The command's say over `wake.enabled`, outranking the file. `aria wake`
/// exists to listen, so it enables monitoring whatever the settings say.
/// Nothing here can turn monitoring off.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeWordOverride {
    Inherit,
    Enable,
}

/// The global dictation hotkey: one bare modifier key, tapped or held. Which
/// key lives in the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyConfig {
    Disabled,
    Enabled,
}

/// Whether this Mac takes recordings relayed off a phone or watch on the
/// LAN, and on which port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayConfig {
    Disabled,
    Enabled { port: u16 },
}

impl RelayConfig {
    pub fn port(self) -> Option<u16> {
        match self {
            Self::Disabled => None,
            Self::Enabled { port } => Some(port),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WakeConfig {
    Disabled,
    Enabled {
        backend: WakeEngine,
        classifier: PathBuf,
        threshold: f32,
    },
}

impl DaemonConfig {
    pub fn load(
        settings: &CheckedSettings,
        home: &Path,
        wake_word: WakeWordOverride,
    ) -> Result<Self, ConfigError> {
        let dictation = DictationConfig::load(settings, home, super::inference::server_pin(home));
        let wake = wake(settings, home, &dictation.capture, wake_word)?;
        Ok(Self {
            dictation,
            wake,
            relay: relay(&settings.phone),
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

fn wake(
    settings: &CheckedSettings,
    home: &Path,
    capture: &CaptureConfig,
    wake_word: WakeWordOverride,
) -> Result<WakeConfig, ConfigError> {
    let wake = &settings.wake;
    let enabled = matches!(wake_word, WakeWordOverride::Enable) || wake.enabled.unwrap_or(false);
    if !enabled {
        return Ok(WakeConfig::Disabled);
    }

    let backend = settings.choices().wake;

    if !matches!(capture, CaptureConfig::Microphone) {
        return Err(ConfigError::WakeRequiresMicrophone);
    }

    let model = match (wake.model.as_deref(), backend) {
        (Some(model), _) => Some(model),
        (None, WakeEngine::OpenWakeWord) => None,
        // No aria-trained livekit head ships with Aria, and the openWakeWord
        // default would load into the wrong backend.
        (None, WakeEngine::LiveKit) => {
            return Err(ConfigError::Invalid {
                key: Key::WakeModel,
                source: InvalidValue::Required {
                    backend: wake_backend::LIVEKIT.to_owned(),
                },
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

const DEFAULT_WAKE_MODEL: &str = "wakeword/aria.onnx";

const DEFAULT_WAKE_THRESHOLD: f32 = 0.5;

#[cfg(test)]
mod tests {
    use super::*;
    use aria_kit::audio::ReplayPace;
    use aria_settings::testing::{file, test_home};

    #[test]
    fn disabled_wake_ignores_its_dormant_settings() {
        let file = file("[wake]\nenabled = false\nmodel = \"/missing/model.onnx\"\n");

        let wake = wake(
            &file,
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
            &file("[wake]\nenabled = false\n"),
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
            &file("[wake]\nenabled = false\n"),
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

    fn enabled_wake(rest: &str) -> CheckedSettings {
        file(&format!("[wake]\nenabled = true\n{rest}"))
    }

    #[test]
    fn wake_defaults_to_openwakeword_and_its_own_model() {
        let wake = wake(
            &enabled_wake(""),
            &test_home(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect("the default backend must resolve");

        assert_eq!(
            wake,
            WakeConfig::Enabled {
                backend: WakeEngine::OpenWakeWord,
                classifier: PathBuf::from("/home/user/.aria/models/wakeword/aria.onnx"),
                threshold: DEFAULT_WAKE_THRESHOLD,
            }
        );
    }

    #[test]
    fn wake_resolves_the_livekit_backend() {
        let file = enabled_wake("backend = \"livekit\"\nmodel = \"~/models/hey_livekit.onnx\"\n");

        let wake = wake(
            &file,
            &test_home(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Inherit,
        )
        .expect("livekit with a model must resolve");

        assert_eq!(
            wake,
            WakeConfig::Enabled {
                backend: WakeEngine::LiveKit,
                classifier: PathBuf::from("/home/user/models/hey_livekit.onnx"),
                threshold: DEFAULT_WAKE_THRESHOLD,
            }
        );
    }

    #[test]
    fn a_wake_word_run_of_livekit_without_a_model_is_refused() {
        let error = wake(
            &file("[wake]\nbackend = \"livekit\"\n"),
            &test_home(),
            &CaptureConfig::Microphone,
            WakeWordOverride::Enable,
        )
        .expect_err("livekit has no default model");

        assert!(matches!(
            error,
            ConfigError::Invalid {
                key: Key::WakeModel,
                source: InvalidValue::Required { ref backend },
            } if backend == "livekit"
        ));
    }

    #[test]
    fn engine_names_match_what_the_engines_declare() {
        assert_eq!(wake_backend::OPEN_WAKE_WORD, aria_wake::openwakeword::NAME);
        assert_eq!(wake_backend::LIVEKIT, aria_wake::livekit::NAME);
    }
}
