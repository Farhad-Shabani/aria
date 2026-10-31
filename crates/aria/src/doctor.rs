//! Checks configuration, model paths, permissions, and daemon readiness
//! using this shell's environment. Models are checked without loading weights;
//! a passing check does not guarantee that inference will succeed.

use std::path::Path;
use std::time::Duration;

use crate::audio::{WavReplayCapture, check_default_input_device};
use crate::config::{
    CaptureConfig, ConfigError, DaemonConfig, DictationConfig, Environment, HotkeyConfig,
    ListenConfig, RelayConfig, SpeakerConfig, TranscriptionBackend, WakeBackend, WakeConfig,
    WakeWordOverride,
};
use crate::daemon::RELAY_SERVICE_TYPE;
use crate::delivery::{PlatformTextDelivery, TextDelivery};
use crate::hotkey::PlatformHotkeyListener;
use crate::ipc::{DaemonClient, Liveness, probe_daemon};
use crate::ipc::{HOTKEY_HEALTH_SERVICE, WAKE_WORD_HEALTH_SERVICE};
use crate::primitives::error_chain;
use crate::runtime::{PreparedRuntimePaths, RuntimeError};
use crate::transcription::check_model;
use aria_kit::segmentation::Engine as SegmentationEngine;
use aria_kit::speaker::Engine as SpeakerEngine;
use aria_segmentation::pyannote::Pyannote;
use aria_speaker::wespeaker::WeSpeaker;
use aria_wake::livekit::{Config as LiveKitConfig, LiveKitWakeWord};
use aria_wake::openwakeword::{Config as OpenWakeWordConfig, OpenWakeWord};

const CONFIGURATION: &str = "configuration";
const RUNTIME_DIRECTORY: &str = "runtime directory";
const AUDIO_CAPTURE: &str = "audio capture";
const TRANSCRIPTION_MODEL: &str = "transcription model";
const WAKE_WORD_MODEL: &str = "wake-word model";
const SPEAKER_MODEL: &str = "speaker model";
const SEGMENTATION_MODEL: &str = "segmentation model";
const LISTEN: &str = "listen";
const PHONE: &str = "phone";
const DAEMON: &str = "daemon";

const UNCONFIGURED: &str = "not checked: the configuration failed to load";

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
/// one broken configuration does not read as five broken things.
pub fn diagnose() -> Report {
    // The report describes what is configured, never what a command would
    // force on top of it.
    let config = Environment::capture()
        .and_then(|environment| DaemonConfig::load(&environment, WakeWordOverride::Inherit));
    let prepared = PreparedRuntimePaths::for_current_user();

    Report {
        checks: vec![
            configuration(config.as_ref()),
            runtime_directory(prepared.as_ref()),
            audio_capture(config.as_ref().ok().map(|config| &config.dictation)),
            transcription_model(config.as_ref().ok().map(|config| &config.dictation)),
            wake_word_model(config.as_ref().ok().map(|config| &config.wake)),
            speaker_model(),
            segmentation_model(),
            listen(),
            relay(config.as_ref().ok().map(|config| config.relay)),
            daemon(
                prepared.as_ref().ok(),
                config.as_ref().ok().map(|config| &config.wake),
                config.as_ref().ok().map(|config| config.hotkey),
            ),
        ],
    }
}

fn configuration(config: Result<&DaemonConfig, &ConfigError>) -> Check {
    match config {
        Ok(config) => Check::ok(
            CONFIGURATION,
            format!(
                "{}, backend {}, wake {}",
                configuration_file(),
                config.dictation.transcription.backend_name(),
                wake_status(&config.wake),
            ),
        ),
        Err(error) => Check::failed(CONFIGURATION, error_chain(error)),
    }
}

fn configuration_file() -> String {
    match crate::config::default_path() {
        Ok(path) if path.is_file() => path.display().to_string(),
        Ok(path) => format!("no {}, using defaults", path.display()),
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
        Some(config) => capture_source(&config.capture, config.dictation_timeouts.recording),
        None => Check::warning(AUDIO_CAPTURE, UNCONFIGURED),
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

fn transcription_model(config: Option<&DictationConfig>) -> Check {
    match config {
        Some(config) => model(&config.transcription),
        None => Check::warning(TRANSCRIPTION_MODEL, UNCONFIGURED),
    }
}

fn model(transcription: &TranscriptionBackend) -> Check {
    let engine = match transcription {
        TranscriptionBackend::Local(engine) => engine,
        TranscriptionBackend::Server { endpoint } => {
            return Check::ok(
                TRANSCRIPTION_MODEL,
                format!("server {endpoint}; no local transcription model"),
            );
        }
    };
    match check_model(engine) {
        Ok(detail) => Check::ok(TRANSCRIPTION_MODEL, detail),
        Err(error) => Check::failed(TRANSCRIPTION_MODEL, error_chain(&error)),
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
                WakeBackend::OpenWakeWord => OpenWakeWordConfig::new(classifier, *threshold)
                    .map_err(|error| error_chain(&error))
                    .and_then(|config| {
                        OpenWakeWord::check_from(&config).map_err(|error| error_chain(&error))
                    }),
                WakeBackend::LiveKit => LiveKitConfig::new(classifier, *threshold)
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
        None => Check::warning(WAKE_WORD_MODEL, UNCONFIGURED),
    }
}

/// A missing speaker model warns because transcription still works without labels.
fn speaker_model() -> Check {
    let resolved = Environment::capture()
        .and_then(|environment| SpeakerConfig::load(&environment))
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

/// A missing segmentation model warns: Listen still names speakers, less
/// reliably, and dictation still types every voice.
fn segmentation_model() -> Check {
    let resolved = Environment::capture()
        .and_then(|environment| SpeakerConfig::load(&environment))
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
            format!("{detail}; listen names speakers by guess, and Only type my voice cannot run"),
        ),
    }
}

/// Listen accepts Parakeet, Moonshine, and Nemotron. Configuration failures
/// warn because dictation can still work without Listen.
fn listen() -> Check {
    let resolved = Environment::capture().and_then(|environment| ListenConfig::load(&environment));
    match resolved {
        Ok(config) => Check::ok(
            LISTEN,
            format!(
                "{} writing to {}",
                config.transcription.backend_name(),
                config.transcripts_dir.display()
            ),
        ),
        Err(error) => Check::warning(
            LISTEN,
            format!(
                "{}; set listen.backend to parakeet, moonshine, or nemotron to capture meetings",
                error_chain(&error)
            ),
        ),
    }
}

/// The relay is optional, so nothing here fails.
fn relay(config: Option<RelayConfig>) -> Check {
    let Some(config) = config else {
        return Check::warning(PHONE, UNCONFIGURED);
    };
    let Some(port) = config.port() else {
        return Check::warning(
            PHONE,
            "disabled; run `aria phone enable` to take recordings from an iPhone or Apple Watch"
                .to_owned(),
        );
    };
    // Asked, not assumed: a lost port or unusable pairing file only logs, and
    // the daemon serves on happily without a relay. Reporting the
    // configuration would call that healthy.
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

fn daemon(
    paths: Option<&PreparedRuntimePaths>,
    wake: Option<&WakeConfig>,
    hotkey: Option<HotkeyConfig>,
) -> Check {
    match paths {
        Some(paths) => daemon_at(paths.socket_path(), wake, hotkey),
        None => Check::warning(DAEMON, "not checked: the runtime directory failed"),
    }
}

fn daemon_at(socket: &Path, wake: Option<&WakeConfig>, hotkey: Option<HotkeyConfig>) -> Check {
    match probe_daemon(socket) {
        Ok(Liveness::Ready(mut client)) => running_daemon(socket, &mut client, wake, hotkey),
        Ok(Liveness::Starting(_)) => {
            Check::warning(DAEMON, format!("starting at {}", socket.display()))
        }
        Ok(Liveness::NotRunning(_)) => not_running(socket, wake, hotkey),
        Err(error) => Check::failed(DAEMON, error_chain(&error)),
    }
}

fn running_daemon(
    socket: &Path,
    client: &mut DaemonClient,
    wake: Option<&WakeConfig>,
    hotkey: Option<HotkeyConfig>,
) -> Check {
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
    if hotkey_is_enabled(hotkey) && client.check_service_ready(HOTKEY_HEALTH_SERVICE).is_err() {
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
        format!(
            "running at {}{}",
            socket.display(),
            serving_suffix(wake, hotkey)
        ),
    )
}

fn serving_suffix(wake: Option<&WakeConfig>, hotkey: Option<HotkeyConfig>) -> &'static str {
    match (wake_is_enabled(wake), hotkey_is_enabled(hotkey)) {
        (true, true) => "; wake-word monitoring and the dictation hotkey are serving",
        (true, false) => "; wake-word monitoring is serving",
        (false, true) => "; the dictation hotkey is armed",
        (false, false) => "",
    }
}

/// Why the dictation hotkey is not armed, and the fix when one exists.
pub fn hotkey_unarmed_reason() -> String {
    PlatformHotkeyListener::unarmed_reason()
}

fn wake_is_enabled(wake: Option<&WakeConfig>) -> bool {
    matches!(wake, Some(WakeConfig::Enabled { .. }))
}

fn hotkey_is_enabled(hotkey: Option<HotkeyConfig>) -> bool {
    matches!(hotkey, Some(HotkeyConfig::Enabled))
}

/// Where the clipboard is served by the process that filled it, a press needs
/// a daemon that is already running. Elsewhere a press starts one itself.
fn not_running(socket: &Path, wake: Option<&WakeConfig>, hotkey: Option<HotkeyConfig>) -> Check {
    if wake_is_enabled(wake) || hotkey_is_enabled(hotkey) {
        return Check::failed(
            DAEMON,
            format!(
                "not running at {}; wake-word monitoring and the dictation hotkey \
                 need the persistent daemon: `aria service install`",
                socket.display()
            ),
        );
    }
    if PlatformTextDelivery::OUTLIVES_PROCESS {
        return Check::warning(DAEMON, "not running; a press starts one on demand");
    }
    Check::failed(
        DAEMON,
        format!(
            "not running at {}; run `aria service install`",
            socket.display()
        ),
    )
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
    fn server_transcription_needs_no_local_model() {
        let check = model(&TranscriptionBackend::Server {
            endpoint: crate::config::ServerEndpoint::parse("https://example.test:9851").unwrap(),
        });
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("https://example.test:9851"));
        assert!(check.detail.contains("no local transcription model"));
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
        let check = model(&TranscriptionBackend::Local(
            crate::config::TranscriptionConfig::Whisper {
                model: PathBuf::from("/nonexistent/model.bin"),
            },
        ));

        assert_eq!(check.status, Status::Failed);
        assert!(check.detail.contains("whisper"));
        assert!(check.detail.contains("/nonexistent/model.bin"));
    }

    #[test]
    fn a_missing_wake_classifier_fails_the_wake_check() {
        for backend in [WakeBackend::OpenWakeWord, WakeBackend::LiveKit] {
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
    fn a_failed_configuration_leaves_its_dependants_unchecked() {
        for check in [
            audio_capture(None),
            transcription_model(None),
            wake_word_model(None),
        ] {
            assert_eq!(check.status, Status::Warning);
            assert_eq!(check.detail, UNCONFIGURED);
        }
    }

    #[test]
    fn a_socket_nobody_listens_on_reports_a_stopped_daemon() {
        let directory = tempfile::TempDir::new().expect("test directory should be created");

        let check = daemon_at(&directory.path().join("daemon.sock"), None, None);

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

        let check = daemon_at(&socket, None, None);

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
