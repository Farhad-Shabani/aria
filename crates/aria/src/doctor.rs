//! Checks settings, model paths, permissions, and daemon readiness
//! using this shell's environment. Models are checked without loading weights;
//! a passing check does not guarantee that inference will succeed.

use std::path::Path;
use std::time::Duration;

use crate::audio::{WavReplayCapture, check_default_input_device};
use crate::primitives::error_chain;
use aria_daemon::RELAY_SERVICE_TYPE;
use aria_daemon::config::{DaemonConfig, RelayConfig, WakeConfig, WakeWordOverride};
use aria_daemon::ipc::{DaemonClient, Liveness, probe_daemon};
use aria_daemon::ipc::{HOTKEY_HEALTH_SERVICE, WAKE_WORD_HEALTH_SERVICE};
use aria_kit::segmentation::Engine as SegmentationEngine;
use aria_kit::speaker::Engine as SpeakerEngine;
use aria_platform::hotkey::PlatformHotkeyListener;
use aria_platform::runtime::{PreparedRuntimePaths, RuntimeError};
use aria_provider::ProviderError;
use aria_segmentation::pyannote::Pyannote;
use aria_settings::WakeEngine;
use aria_settings::config::ConfigError;
use aria_settings::{CheckedSettings, LlmCleanupTarget, Settings};
use aria_speaker::wespeaker::WeSpeaker;
use aria_voice::config::{
    CaptureConfig, DictationConfig, SpeakerConfig, SttBackend, SystemTranscriptionConfig,
};
use aria_voice::dictation::{LlmCleanup, LlmCleanupError};
use aria_voice::stt::check_model;
use aria_wake::livekit::{Config as LiveKitConfig, LiveKitWakeWord};
use aria_wake::openwakeword::{Config as OpenWakeWordConfig, OpenWakeWord};

const SETTINGS: &str = "settings";
const RUNTIME_DIRECTORY: &str = "runtime directory";
const AUDIO_CAPTURE: &str = "audio capture";
const SPEECH_MODEL: &str = "speech model";
const WAKE_WORD_MODEL: &str = "wake-word model";
const SPEAKER_MODEL: &str = "speaker model";
const SEGMENTATION_MODEL: &str = "segmentation model";
const SYSTEM_TRANSCRIPTION: &str = "system audio transcription";
const PHONE: &str = "phone";
const DAEMON: &str = "daemon";
const ARIA_KEY: &str = "aria key";
const CLEANUP_MODEL: &str = "cleanup model";

const SETTINGS_FAILED: &str = "not checked: the settings failed to load";

/// Bounds the probe when no relay accepts connections.
const RELAY_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

pub struct Report {
    pub checks: Vec<Check>,
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Optional, still starting, or not checked. Never sets the exit code.
    Warning,
    Failed,
}

impl Report {
    pub fn has_failure(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == Status::Failed)
    }
}

/// A check whose dependency failed is left unchecked rather than failed, so
/// one broken setting does not read as five broken things.
pub fn diagnose() -> Report {
    // The report describes what is configured, never what a command would
    // force on top of it.
    let config =
        loaded(|settings, home| DaemonConfig::load(settings, home, WakeWordOverride::Inherit));
    let prepared = PreparedRuntimePaths::for_current_user();

    Report {
        checks: vec![
            settings_check(config.as_ref()),
            runtime_directory(prepared.as_ref()),
            audio_capture(config.as_ref().ok().map(|config| &config.dictation)),
            stt_model(config.as_ref().ok().map(|config| &config.dictation)),
            wake_word_model(config.as_ref().ok().map(|config| &config.wake)),
            speaker_model(),
            segmentation_model(),
            system_transcription(),
            relay(config.as_ref().ok().map(|config| config.relay)),
            daemon(
                prepared.as_ref().ok(),
                config.as_ref().ok().map(|config| &config.wake),
            ),
            aria_key(),
            cleanup_model(config.as_ref().ok().map(|config| &config.dictation)),
        ],
    }
}

fn settings_check(config: Result<&DaemonConfig, &ConfigError>) -> Check {
    match config {
        Ok(config) => Check::ok(
            SETTINGS,
            format!(
                "{}, backend {}, wake {}",
                settings_file(),
                config.dictation.stt.backend_name(),
                wake_status(&config.wake),
            ),
        ),
        Err(error) => Check::failed(SETTINGS, error_chain(error)),
    }
}

/// Resolves one part of the config from the settings file.
fn loaded<T>(
    load: impl FnOnce(&CheckedSettings, &Path) -> Result<T, ConfigError>,
) -> Result<T, ConfigError> {
    let home = aria_settings::config::home_directory()?;
    let settings = aria_settings::load(&home)?;
    load(&settings, &home)
}

fn settings_file() -> String {
    match crate::primitives::user_home::resolve() {
        Ok(home) => {
            let path = aria_settings::default_path(&home);
            if path.is_file() {
                path.display().to_string()
            } else {
                format!("no {}, using defaults", path.display())
            }
        }
        Err(error) => error_chain(&error),
    }
}

fn runtime_directory(paths: Result<&PreparedRuntimePaths, &RuntimeError>) -> Check {
    match paths {
        Ok(paths) => Check::ok(RUNTIME_DIRECTORY, paths.directory().display().to_string()),
        Err(error) => Check::failed(RUNTIME_DIRECTORY, error_chain(error)),
    }
}

fn audio_capture(config: Option<&DictationConfig>) -> Check {
    match config {
        Some(config) => capture_source(&config.capture, config.dictation_timeouts.recording()),
        None => Check::warning(AUDIO_CAPTURE, SETTINGS_FAILED),
    }
}

fn capture_source(capture: &CaptureConfig, recording_timeout: Duration) -> Check {
    match capture {
        CaptureConfig::Microphone => match check_default_input_device() {
            Ok(device) => Check::ok(AUDIO_CAPTURE, device),
            Err(error) => Check::failed(AUDIO_CAPTURE, error_chain(&error)),
        },
        CaptureConfig::WavFile { path, .. } => {
            match WavReplayCapture::check(path, recording_timeout) {
                Ok(fixture) => Check::ok(AUDIO_CAPTURE, format!("replaying {}", fixture.display())),
                Err(error) => Check::failed(AUDIO_CAPTURE, error_chain(&error)),
            }
        }
    }
}

fn stt_model(config: Option<&DictationConfig>) -> Check {
    match config {
        Some(config) => match aria_settings::config::home_directory() {
            Ok(home) => model(&config.stt, &home),
            Err(error) => Check::failed(SPEECH_MODEL, error_chain(&error)),
        },
        None => Check::warning(SPEECH_MODEL, SETTINGS_FAILED),
    }
}

fn model(stt: &SttBackend, home: &Path) -> Check {
    let engine = match stt {
        SttBackend::Local(engine) => engine,
        SttBackend::Server(_) => return server(home),
    };
    match check_model(engine) {
        Ok(detail) => Check::ok(SPEECH_MODEL, detail),
        Err(error) => Check::failed(SPEECH_MODEL, error_chain(&error)),
    }
}

/// The daemon reaches only the server its pin names.
fn server(home: &Path) -> Check {
    match aria_daemon::paired_server(home) {
        Ok(Some(paired)) => Check::ok(
            SPEECH_MODEL,
            format!("server {}; no local speech model", paired.endpoint),
        ),
        Ok(None) => Check::failed(
            SPEECH_MODEL,
            "no server is paired; run `aria remote pair` first",
        ),
        Err(error) => Check::failed(SPEECH_MODEL, error_chain(&error)),
    }
}

fn wake_status(wake: &WakeConfig) -> &'static str {
    match wake {
        WakeConfig::Disabled => "disabled",
        WakeConfig::Enabled { backend, .. } => backend.name(),
    }
}

fn wake_word_model(wake: Option<&WakeConfig>) -> Check {
    match wake {
        Some(WakeConfig::Disabled) => Check::warning(WAKE_WORD_MODEL, "disabled"),
        Some(WakeConfig::Enabled {
            backend,
            classifier,
            threshold,
        }) => {
            let resolved = match backend {
                WakeEngine::OpenWakeWord => OpenWakeWordConfig::new(classifier, *threshold)
                    .map_err(|error| error_chain(&error))
                    .and_then(|config| {
                        OpenWakeWord::check_from(&config).map_err(|error| error_chain(&error))
                    }),
                WakeEngine::LiveKit => LiveKitConfig::new(classifier, *threshold)
                    .map_err(|error| error_chain(&error))
                    .and_then(|config| {
                        LiveKitWakeWord::check_from(&config).map_err(|error| error_chain(&error))
                    }),
            };
            match resolved {
                Ok(files) => Check::ok(
                    WAKE_WORD_MODEL,
                    format!(
                        "{} with {} and {}",
                        files.classifier.display(),
                        files.melspectrogram.display(),
                        files.embedding.display()
                    ),
                ),
                Err(detail) => Check::failed(WAKE_WORD_MODEL, detail),
            }
        }
        None => Check::warning(WAKE_WORD_MODEL, SETTINGS_FAILED),
    }
}

/// A missing speaker model warns because transcription still works without labels.
fn speaker_model() -> Check {
    let resolved = loaded(|settings, home| Ok(SpeakerConfig::load(&settings.speaker, home)))
        .map_err(|error| error_chain(&error))
        .and_then(|config| {
            WeSpeaker::check_from(&aria_speaker::wespeaker::Config {
                model: config.model,
            })
            .map_err(|error| error_chain(&error))
        });
    match resolved {
        Ok(model) => Check::ok(SPEAKER_MODEL, model.display().to_string()),
        Err(detail) => Check::warning(
            SPEAKER_MODEL,
            format!("{detail}; transcripts stay unlabeled"),
        ),
    }
}

/// A missing segmentation model warns: system transcription still names speakers, less
/// reliably, and dictation still types every voice.
fn segmentation_model() -> Check {
    let resolved = loaded(|settings, home| Ok(SpeakerConfig::load(&settings.speaker, home)))
        .map_err(|error| error_chain(&error))
        .and_then(|config| {
            Pyannote::check_from(&aria_segmentation::pyannote::Config {
                model: config.segmentation_model,
            })
            .map_err(|error| error_chain(&error))
        });
    match resolved {
        Ok(model) => Check::ok(SEGMENTATION_MODEL, model.display().to_string()),
        Err(detail) => Check::warning(
            SEGMENTATION_MODEL,
            format!(
                "{detail}; Transcription names speakers by guess, and Only type my voice cannot run"
            ),
        ),
    }
}

/// System transcription accepts any engine that streams. A config or model it can't
/// use warns, because dictation still works without it.
fn system_transcription() -> Check {
    let resolved = loaded(SystemTranscriptionConfig::load);
    match resolved {
        Ok(config) => match config.engine.check_model() {
            Ok(_) => Check::ok(
                SYSTEM_TRANSCRIPTION,
                format!(
                    "{} writing to {}",
                    config.engine.backend_name(),
                    config.transcripts_dir.display()
                ),
            ),
            Err(error) => Check::warning(
                SYSTEM_TRANSCRIPTION,
                format!(
                    "{}; Transcription can't start until {} is downloaded",
                    error_chain(&error),
                    config.engine.backend_name()
                ),
            ),
        },
        Err(error) => Check::warning(
            SYSTEM_TRANSCRIPTION,
            format!(
                "{}; set transcription.backend to one of {} to transcribe your Mac's audio",
                error_chain(&error),
                aria_settings::config::backend::STREAMING.join(", ")
            ),
        ),
    }
}

/// The relay is optional, so nothing here fails.
fn relay(config: Option<RelayConfig>) -> Check {
    let Some(config) = config else {
        return Check::warning(PHONE, SETTINGS_FAILED);
    };
    let Some(port) = config.port() else {
        return Check::warning(
            PHONE,
            "disabled; run `aria settings set phone.enabled on` to take recordings from an iPhone or Apple Watch"
                .to_owned(),
        );
    };
    // Asked, not assumed: a lost port or unusable pairing file only logs, and
    // the daemon serves on happily without a relay. Reporting the
    // settings would call that healthy.
    if !relay_is_listening(port) {
        return Check::warning(
            PHONE,
            format!(
                "nothing is listening on port {port}; the daemon is not running, \
                 something else took the port, or its pairing file is unusable"
            ),
        );
    }
    Check::ok(
        PHONE,
        format!("listening on port {port}, advertised as {RELAY_SERVICE_TYPE}"),
    )
}

/// A plain TCP connect to the loopback side of the relay's own port. It says
/// something is accepting there, which is the question, and costs no gRPC.
fn relay_is_listening(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&address, RELAY_PROBE_TIMEOUT).is_ok()
}

fn daemon(paths: Option<&PreparedRuntimePaths>, wake: Option<&WakeConfig>) -> Check {
    match paths {
        Some(paths) => daemon_at(paths.socket_path(), wake),
        None => Check::warning(DAEMON, "not checked: the runtime directory failed"),
    }
}

fn daemon_at(socket: &Path, wake: Option<&WakeConfig>) -> Check {
    match probe_daemon(socket) {
        Ok(Liveness::Ready(mut client)) => running_daemon(socket, &mut client, wake),
        Ok(Liveness::Starting(_)) => {
            Check::warning(DAEMON, format!("starting at {}", socket.display()))
        }
        Ok(Liveness::NotRunning(_)) => not_running(socket, wake),
        Err(error) => Check::failed(DAEMON, error_chain(&error)),
    }
}

fn running_daemon(socket: &Path, client: &mut DaemonClient, wake: Option<&WakeConfig>) -> Check {
    if wake_is_enabled(wake)
        && let Err(error) = client.check_service_ready(WAKE_WORD_HEALTH_SERVICE)
    {
        return Check::failed(
            DAEMON,
            format!(
                "running at {}, but wake-word monitoring is unavailable: {}",
                socket.display(),
                error_chain(&error)
            ),
        );
    }
    // A missing Accessibility grant is the expected first-run state, so an
    // unarmed hotkey warns with the fix instead of failing.
    if client.check_service_ready(HOTKEY_HEALTH_SERVICE).is_err() {
        return Check::warning(
            DAEMON,
            format!(
                "running at {}, but {}",
                socket.display(),
                hotkey_unarmed_reason()
            ),
        );
    }
    Check::ok(
        DAEMON,
        format!("running at {}{}", socket.display(), serving_suffix(wake)),
    )
}

fn serving_suffix(wake: Option<&WakeConfig>) -> &'static str {
    if wake_is_enabled(wake) {
        "; wake-word monitoring and the dictation hotkey are serving"
    } else {
        "; the dictation hotkey is armed"
    }
}

/// Why the dictation hotkey is not armed, and the fix when one exists.
pub fn hotkey_unarmed_reason() -> String {
    match loaded(|settings, _home| Ok(settings.aria_key())) {
        Ok(key) => PlatformHotkeyListener::unarmed_reason(key),
        Err(error) => format!("the settings file could not be read: {error}"),
    }
}

fn wake_is_enabled(wake: Option<&WakeConfig>) -> bool {
    matches!(wake, Some(WakeConfig::Enabled { .. }))
}

fn not_running(socket: &Path, wake: Option<&WakeConfig>) -> Check {
    let needs = if wake_is_enabled(wake) {
        "wake-word monitoring and the dictation hotkey need"
    } else {
        "the dictation hotkey needs"
    };
    Check::failed(
        DAEMON,
        format!(
            "not running at {}; {needs} the persistent daemon: `aria service install`",
            socket.display()
        ),
    )
}

/// Modifier Keys acts before any app sees a key. Pressing a remapped Aria
/// key never fires the hotkey. Only a key remapped onto the Aria key can.
fn aria_key() -> Check {
    match loaded(|settings, _home| Ok(settings.aria_key())) {
        Ok(key) => match PlatformHotkeyListener::remap(key) {
            Some(remap) => Check::warning(
                ARIA_KEY,
                remap_detail(remap.key(), remap.destination(), remap.senders()),
            ),
            None => Check::ok(ARIA_KEY, "reaches Aria unchanged"),
        },
        Err(_) => Check::warning(ARIA_KEY, SETTINGS_FAILED),
    }
}

fn remap_detail(key: &str, destination: &str, senders: &[&str]) -> String {
    let sent_by = match senders {
        [] => format!("no key sends {key}"),
        [only] => format!("only {only} sends {key}, if this keyboard has one"),
        [rest @ .., last] => format!(
            "{} and {last} send {key}, where this keyboard has them",
            rest.join(", ")
        ),
    };
    format!(
        "Modifier Keys remaps it: {key} now sends {destination}, and {sent_by}; restore it \
         in System Settings → Keyboard → Keyboard Shortcuts → Modifier Keys"
    )
}

/// The model is optional, and every warning still types the rule-cleaned
/// text, so none of them fails. Each press decides whether it delivers
/// whole, so this doesn't judge delivery.
fn cleanup_model(dictation: Option<&DictationConfig>) -> Check {
    let Some(dictation) = dictation else {
        return Check::warning(CLEANUP_MODEL, SETTINGS_FAILED);
    };
    let Some(target) = &dictation.cleanup_llm else {
        return Check::ok(CLEANUP_MODEL, "not set; cleanup uses its rules only");
    };
    match loaded(|settings, _home| {
        let settings = Settings::from_file(settings)?;
        Ok(settings.cleanup_enabled && settings.cleanup_llm_enabled)
    }) {
        Ok(cleanup) => cleanup_check(target, cleanup, || LlmCleanup::check_ready(target)),
        Err(_) => Check::warning(CLEANUP_MODEL, SETTINGS_FAILED),
    }
}

fn cleanup_check(
    target: &LlmCleanupTarget,
    cleanup: bool,
    ready: impl FnOnce() -> Result<(), LlmCleanupError>,
) -> Check {
    let model = target.model();
    if !cleanup {
        return Check::warning(
            CLEANUP_MODEL,
            format!("{model} is set, but text cleanup or its model is off, so it never runs"),
        );
    }
    let endpoint = target.provider().endpoint();
    match ready() {
        Ok(()) => Check::ok(
            CLEANUP_MODEL,
            format!("{model} at {endpoint}; skipped for text typed while you speak"),
        ),
        Err(LlmCleanupError::ModelMissing) => Check::warning(
            CLEANUP_MODEL,
            format!("{endpoint} doesn't list {model}; dictation types the rule-cleaned text"),
        ),
        Err(
            error @ (LlmCleanupError::Timeout | LlmCleanupError::Server(ProviderError::Transport)),
        ) => Check::warning(
            CLEANUP_MODEL,
            format!("{endpoint} doesn't answer ({error}); dictation types the rule-cleaned text"),
        ),
        Err(error) => Check::warning(
            CLEANUP_MODEL,
            format!("{endpoint} answered, but {error}; dictation types the rule-cleaned text"),
        ),
    }
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Ok, detail)
    }

    fn warning(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Warning, detail)
    }

    fn failed(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Failed, detail)
    }

    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use crate::audio::ReplayPace;
    use crate::audio::canonical_fixture;

    use super::*;

    const RECORDING_TIMEOUT: Duration = Duration::from_secs(300);

    #[test]
    fn a_remap_names_every_key_that_sends_the_aria_key() {
        let detail = |senders: &[&str]| remap_detail("Right Option", "Caps Lock", senders);
        assert!(
            detail(&[])
                .contains("Right Option now sends Caps Lock, and no key sends Right Option;")
        );
        assert!(
            detail(&["Right Control"])
                .contains(", and only Right Control sends Right Option, if this keyboard has one;")
        );
        assert!(
            detail(&["Left Control", "Right Control", "Right Command"]).contains(
                ", and Left Control, Right Control and Right Command send Right Option, \
                     where this keyboard has them;"
            )
        );
    }

    #[test]
    fn the_cleanup_model_warns_when_it_would_never_run_or_cannot_be_reached() {
        let target = LlmCleanupTarget::new(
            aria_settings::LocalProvider::new("http://127.0.0.1:11434/v1/chat/completions")
                .unwrap(),
            "qwen2.5:3b",
        )
        .unwrap();
        let cases = [
            (false, None, Status::Warning, "its model is off"),
            (
                true,
                Some(LlmCleanupError::ModelMissing),
                Status::Warning,
                "doesn't list qwen2.5:3b",
            ),
            (
                true,
                Some(LlmCleanupError::Server(ProviderError::Transport)),
                Status::Warning,
                "doesn't answer",
            ),
            (
                true,
                Some(LlmCleanupError::from(ProviderError::Timeout)),
                Status::Warning,
                "doesn't answer",
            ),
            (
                true,
                Some(LlmCleanupError::Server(ProviderError::HttpStatus)),
                Status::Warning,
                "answered, but",
            ),
            (
                true,
                None,
                Status::Ok,
                "qwen2.5:3b at http://127.0.0.1:11434",
            ),
        ];
        for (cleanup, failure, status, detail) in cases {
            let check = cleanup_check(&target, cleanup, || failure.map_or(Ok(()), Err));
            assert_eq!(check.status, status, "{detail}");
            assert!(check.detail.contains(detail), "{}", check.detail);
        }
    }

    fn replaying(path: PathBuf) -> Check {
        capture_source(
            &CaptureConfig::WavFile {
                path,
                pace: ReplayPace::Immediate,
            },
            RECORDING_TIMEOUT,
        )
    }

    #[test]
    fn a_paired_server_needs_no_local_model() {
        let home = tempfile::TempDir::new().unwrap();
        aria_daemon::pair_server_for_tests(home.path(), "https://example.test:9851");

        let check = model(&SttBackend::Server(None), home.path());

        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("https://example.test:9851"));
    }

    #[test]
    fn the_server_without_a_pairing_fails() {
        let home = tempfile::TempDir::new().unwrap();

        let unpaired = model(&SttBackend::Server(None), home.path());

        assert_eq!(unpaired.status, Status::Failed);
        assert!(unpaired.detail.contains("aria remote pair"));
    }

    #[test]
    fn a_replayed_fixture_is_reported_by_path() {
        let directory = tempfile::TempDir::new().expect("test directory should be created");

        let check = replaying(canonical_fixture(directory.path()));

        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("fixture.wav"));
    }

    #[test]
    fn a_missing_fixture_fails_and_says_why() {
        let check = replaying(PathBuf::from("/nonexistent/fixture.wav"));

        assert_eq!(check.status, Status::Failed);
        assert!(check.detail.contains("/nonexistent/fixture.wav"));
    }

    /// A fixture the daemon refuses must not pass here, or doctor sends the
    /// user to press a key that cannot work.
    #[test]
    fn a_fixture_other_users_can_write_is_refused() {
        let directory = tempfile::TempDir::new().expect("test directory should be created");
        let path = canonical_fixture(directory.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666))
            .expect("test permissions should be set");

        let check = replaying(path);

        assert_eq!(check.status, Status::Failed);
    }

    #[test]
    fn a_missing_model_fails_and_says_which_backend() {
        let check = model(
            &SttBackend::Local(aria_voice::config::SttConfig::Whisper {
                model: PathBuf::from("/nonexistent/model.bin"),
            }),
            Path::new("/nonexistent"),
        );

        assert_eq!(check.status, Status::Failed);
        assert!(check.detail.contains("whisper"));
        assert!(check.detail.contains("/nonexistent/model.bin"));
    }

    #[test]
    fn a_missing_wake_classifier_fails_the_wake_check() {
        for backend in [WakeEngine::OpenWakeWord, WakeEngine::LiveKit] {
            let check = wake_word_model(Some(&WakeConfig::Enabled {
                backend,
                classifier: PathBuf::from("/nonexistent/wake.onnx"),
                threshold: 0.5,
            }));

            assert_eq!(check.status, Status::Failed, "{}", backend.name());
            assert!(check.detail.contains("classifier"));
            assert!(check.detail.contains("/nonexistent/wake.onnx"));
        }
    }

    /// A check with nothing to check against must not read as a failure of the
    /// thing it never looked at.
    #[test]
    fn failed_settings_leave_its_dependants_unchecked() {
        for check in [audio_capture(None), stt_model(None), wake_word_model(None)] {
            assert_eq!(check.status, Status::Warning);
            assert_eq!(check.detail, SETTINGS_FAILED);
        }
    }

    #[test]
    fn a_socket_nobody_listens_on_reports_a_stopped_daemon() {
        let directory = tempfile::TempDir::new().expect("test directory should be created");

        let check = daemon_at(&directory.path().join("daemon.sock"), None);

        assert_ne!(check.status, Status::Ok);
        assert!(check.detail.contains("not running"));
    }

    /// The daemon binds its socket before it loads a model, so for as long as
    /// that takes it accepts nothing. Reporting that as a failure sends the
    /// user to fix a daemon that is about to work.
    #[test]
    fn a_daemon_that_has_not_answered_yet_is_starting_rather_than_broken() {
        let directory = tempfile::TempDir::new().expect("test directory should be created");
        let socket = directory.path().join("daemon.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket)
            .expect("the test socket should be bound");

        let check = daemon_at(&socket, None);

        assert_eq!(check.status, Status::Warning);
        assert!(check.detail.contains("starting"));
    }

    #[test]
    fn only_a_failure_sets_the_exit_code() {
        let healthy = Report {
            checks: vec![Check::ok("a", "fine"), Check::warning("b", "noted")],
        };
        let broken = Report {
            checks: vec![Check::ok("a", "fine"), Check::failed("b", "broken")],
        };

        assert!(!healthy.has_failure());
        assert!(broken.has_failure());
    }
}
