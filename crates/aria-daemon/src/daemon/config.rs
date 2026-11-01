//! What the daemon itself reads from the settings: the wake word, the Aria
//! key, and the phone relay.

use std::path::{Path, PathBuf};

use aria_settings::config::{ConfigError, model_location, string_value, wake_backend};
use aria_settings::settings::{Phone, SettingsFile, Wake};

use aria_voice::config::{CaptureConfig, DictationConfig, TtsConfig};

pub struct DaemonConfig {
    pub dictation: DictationConfig,
    pub wake: WakeConfig,
    pub hotkey: HotkeyConfig,
    pub relay: RelayConfig,
    pub tts: TtsConfig,
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
        backend: WakeBackend,
        classifier: PathBuf,
        threshold: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeBackend {
    OpenWakeWord,
    LiveKit,
}

impl WakeBackend {
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenWakeWord => wake_backend::OPEN_WAKE_WORD,
            Self::LiveKit => wake_backend::LIVEKIT,
        }
    }
}

impl DaemonConfig {
    pub fn load(
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

const DEFAULT_WAKE_MODEL: &str = "wakeword/aria.onnx";

const DEFAULT_WAKE_THRESHOLD: f32 = 0.5;

#[cfg(test)]
mod tests {
    use super::*;
    use aria_kit::audio::ReplayPace;
    use aria_settings::settings::Loaded;

    fn test_home() -> PathBuf {
        PathBuf::from("/home/user")
    }

    fn file(text: &str) -> SettingsFile {
        Loaded::parse(text, &test_home())
            .expect("the file is valid")
            .settings
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

    #[test]
    fn engine_names_match_what_the_engines_declare() {
        assert_eq!(wake_backend::OPEN_WAKE_WORD, aria_wake::openwakeword::NAME);
        assert_eq!(wake_backend::LIVEKIT, aria_wake::livekit::NAME);
    }
}
